//! In-process Noise+CBOR responder used by the checkpoint-1 echo test
//! and reused by `test/noise-echo` for checkpoint 2's HTTP-inside-stream
//! responder.
//!
//! Speaks the same Noise NN + CBOR + StreamData envelope the real
//! `enclavia-server` does, but it is the simplest possible byte echo:
//! `OpenStream { id: 1, payload: empty }` opens, every subsequent
//! `StreamData { id: 1, payload }` is echoed back as a
//! `StreamData { id: 1, payload }` byte-for-byte.

use enclavia_protocol::{
    attestation::test_utils::FakeAttestation, ClientMessage, NoiseHandshake, ServerMessage,
};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::{accept_async, tungstenite::Message, WebSocketStream};

const NOISE_MAX_PAYLOAD: usize = 65535;
pub const PCR_SEED: u8 = 0x42;

pub async fn run(sock: TcpStream) {
    if let Err(e) = run_inner(sock, ResponderMode::Echo).await {
        eprintln!("test_responder: {e}");
    }
}

pub async fn run_http_echo(sock: TcpStream) {
    if let Err(e) = run_inner(sock, ResponderMode::HttpEcho).await {
        eprintln!("test_responder(http): {e}");
    }
}

enum ResponderMode {
    Echo,
    HttpEcho,
}

async fn run_inner(sock: TcpStream, mode: ResponderMode) -> anyhow::Result<()> {
    let mut ws = accept_async(sock).await?;

    let mut handshake = NoiseHandshake::responder()?;
    let first = recv_binary(&mut ws).await?;
    let mut buf = vec![0u8; NOISE_MAX_PAYLOAD];
    handshake.read_message(&first, &mut buf)?;
    let len = handshake.write_message(&[], &mut buf)?;
    ws.send(Message::Binary(buf[..len].to_vec().into())).await?;
    let handshake_hash = handshake.get_handshake_hash().to_vec();
    let mut transport = handshake.into_transport_mode()?;

    let mut accum: Vec<u8> = Vec::new();
    let mut open_id: Option<u64> = None;
    let mut http_accum: Vec<u8> = Vec::new();

    loop {
        loop {
            if accum.len() < 4 {
                break;
            }
            let encrypted_len = u32::from_be_bytes(accum[..4].try_into().unwrap()) as usize;
            if accum.len() < 4 + encrypted_len {
                break;
            }
            let mut plain = vec![0u8; NOISE_MAX_PAYLOAD];
            let n = transport.read_message(&accum[4..4 + encrypted_len], &mut plain)?;
            accum.drain(..4 + encrypted_len);
            let msg: ClientMessage = ciborium::from_reader(&plain[..n])?;

            match msg {
                ClientMessage::RequestAttestation => {
                    let fake = FakeAttestation::with_seed(PCR_SEED, handshake_hash.clone());
                    let doc = fake.encode();
                    send(
                        &mut ws,
                        &mut transport,
                        &ServerMessage::Attestation {
                            data: doc,
                            control_nonce: [0u8; 32],
                        },
                    )
                    .await?;
                }
                ClientMessage::OpenStream { id, .. } => {
                    open_id = Some(id);
                }
                ClientMessage::StreamData { id, payload } => {
                    if Some(id) != open_id {
                        continue;
                    }
                    match mode {
                        ResponderMode::Echo => {
                            send(
                                &mut ws,
                                &mut transport,
                                &ServerMessage::StreamData { id, payload },
                            )
                            .await?;
                        }
                        ResponderMode::HttpEcho => {
                            http_accum.extend_from_slice(&payload);
                            if let Some(resp) = try_synthesize_http(&mut http_accum) {
                                send(
                                    &mut ws,
                                    &mut transport,
                                    &ServerMessage::StreamData { id, payload: resp },
                                )
                                .await?;
                            }
                        }
                    }
                }
                ClientMessage::StreamClose { id, .. } => {
                    send(
                        &mut ws,
                        &mut transport,
                        &ServerMessage::StreamClose { id },
                    )
                    .await?;
                    let _ = ws.close(None).await;
                    return Ok(());
                }
                _ => {}
            }
        }

        match ws.next().await {
            Some(Ok(Message::Binary(b))) => accum.extend_from_slice(&b),
            Some(Ok(Message::Close(_))) | None => return Ok(()),
            Some(Ok(_)) => continue,
            Some(Err(e)) => return Err(e.into()),
        }
    }
}

async fn recv_binary(
    ws: &mut WebSocketStream<TcpStream>,
) -> anyhow::Result<Vec<u8>> {
    loop {
        match ws.next().await {
            Some(Ok(Message::Binary(b))) => return Ok(b.into()),
            Some(Ok(Message::Close(_))) | None => {
                anyhow::bail!("ws closed before handshake")
            }
            Some(Ok(_)) => continue,
            Some(Err(e)) => return Err(e.into()),
        }
    }
}

async fn send(
    ws: &mut WebSocketStream<TcpStream>,
    transport: &mut enclavia_protocol::NoiseTransport,
    msg: &ServerMessage,
) -> anyhow::Result<()> {
    let mut cbor = Vec::new();
    ciborium::into_writer(msg, &mut cbor)?;
    let mut wbuf = vec![0u8; NOISE_MAX_PAYLOAD];
    let n = transport.write_message(&cbor, &mut wbuf)?;
    let mut out = Vec::with_capacity(4 + n);
    out.extend_from_slice(&(n as u32).to_be_bytes());
    out.extend_from_slice(&wbuf[..n]);
    ws.send(Message::Binary(out.into())).await?;
    Ok(())
}

/// Parse a complete HTTP/1.1 request out of an accumulating buffer.
///
/// Looks for the head terminator and, if `Content-Length` is set, also
/// waits for the body bytes. Returns the response bytes once a full
/// request is in hand and removes the consumed input from `accum`.
fn try_synthesize_http(accum: &mut Vec<u8>) -> Option<Vec<u8>> {
    let head_end = find_double_crlf(accum)?;
    let head = &accum[..head_end];
    let head_str = std::str::from_utf8(head).ok()?;
    let mut lines = head_str.split("\r\n");
    let request_line = lines.next()?;
    let mut parts = request_line.split(' ');
    let method = parts.next()?.to_string();

    let mut content_length: usize = 0;
    for line in lines {
        if let Some(rest) = line
            .strip_prefix("Content-Length:")
            .or_else(|| line.strip_prefix("content-length:"))
        {
            content_length = rest.trim().parse().unwrap_or(0);
        }
    }

    if accum.len() < head_end + content_length {
        return None;
    }
    let body = accum[head_end..head_end + content_length].to_vec();
    accum.drain(..head_end + content_length);

    let resp = format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Length: {body_len}\r\n\
         Content-Type: text/plain\r\n\
         Hello-Method: {method}\r\n\
         Connection: close\r\n\
         \r\n",
        body_len = body.len(),
        method = method
    );
    let mut out = resp.into_bytes();
    out.extend_from_slice(&body);
    Some(out)
}

fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| i + 4)
}
