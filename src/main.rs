use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use async_trait::async_trait;
use clap::Parser;
use enclavia::{Pcrs, UpgradedStream};
use pingora_core::connectors::l4::Connect as L4Connect;
use pingora_core::prelude::*;
use pingora_core::protocols::l4::socket::SocketAddr;
use pingora_core::protocols::l4::stream::Stream as PStream;
use pingora_core::protocols::l4::virt::{VirtualSockOpt, VirtualSocket, VirtualSocketStream};
use pingora_core::upstreams::peer::HttpPeer;
use pingora_proxy::{ProxyHttp, Session};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tracing::{error, info};
use uuid::Uuid;

use pingora_enclavia::registry::{self, ProxyTarget, Registry, RegistryError};
use pingora_enclavia::tunnel::{self, TunnelConfig};

#[derive(Parser, Debug)]
#[command(
    name = "pingora-enclavia",
    about = "Attested Pingora proxy for Nitro enclaves"
)]
struct Cli {
    /// Directory of per-enclave proxy-target JSON files written by the
    /// backend launcher. One file per running enclave, keyed by UUID.
    #[arg(
        long,
        env = "PROXY_TARGETS_DIR",
        default_value = "/run/enclavia/proxy-targets"
    )]
    config_dir: std::path::PathBuf,

    /// Address the HTTP listener binds on. Behind nginx in production
    /// (`127.0.0.1:6188`), public-facing in dev.
    #[arg(long, env = "LISTEN", default_value = "127.0.0.1:6188")]
    listen: String,

    /// Time budget for the full TLS+WSS+Noise+attestation dance against
    /// the upstream enclave. Connections that don't complete inside this
    /// window surface as `tunnel_dial` errors.
    #[arg(long, env = "TUNNEL_TIMEOUT_SECS", default_value_t = 10)]
    tunnel_timeout_secs: u64,

    /// Per-request response timeout (head + body). Applied as the
    /// Pingora upstream read timeout.
    #[arg(long, env = "REQUEST_TIMEOUT_SECS", default_value_t = 30)]
    request_timeout_secs: u64,
}

/// Per-request scratch storage for the proxy. Populated in
/// `request_filter` (or `upstream_peer` if we skipped past
/// `request_filter` for healthz), consumed by `upstream_peer` and the
/// logging hook.
#[derive(Default)]
struct ReqCtx {
    enclave_id: Option<Uuid>,
    target: Option<ProxyTarget>,
    started: Option<std::time::Instant>,
    failure_kind: Option<&'static str>,
}

/// `X-Enclavia-Tunnel-Error` values surfaced to nginx + the client.
const ERR_CONFIG_NOT_FOUND: &str = "config_not_found";
const ERR_BAD_CONFIG: &str = "bad_config";
const ERR_TUNNEL_DIAL: &str = "tunnel_dial";

#[derive(Debug)]
struct EnclaviaConnect {
    upstream_url: String,
    pcrs: Pcrs,
    debug_mode: bool,
    timeout: Duration,
}

#[async_trait]
impl L4Connect for EnclaviaConnect {
    async fn connect(&self, _addr: &SocketAddr) -> pingora_core::Result<PStream> {
        let stream = tokio::time::timeout(
            self.timeout,
            tunnel::connect(TunnelConfig {
                url: self.upstream_url.clone(),
                pcrs: self.pcrs.clone(),
                debug_mode: self.debug_mode,
            }),
        )
        .await
        .map_err(|_| {
            pingora_core::Error::explain(
                pingora_core::ErrorType::ConnectTimedout,
                "attested tunnel handshake timed out",
            )
        })?
        .map_err(|e| {
            pingora_core::Error::because(
                pingora_core::ErrorType::ConnectError,
                "attested tunnel handshake",
                e,
            )
        })?;
        let virt = TunnelSocket { inner: stream };
        Ok(VirtualSocketStream::new(Box::new(virt)).into())
    }
}

#[derive(Debug)]
struct TunnelSocket {
    inner: UpgradedStream,
}

