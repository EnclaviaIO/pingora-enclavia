//! Thin wrapper around `enclavia::Client::open_stream` so the rest of the
//! crate (and Pingora's `custom_l4` connector) gets an `UpgradedStream`
//! without thinking about which SDK call provides it.
//!
//! All the WSS + Noise + attestation + OpenStream machinery lives in the
//! enclavia SDK; pingora-enclavia just decides which enclave to dial.

use enclavia::{Client, Pcrs, UpgradedStream};

#[derive(Debug, thiserror::Error)]
pub enum TunnelError {
    #[error("enclavia sdk: {0}")]
    Sdk(#[from] enclavia::Error),
}

/// What the connector needs to dial a single enclave: the WSS URL, the
/// PCRs to pin against, and the debug-mode toggle.
#[derive(Clone, Debug)]
pub struct TunnelConfig {
    pub url: String,
    pub pcrs: Pcrs,
    pub debug_mode: bool,
}

/// Dial the enclave, verify attestation, and return a raw byte stream the
/// caller (Pingora's custom_l4) can hand to its HTTP/1.1 codec.
pub async fn connect(cfg: TunnelConfig) -> Result<UpgradedStream, TunnelError> {
    // rustls 0.23 needs a process-global CryptoProvider before the first
    // TLS handshake. The SDK installs it on every `build()`, but doing it
    // here too keeps things robust if the connector is reachable through a
    // path that bypasses build (it doesn't today, but the cost is one
    // idempotent function call).
    let _ = rustls::crypto::ring::default_provider().install_default();

    let client = Client::builder(&cfg.url)
        .pcrs(cfg.pcrs)
        .debug_mode(cfg.debug_mode)
        .build()
        .await?;

    let stream = client.open_stream(Vec::new()).await?;
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn tunnel_error_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<TunnelError>();
    }

    /// Same shape as the original `echo_through_tunnel` test, but driven
    /// through the SDK's `open_stream` rather than the hand-rolled
    /// `AttestedTunnel`. The in-process `test_responder` (no-HTTP echo
    /// mode) treats OpenStream as opaque bytes, exactly what `open_stream`
    /// sends now.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn echo_through_tunnel() {
        let _ = tracing_subscriber::fmt::try_init();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();

        let pcrs_seed = enclavia_protocol::attestation::test_utils::FakeAttestation::with_seed(
            0x42,
            vec![0u8; 32],
        );
        let expected = Pcrs {
            pcr0: pcrs_seed.pcr0.clone(),
            pcr1: pcrs_seed.pcr1.clone(),
            pcr2: pcrs_seed.pcr2.clone(),
        };

        tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            crate::test_responder::run(sock).await;
        });

        let url = format!("ws://{addr}/");
        let mut stream = connect(TunnelConfig {
            url,
            pcrs: expected,
            debug_mode: true,
        })
        .await
        .expect("connect");

        stream.write_all(b"hello world").await.unwrap();
        stream.flush().await.unwrap();

        let mut buf = vec![0u8; 64];
        let n = stream.read(&mut buf).await.expect("read");
        assert_eq!(&buf[..n], b"hello world");
    }
}
