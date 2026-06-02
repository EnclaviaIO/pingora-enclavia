use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use async_trait::async_trait;
use enclavia_protocol::attestation::Pcrs;
use pingora_core::connectors::l4::Connect as L4Connect;
use pingora_core::prelude::*;
use pingora_core::protocols::l4::socket::SocketAddr;
use pingora_core::protocols::l4::stream::Stream as PStream;
use pingora_core::protocols::l4::virt::{VirtualSockOpt, VirtualSocket, VirtualSocketStream};
use pingora_core::upstreams::peer::HttpPeer;
use pingora_proxy::{ProxyHttp, Session};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tracing::error;

use pingora_enclavia::config::Config;
use pingora_enclavia::tunnel::{AttestedTunnel, TunnelConfig};

#[derive(Debug)]
struct EnclaviaConnect {
    upstream_url: String,
    pcrs: Pcrs,
    debug_mode: bool,
    extra_headers: Vec<(String, String)>,
}

#[async_trait]
impl L4Connect for EnclaviaConnect {
    async fn connect(&self, _addr: &SocketAddr) -> pingora_core::Result<PStream> {
        let tunnel = AttestedTunnel::connect(TunnelConfig {
            url: self.upstream_url.clone(),
            pcrs: self.pcrs.clone(),
            debug_mode: self.debug_mode,
            extra_headers: self.extra_headers.clone(),
        })
        .await
        .map_err(|e| {
            pingora_core::Error::because(
                pingora_core::ErrorType::ConnectError,
                "attested tunnel handshake",
                e,
            )
        })?;
        let virt = TunnelSocket { inner: tunnel };
        Ok(VirtualSocketStream::new(Box::new(virt)).into())
    }
}

#[derive(Debug)]
struct TunnelSocket {
    inner: AttestedTunnel,
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

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

impl VirtualSocket for TunnelSocket {
    fn set_socket_option(&self, _opt: VirtualSockOpt) -> std::io::Result<()> {
        Ok(())
    }
}

struct EnclaviaProxy {
    cfg: Arc<Config>,
    pcrs: Pcrs,
    pcr_headers: PcrHeaders,
}

struct PcrHeaders {
    pcr0_hex: String,
    pcr1_hex: String,
    pcr2_hex: String,
}

#[async_trait]
impl ProxyHttp for EnclaviaProxy {
    type CTX = ();
    fn new_ctx(&self) -> Self::CTX {}

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> pingora_core::Result<Box<HttpPeer>> {
        let mut peer = HttpPeer::new(
            // The address is a placeholder: our custom_l4 ignores it.
            // 127.0.0.1:1 is unreachable so a bug that skips custom_l4
            // surfaces immediately as a connect failure.
            "127.0.0.1:1",
            false,
            String::new(),
        );

        let mut extra_headers = Vec::new();
        if let Some(host) = &self.cfg.enclave_host {
            extra_headers.push(("X-Enclave-Host".to_string(), host.clone()));
        }
        let connector = EnclaviaConnect {
            upstream_url: self.cfg.upstream_url.clone(),
            pcrs: self.pcrs.clone(),
            debug_mode: self.cfg.debug_mode,
            extra_headers,
        };
        peer.options.custom_l4 = Some(Arc::new(connector));
        peer.options.connection_timeout = Some(std::time::Duration::from_secs(15));
        peer.options.total_connection_timeout = Some(std::time::Duration::from_secs(15));
        Ok(Box::new(peer))
    }

    async fn response_filter(
        &self,
        _session: &mut Session,
        upstream_response: &mut pingora_http::ResponseHeader,
        _ctx: &mut Self::CTX,
    ) -> pingora_core::Result<()>
    where
        Self::CTX: Send + Sync,
    {
        let _ = upstream_response.insert_header("X-Enclavia-PCR0", &self.pcr_headers.pcr0_hex);
        let _ = upstream_response.insert_header("X-Enclavia-PCR1", &self.pcr_headers.pcr1_hex);
        let _ = upstream_response.insert_header("X-Enclavia-PCR2", &self.pcr_headers.pcr2_hex);
        Ok(())
    }
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cfg_path = std::env::args().nth(1).unwrap_or_else(|| "config.toml".into());
    let cfg = Config::load(&cfg_path)?;
    let pcrs = cfg.pcrs()?;
    let cfg = Arc::new(cfg);
    let pcr_headers = PcrHeaders {
        pcr0_hex: hex::encode(&pcrs.pcr0),
        pcr1_hex: hex::encode(&pcrs.pcr1),
        pcr2_hex: hex::encode(&pcrs.pcr2),
    };

    let mut server = Server::new(None).map_err(|e| {
        error!("server new: {e}");
        anyhow::anyhow!("server new failed")
    })?;
    server.bootstrap();

    let proxy = EnclaviaProxy {
        cfg: cfg.clone(),
        pcrs,
        pcr_headers,
    };

    let mut svc = pingora_proxy::http_proxy_service(&server.configuration, proxy);
    svc.add_tcp(&cfg.listen);
    server.add_service(svc);

    server.run_forever();
}