impl AsyncRead for TunnelSocket {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for TunnelSocket {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

impl VirtualSocket for TunnelSocket {
    fn set_socket_option(&self, _opt: VirtualSockOpt) -> std::io::Result<()> {
        Ok(())
    }
}

struct EnclaviaProxy {
    registry: Registry,
    tunnel_timeout: Duration,
    request_timeout: Duration,
}

#[async_trait]
impl ProxyHttp for EnclaviaProxy {
    type CTX = ReqCtx;
    fn new_ctx(&self) -> Self::CTX {
        ReqCtx::default()
    }

    async fn request_filter(
        &self,
        session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> pingora_core::Result<bool> {
        ctx.started = Some(std::time::Instant::now());

        let req = session.req_header();
        let path = req.uri.path();

        // Health endpoint: 200 if the config dir is readable. Used by
        // nginx upstream checks + systemd's ExecStartPost smoke. Returns
        // before the host-header dispatch so it works at every vhost.
        if path == "/healthz" {
            let body = if std::fs::metadata(self.registry.dir()).is_ok() {
                b"ok".to_vec()
            } else {
                b"degraded".to_vec()
            };
            session.respond_error_with_body(200, body.into()).await.ok();
            return Ok(true);
        }

        let host = host_header(session);
        let id = match host.as_deref().and_then(registry::enclave_id_from_host) {
            Some(id) => id,
            None => {
                ctx.failure_kind = Some(ERR_CONFIG_NOT_FOUND);
                respond_with_error(
                    session,
                    404,
                    ERR_CONFIG_NOT_FOUND,
                    "no enclave id in Host header",
                )
                .await;
                return Ok(true);
            }
        };
        ctx.enclave_id = Some(id);

        match self.registry.get(id).await {
            Ok(target) => {
                ctx.target = Some(target);
                Ok(false)
            }
            Err(RegistryError::ConfigNotFound(_)) => {
                ctx.failure_kind = Some(ERR_CONFIG_NOT_FOUND);
                respond_with_error(
                    session,
                    404,
                    ERR_CONFIG_NOT_FOUND,
                    &format!("no proxy target registered for enclave {id}"),
                )
                .await;
                Ok(true)
            }
            Err(RegistryError::BadConfig { reason, .. }) => {
                ctx.failure_kind = Some(ERR_BAD_CONFIG);
                respond_with_error(
                    session,
                    502,
                    ERR_BAD_CONFIG,
                    &format!("proxy target failed to load: {reason}"),
                )
                .await;
                Ok(true)
            }
        }
    }

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> pingora_core::Result<Box<HttpPeer>> {
        // request_filter guarantees this is set: it's the only path that
        // gets past Ok(false).
        let target = ctx.target.as_ref().expect("request_filter set target");

        // The address is a placeholder: our custom_l4 ignores it. 127.0.0.1:1
        // is unreachable so a bug that skips custom_l4 surfaces immediately
        // as a connect failure.
        let mut peer = HttpPeer::new("127.0.0.1:1", false, String::new());
        let connector = EnclaviaConnect {
            upstream_url: target.endpoint.clone(),
            pcrs: target.pcrs.clone(),
            debug_mode: target.debug_mode,
            timeout: self.tunnel_timeout,
        };
        peer.options.custom_l4 = Some(Arc::new(connector));
        peer.options.connection_timeout = Some(self.tunnel_timeout);
        peer.options.total_connection_timeout = Some(self.tunnel_timeout);
        peer.options.read_timeout = Some(self.request_timeout);
        peer.options.write_timeout = Some(self.request_timeout);
        Ok(Box::new(peer))
    }

    async fn response_filter(
        &self,
        _session: &mut Session,
        upstream_response: &mut pingora_http::ResponseHeader,
        ctx: &mut Self::CTX,
    ) -> pingora_core::Result<()>
    where
        Self::CTX: Send + Sync,
    {
        if let Some(target) = ctx.target.as_ref() {
            let _ =
                upstream_response.insert_header("X-Enclavia-PCR0", hex::encode(&target.pcrs.pcr0));
            let _ =
                upstream_response.insert_header("X-Enclavia-PCR1", hex::encode(&target.pcrs.pcr1));
            let _ =
                upstream_response.insert_header("X-Enclavia-PCR2", hex::encode(&target.pcrs.pcr2));
            // Tell the client whether the attestation behind those PCRs was a
            // real AWS-CA-signed Nitro document or a debug/self-signed one. The
            // PCRs alone don't distinguish them (a debug enclave's PCRs are real
            // measurements, just not CA-signed), so without this a debug enclave
            // is indistinguishable from a production one to the client.
            let _ = upstream_response.insert_header(
                "X-Enclavia-Debug-Mode",
                if target.debug_mode { "true" } else { "false" },
            );
        }
        Ok(())
    }

    fn fail_to_connect(
        &self,
        _session: &mut Session,
        _peer: &HttpPeer,
        ctx: &mut Self::CTX,
        e: Box<pingora_core::Error>,
    ) -> Box<pingora_core::Error> {
        // Couldn't reach the enclave: dial timed out, TLS broke, Noise
        // handshake rejected. We can't tell `pcr_mismatch` apart from
        // `noise_handshake` apart from `attestation_parse` here without
        // walking the error chain, and the SDK doesn't currently distinguish
        // those internally either. Beta lumps them under `tunnel_dial`
        // and surfaces the underlying message in the error log line.
        ctx.failure_kind = Some(ERR_TUNNEL_DIAL);
        e
    }

    async fn logging(
        &self,
        session: &mut Session,
        e: Option<&pingora_core::Error>,
        ctx: &mut Self::CTX,
    ) {
        let latency_ms = ctx
            .started
            .map(|t| t.elapsed().as_millis() as u64)
            .unwrap_or(0);
        let req = session.req_header();
        let method = req.method.as_str().to_string();
        let path = req.uri.path().to_string();
        let status = session.response_written().map(|r| r.status.as_u16());

        if let Some(kind) = ctx.failure_kind {
            error!(
                enclave_id = ?ctx.enclave_id.map(|i| i.to_string()),
                %method,
                %path,
                latency_ms,
                failure_kind = kind,
                error = e.map(|e| e.to_string()),
                "tunnel request failed",
            );
        } else {
            info!(
                enclave_id = ?ctx.enclave_id.map(|i| i.to_string()),
                %method,
                %path,
                status = ?status,
                latency_ms,
                "tunnel request",
            );
        }
    }
}

fn host_header(session: &Session) -> Option<String> {
    session
        .req_header()
        .headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

async fn respond_with_error(session: &mut Session, status: u16, kind: &'static str, msg: &str) {
    use pingora_http::ResponseHeader;

    let mut resp = match ResponseHeader::build(status, None) {
        Ok(r) => r,
        Err(_) => return,
    };
    let _ = resp.insert_header("X-Enclavia-Tunnel-Error", kind);
    let _ = resp.insert_header("Content-Type", "text/plain; charset=utf-8");
    let body = format!("{kind}: {msg}\n").into_bytes();
    let _ = resp.insert_header("Content-Length", body.len().to_string());
    if let Err(e) = session.write_response_header(Box::new(resp), false).await {
        error!(?e, "failed to write error response header");
        return;
    }
    if let Err(e) = session.write_response_body(Some(body.into()), true).await {
        error!(?e, "failed to write error response body");
    }
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Default to JSON-formatted structured logging so the deployed
    // service plugs straight into journald → log shipper without an
    // intermediate parser. `RUST_LOG` is still honoured.
    tracing_subscriber::fmt()
        .json()
        .with_current_span(false)
        .with_span_list(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let rt = tokio::runtime::Runtime::new()?;
    let registry = rt.block_on(Registry::start(cli.config_dir.clone()))?;
    info!(
        config_dir = %cli.config_dir.display(),
        listen = %cli.listen,
        "pingora-enclavia starting"
    );

    let mut server = Server::new(None).map_err(|e| {
        error!("server new: {e}");
        anyhow::anyhow!("server new failed")
    })?;
    server.bootstrap();

    let proxy = EnclaviaProxy {
        registry,
        tunnel_timeout: Duration::from_secs(cli.tunnel_timeout_secs),
        request_timeout: Duration::from_secs(cli.request_timeout_secs),
    };

    let mut svc = pingora_proxy::http_proxy_service(&server.configuration, proxy);
    svc.add_tcp(&cli.listen);
    server.add_service(svc);

    // Pingora's run_forever handles SIGTERM as graceful terminate
    // (broadcasts shutdown to every service and waits up to
    // `graceful_shutdown_timeout_seconds`, default 5s; we leave the
    // default in place because per-request work here is cheap).
    server.run_forever();
}
