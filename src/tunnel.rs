use std::fmt;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use enclavia_protocol::{
    attestation::{self, Pcrs},
    ClientMessage, NoiseHandshake, NoiseTransport, ServerMessage, StreamHalf,
};
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream, ReadBuf};
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};
use tracing::{debug, info, trace, warn};

type WsStream = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

const NOISE_MAX_PAYLOAD: usize = 65535;
const STREAM_ID: u64 = 1;
const DUPLEX_BUFFER: usize = 64 * 1024;
const APP_CHUNK: usize = 16 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum TunnelError {
    #[error("invalid url: {0}")]
    InvalidUrl(String),
    #[error("websocket error: {0}")]
    WebSocket(#[from] tokio_tungstenite::tungstenite::Error),
    #[error("noise: {0}")]
    Noise(#[from] snow::Error),
    #[error("cbor encode: {0}")]
    CborEncode(String),
    #[error("cbor decode: {0}")]
    CborDecode(String),
    #[error("connection closed before completion")]
    ConnectionClosed,
    #[error("unexpected server message: {0}")]
    Unexpected(&'static str),
    #[error("attestation failed: {0}")]
    Attestation(String),
    #[error("handshake io: {0}")]
    HandshakeIo(String),
}

pub struct TunnelConfig {
    pub url: String,
    pub pcrs: Pcrs,
    pub debug_mode: bool,
    pub extra_headers: Vec<(String, String)>,
}

pub struct AttestedTunnel {
    stream: DuplexStream,
}

impl fmt::Debug for AttestedTunnel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AttestedTunnel").finish_non_exhaustive()
    }
}

impl AttestedTunnel {
    pub async fn connect(cfg: TunnelConfig) -> Result<Self, TunnelError> {
        let _ = rustls::crypto::ring::default_provider().install_default();

        info!(url = %cfg.url, "tunnel: connecting");
        let mut request = cfg
            .url
            .as_str()
            .into_client_request()
            .map_err(|e| TunnelError::InvalidUrl(e.to_string()))?;
        for (name, value) in &cfg.extra_headers {
            let header_name: tokio_tungstenite::tungstenite::http::HeaderName = name
                .parse()
                .map_err(|e: tokio_tungstenite::tungstenite::http::header::InvalidHeaderName| {
                    TunnelError::InvalidUrl(format!("bad header name {name:?}: {e}"))
                })?;
            let header_value = HeaderValue::from_str(value)
                .map_err(|e| TunnelError::InvalidUrl(format!("bad header value {name:?}: {e}")))?;
            request.headers_mut().insert(header_name, header_value);
        }
        let (mut ws, _resp) = connect_async(request).await?;
        info!("tunnel: ws connected");

        let (transport, handshake_hash) = run_handshake(&mut ws).await?;
        info!("tunnel: noise handshake done");

        let transport = Mutex::new(transport);

        send_cbor(
            &mut ws,
            &mut *transport.lock().await,
            &ClientMessage::RequestAttestation,
        )
        .await?;

        let mut accum: Vec<u8> = Vec::new();
        let attestation_data = loop {
            let mut guard = transport.lock().await;
            if let Some((msg, consumed)) = try_decode::<ServerMessage>(&mut guard, &accum)? {
                drop(guard);
                accum.drain(..consumed);
                match msg {
                    ServerMessage::Attestation { data, .. } => break data,
                    other => {
                        warn!(?other, "tunnel: unexpected message before attestation");
                        return Err(TunnelError::Unexpected("waiting for Attestation"));
                    }
                }
            } else {
                drop(guard);
                let frame = recv_binary(&mut ws).await?;
                accum.extend_from_slice(&frame);
            }
        };

        attestation::verify_against(&attestation_data, &handshake_hash, &cfg.pcrs, cfg.debug_mode)
            .map_err(|e| TunnelError::Attestation(e.to_string()))?;
        info!("tunnel: attestation verified");

        {
            let mut guard = transport.lock().await;
            send_cbor(
                &mut ws,
                &mut *guard,
                &ClientMessage::OpenStream {
                    id: STREAM_ID,
                    payload: Vec::new(),
                },
            )
            .await?;
        }
        debug!("tunnel: OpenStream sent");

        let (a, b) = tokio::io::duplex(DUPLEX_BUFFER);
        tokio::spawn(pump_task(ws, transport, accum, b));

        Ok(Self { stream: a })
    }
}

impl AsyncRead for AttestedTunnel {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}

impl AsyncWrite for AttestedTunnel {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

async fn run_handshake(
    ws: &mut WsStream,
) -> Result<(NoiseTransport, Vec<u8>), TunnelError> {
    let mut handshake = NoiseHandshake::initiator()?;

    let mut buf = vec![0u8; NOISE_MAX_PAYLOAD];
    let len = handshake.write_message(&[], &mut buf)?;
    ws.send(Message::Binary(buf[..len].to_vec().into())).await?;
    debug!("tunnel: noise -> e");

    let frame = recv_binary(ws).await?;
    let mut payload = vec![0u8; NOISE_MAX_PAYLOAD];
    handshake.read_message(&frame, &mut payload)?;
    debug!("tunnel: noise <- e, ee");

    let handshake_hash = handshake.get_handshake_hash().to_vec();
    let transport = handshake.into_transport_mode()?;

    Ok((transport, handshake_hash))
}

async fn recv_binary(ws: &mut WsStream) -> Result<Vec<u8>, TunnelError> {
    loop {
        match ws.next().await {
            Some(Ok(Message::Binary(b))) => return Ok(b.into()),
            Some(Ok(Message::Close(_))) | None => return Err(TunnelError::ConnectionClosed),
            Some(Ok(_)) => continue,
            Some(Err(e)) => return Err(TunnelError::WebSocket(e)),
        }
    }
}

fn encrypt_cbor<T: serde::Serialize>(
    transport: &mut NoiseTransport,
    msg: &T,
) -> Result<Vec<u8>, TunnelError> {
    let mut cbor = Vec::new();
    ciborium::ser::into_writer(msg, &mut cbor).map_err(|e| TunnelError::CborEncode(e.to_string()))?;

    let mut write_buf = vec![0u8; NOISE_MAX_PAYLOAD];
    let encrypted_len = transport.write_message(&cbor, &mut write_buf)?;

    let mut out = Vec::with_capacity(4 + encrypted_len);
    out.extend_from_slice(&(encrypted_len as u32).to_be_bytes());
    out.extend_from_slice(&write_buf[..encrypted_len]);
    Ok(out)
}

fn try_decode<T: serde::de::DeserializeOwned>(
    transport: &mut NoiseTransport,
    accum: &[u8],
) -> Result<Option<(T, usize)>, TunnelError> {
    if accum.len() < 4 {
        return Ok(None);
    }
    let encrypted_len = u32::from_be_bytes(accum[..4].try_into().unwrap()) as usize;
    if accum.len() < 4 + encrypted_len {
        return Ok(None);
    }
    let mut read_buf = vec![0u8; NOISE_MAX_PAYLOAD];
    let payload_len = transport.read_message(&accum[4..4 + encrypted_len], &mut read_buf)?;
    let msg: T = ciborium::de::from_reader(&read_buf[..payload_len])
        .map_err(|e| TunnelError::CborDecode(e.to_string()))?;
    Ok(Some((msg, 4 + encrypted_len)))
}

async fn send_cbor<T: serde::Serialize>(
    ws: &mut WsStream,
    transport: &mut NoiseTransport,
    msg: &T,
) -> Result<(), TunnelError> {
    let bytes = encrypt_cbor(transport, msg)?;
    ws.send(Message::Binary(bytes.into())).await?;
    Ok(())
}

async fn pump_task(
    mut ws: WsStream,
    transport: Mutex<NoiseTransport>,
    mut accum: Vec<u8>,
    mut user: DuplexStream,
) {
    loop {
        if let Err(e) =
            drain_inbound(&mut accum, &transport, &mut user).await
        {
            warn!(error = %e, "tunnel: drain failed");
            break;
        }

        let mut app_chunk = vec![0u8; APP_CHUNK];
        tokio::select! {
            ws_frame = ws.next() => {
                match ws_frame {
                    Some(Ok(Message::Binary(b))) => {
                        accum.extend_from_slice(&b);
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        debug!("tunnel: ws closed by peer");
                        break;
                    }
                    Some(Ok(_)) => continue,
                    Some(Err(e)) => {
                        warn!(error = %e, "tunnel: ws error");
                        break;
                    }
                }
            }
            read_res = user.read(&mut app_chunk) => {
                match read_res {
                    Ok(0) => {
                        debug!("tunnel: user side EOF, sending StreamClose");
                        let mut guard = transport.lock().await;
                        let _ = send_cbor(
                            &mut ws,
                            &mut *guard,
                            &ClientMessage::StreamClose {
                                id: STREAM_ID,
                                half: StreamHalf::Both,
                            },
                        ).await;
                        drop(guard);
                        let _ = ws.close(None).await;
                        break;
                    }
                    Ok(n) => {
                        let msg = ClientMessage::StreamData {
                            id: STREAM_ID,
                            payload: app_chunk[..n].to_vec(),
                        };
                        let mut guard = transport.lock().await;
                        if let Err(e) = send_cbor(&mut ws, &mut *guard, &msg).await {
                            warn!(error = %e, "tunnel: send_cbor StreamData failed");
                            break;
                        }
                        trace!(n, "tunnel: sent StreamData");
                    }
                    Err(e) => {
                        warn!(error = %e, "tunnel: user side read err");
                        break;
                    }
                }
            }
        }
    }
}

async fn drain_inbound(
    accum: &mut Vec<u8>,
    transport: &Mutex<NoiseTransport>,
    user: &mut DuplexStream,
) -> Result<(), TunnelError> {
    loop {
        let next = {
            let mut guard = transport.lock().await;
            try_decode::<ServerMessage>(&mut guard, accum)?
        };
        let Some((msg, consumed)) = next else {
            return Ok(());
        };
        accum.drain(..consumed);
        match msg {
            ServerMessage::StreamData { id, payload } => {
                if id != STREAM_ID {
                    warn!(id, "tunnel: StreamData for unknown id");
                    continue;
                }
                if let Err(e) = user.write_all(&payload).await {
                    return Err(TunnelError::HandshakeIo(format!("write to user: {e}")));
                }
            }
            ServerMessage::StreamClose { id } => {
                if id != STREAM_ID {
                    continue;
                }
                debug!("tunnel: server StreamClose");
                let _ = user.shutdown().await;
                return Ok(());
            }
            ServerMessage::Error { id, message } => {
                warn!(id, message, "tunnel: server error");
                let _ = user.shutdown().await;
                return Err(TunnelError::Unexpected("server Error"));
            }
            ServerMessage::Data { .. }
            | ServerMessage::ControlResult { .. }
            | ServerMessage::Attestation { .. } => {
                warn!("tunnel: unexpected message variant on data path");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    #[test]
    fn tunnel_error_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<TunnelError>();
        assert_send_sync::<AttestedTunnel>();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn echo_through_tunnel() {
        let _ = tracing_subscriber::fmt::try_init();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();

        let pcrs = enclavia_protocol::attestation::test_utils::FakeAttestation::with_seed(
            0x42,
            vec![0u8; 32],
        );
        let expected = Pcrs {
            pcr0: pcrs.pcr0.clone(),
            pcr1: pcrs.pcr1.clone(),
            pcr2: pcrs.pcr2.clone(),
        };

        tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            crate::test_responder::run(sock).await;
        });

        let url = format!("ws://{}/", addr);
        let tunnel = AttestedTunnel::connect(TunnelConfig {
            url,
            pcrs: expected,
            debug_mode: true,
            extra_headers: Vec::new(),
        })
        .await
        .expect("connect");

        let (mut r, mut w) = tokio::io::split(tunnel);
        let writer = tokio::spawn(async move {
            w.write_all(b"hello world").await.unwrap();
            w.flush().await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            w
        });
        let mut buf = vec![0u8; 64];
        let n = r.read(&mut buf).await.expect("read");
        assert_eq!(&buf[..n], b"hello world");
        let _w = writer.await.unwrap();
    }
}
