//! Test responder used by tests/run-spike.sh (checkpoint 2).
//!
//! Stands up a plain TCP listener on $LISTEN (default 127.0.0.1:9001),
//! accepts WS connections, speaks Noise NN + CBOR + StreamData, and
//! when --mode=http (the default) synthesizes an HTTP/1.1 response per
//! complete request received through the tunnel. --mode=echo echoes
//! StreamData bytes verbatim (used by the cargo-test path).

use pingora_enclavia::test_responder::{run_inner, ResponderMode};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let listen = std::env::var("LISTEN").unwrap_or_else(|_| "127.0.0.1:9001".into());
    let mode_str = std::env::var("MODE").unwrap_or_else(|_| "http".into());
    let mode = match mode_str.as_str() {
        "echo" => ResponderMode::Echo,
        _ => ResponderMode::HttpEcho,
    };
    let listener = tokio::net::TcpListener::bind(&listen).await?;
    eprintln!("noise-echo listening on {listen} mode={mode_str}");
    loop {
        let (sock, _) = listener.accept().await?;
        tokio::spawn(async move {
            if let Err(e) = run_inner(sock, mode).await {
                eprintln!("noise-echo conn err: {e}");
            }
        });
    }
}
