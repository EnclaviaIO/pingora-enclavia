//! Per-enclave proxy target registry, fed by a filesystem watch.
//!
//! Each running enclave drops one file at `<dir>/<uuid>.json` (the
//! backend writes it from `services/routes.rs` siblings; see
//! enclavia-crates PR linked in the README). The file shape is locked:
//!
//! ```json
//! {
//!   "enclave_id": "<uuid>",
//!   "endpoint": "wss://<uuid>.enclaves.beta.enclavia.io",
//!   "pcrs": { "pcr0": "<hex>", "pcr1": "<hex>", "pcr2": "<hex>" },
//!   "debug_mode": true
//! }
//! ```
//!
//! The registry watches the directory with `notify`, rebuilds the in-
//! memory entry on every create/modify, and removes it on delete.
//! Lookups by enclave UUID (extracted from the inbound Host header's
//! leftmost label) are O(1) through an `RwLock<HashMap<Uuid, Target>>`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use enclavia::Pcrs;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde::Deserialize;
use tokio::sync::{mpsc, RwLock};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

/// The validated, in-memory shape of a proxy target. PCRs are decoded
/// once at load so the per-request hot path doesn't redo hex.
#[derive(Clone, Debug)]
pub struct ProxyTarget {
    pub enclave_id: Uuid,
    pub endpoint: String,
    pub pcrs: Pcrs,
    pub debug_mode: bool,
}

/// Raw on-disk schema. Kept separate from `ProxyTarget` so the parser
/// can surface a clean `RegistryError::BadConfig` for any malformed
/// hex / missing field.
#[derive(Debug, Deserialize)]
struct ProxyTargetRaw {
    enclave_id: Uuid,
    endpoint: String,
    pcrs: PcrsRaw,
    #[serde(default)]
    debug_mode: bool,
}

#[derive(Debug, Deserialize)]
struct PcrsRaw {
    pcr0: String,
    pcr1: String,
    pcr2: String,
}

/// Failure modes the lookup path needs to distinguish. `ConfigNotFound`
/// becomes a 404; everything else becomes 502 with the matching
/// `X-Enclavia-Tunnel-Error` value.
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("no proxy target registered for enclave {0}")]
    ConfigNotFound(Uuid),
    #[error("bad config for enclave {id}: {reason}")]
    BadConfig { id: Uuid, reason: String },
}

#[derive(Clone)]
pub struct Registry {
    inner: Arc<RwLock<HashMap<Uuid, ProxyTarget>>>,
    dir: PathBuf,
}

impl Registry {
    /// Start the registry: load whatever's already in `dir`, then watch
    /// for changes. The returned handle is cheap to clone and can be
    /// shared across the Pingora request pipeline.
    pub async fn start(dir: PathBuf) -> anyhow::Result<Self> {
        let registry = Registry {
            inner: Arc::new(RwLock::new(HashMap::new())),
            dir: dir.clone(),
        };
        registry.initial_scan().await;
        registry.spawn_watcher()?;
        Ok(registry)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub async fn get(&self, id: Uuid) -> Result<ProxyTarget, RegistryError> {
        self.inner
            .read()
            .await
            .get(&id)
            .cloned()
            .ok_or(RegistryError::ConfigNotFound(id))
    }

    /// Rescan the entire directory. Used on startup and on the catch-all
    /// "something happened we can't trust" branch of the watcher.
    async fn initial_scan(&self) {
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(it) => it,
            Err(e) => {
                warn!(path = %self.dir.display(), error = %e, "proxy-target dir unreadable on scan");
                return;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            self.reload_file(&path).await;
        }
        info!(
            count = self.inner.read().await.len(),
            "initial proxy-target scan complete"
        );
    }

    async fn reload_file(&self, path: &Path) {
        match load_one(path) {
            Ok(target) => {
                let mut guard = self.inner.write().await;
                let id = target.enclave_id;
                guard.insert(id, target);
                debug!(%id, path = %path.display(), "proxy target loaded");
            }
            Err(e) => {
                warn!(path = %path.display(), error = %e, "failed to load proxy target");
            }
        }
    }

    async fn remove_for_path(&self, path: &Path) {
        // The filename is `<uuid>.json`; turn that into the key we
        // stored under. Anything that doesn't parse is fine to ignore
        // because we never inserted it.
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            return;
        };
        let Ok(id) = Uuid::parse_str(stem) else {
            return;
        };
        if self.inner.write().await.remove(&id).is_some() {
            debug!(%id, path = %path.display(), "proxy target removed");
        }
    }

    fn spawn_watcher(&self) -> anyhow::Result<()> {
        let (tx, mut rx) = mpsc::unbounded_channel::<notify::Result<Event>>();
        let mut watcher = RecommendedWatcher::new(
            move |res| {
                let _ = tx.send(res);
            },
            notify::Config::default().with_poll_interval(Duration::from_secs(2)),
        )?;
        watcher.watch(&self.dir, RecursiveMode::NonRecursive)?;

        let me = self.clone();
        tokio::spawn(async move {
            // Hold the watcher inside the task so it lives as long as
            // the registry. Dropping it here would silently stop fs
            // notifications.
            let _w = watcher;
            while let Some(res) = rx.recv().await {
                match res {
                    Ok(ev) => me.handle_event(ev).await,
                    Err(e) => warn!(error = %e, "fs notify error; rescanning"),
                }
            }
            warn!("fs notify channel closed; watcher task exiting");
        });
        Ok(())
    }

    async fn handle_event(&self, ev: Event) {
        match ev.kind {
            EventKind::Create(_) | EventKind::Modify(_) => {
                for path in ev.paths {
                    if path.extension().and_then(|s| s.to_str()) == Some("json") {
                        self.reload_file(&path).await;
                    }
                }
            }
            EventKind::Remove(_) => {
                for path in ev.paths {
                    self.remove_for_path(&path).await;
                }
            }
            _ => {}
        }
    }
}

fn load_one(path: &Path) -> Result<ProxyTarget, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("read: {e}"))?;
    let raw: ProxyTargetRaw = serde_json::from_slice(&bytes).map_err(|e| format!("parse: {e}"))?;
    let pcrs = Pcrs {
        pcr0: hex::decode(&raw.pcrs.pcr0).map_err(|e| format!("pcr0 hex: {e}"))?,
        pcr1: hex::decode(&raw.pcrs.pcr1).map_err(|e| format!("pcr1 hex: {e}"))?,
        pcr2: hex::decode(&raw.pcrs.pcr2).map_err(|e| format!("pcr2 hex: {e}"))?,
    };
    Ok(ProxyTarget {
        enclave_id: raw.enclave_id,
        endpoint: raw.endpoint,
        pcrs,
        debug_mode: raw.debug_mode,
    })
}

/// Parse the leftmost label of a Host header (everything before the
/// first `.`) as a UUID. The wildcard nginx vhost
/// `*.enclaves.beta.enclavia.io` puts the enclave id there, and the
/// `<uuid>.<rest>` shape is what `proxy_endpoint_template` in the
/// backend renders for users.
pub fn enclave_id_from_host(host: &str) -> Option<Uuid> {
    let label = host.split('.').next()?;
    // Strip an explicit port if the Host header carries one — shouldn't
    // happen behind nginx but cheap to be tolerant.
    let label = label.split(':').next()?;
    Uuid::parse_str(label).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_uuid_from_leftmost_label() {
        let id = Uuid::new_v4();
        let host = format!("{id}.enclaves.beta.enclavia.io");
        assert_eq!(enclave_id_from_host(&host), Some(id));
    }

    #[test]
    fn rejects_non_uuid_leftmost_label() {
        assert!(enclave_id_from_host("api.beta.enclavia.io").is_none());
    }

    #[tokio::test]
    async fn loads_existing_files_and_reflects_writes() {
        let dir = tempfile::tempdir().unwrap();
        let id = Uuid::new_v4();
        let path = dir.path().join(format!("{id}.json"));
        let pcr_hex = "ab".repeat(48);
        std::fs::write(
            &path,
            serde_json::json!({
                "enclave_id": id,
                "endpoint": "wss://example/",
                "pcrs": { "pcr0": pcr_hex, "pcr1": pcr_hex, "pcr2": pcr_hex },
                "debug_mode": true,
            })
            .to_string(),
        )
        .unwrap();

        let reg = Registry::start(dir.path().to_path_buf()).await.unwrap();
        let target = reg.get(id).await.expect("present");
        assert_eq!(target.endpoint, "wss://example/");
        assert!(target.debug_mode);
        assert_eq!(target.pcrs.pcr0.len(), 48);
    }

    #[tokio::test]
    async fn missing_target_surfaces_config_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let reg = Registry::start(dir.path().to_path_buf()).await.unwrap();
        let err = reg.get(Uuid::new_v4()).await.unwrap_err();
        assert!(matches!(err, RegistryError::ConfigNotFound(_)));
    }
}
