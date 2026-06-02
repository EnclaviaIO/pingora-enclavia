# pingora-enclavia

An attested network tunnel to a remote Nitro enclave, built on top of
Cloudflare's [Pingora](https://github.com/cloudflare/pingora) proxy
framework.

## What this is, and what it deliberately is not

This is a **byte tunnel that attests its peer**. It does not parse
HTTP at any attack-exposed layer; Pingora handles every byte of HTTP
parsing on the user side, and the application running inside the
enclave handles HTTP on the other side. Our code in the middle does
exactly four things:

1. Open a WebSocket (over TLS) to the enclave's edge.
2. Run a Noise NN handshake inside that WebSocket.
3. Request an AWS Nitro attestation document and verify the
   reported PCRs against a static, configured allowlist.
4. After verification, expose the encrypted channel as a normal
   `AsyncRead + AsyncWrite` byte stream and hand it to Pingora as
   the upstream transport for the request.

Everything else is Pingora or the application. If you can attack
this proxy in a way that doesn't reduce to either "I broke Pingora"
or "I broke the application", that's a bug worth a report.

## Why Pingora, not a custom nginx module

Two reasons:

1. **No bespoke HTTP parsing.** The earlier nginx-module spike was
   starting to grow into custom HTTP-header rewriting, `101`
   suppression, and status synthesis in C-adjacent territory. Pingora
   is a hardened HTTP proxy used at Cloudflare's edge; it already
   knows how to do all of that correctly.
2. **Sync-state-machine gymnastics gone.** Pingora is async-native.
   We can use straightforward tokio + sync Noise crypto without
   contorting around a foreign event loop.

## Architecture

```
user (HTTP / WS / anything Pingora speaks)
  ↓
Pingora frontend (TLS termination, HTTP parsing, routing, observability)
  ↓ (custom upstream Connector)
this crate: TCP+TLS → WebSocket → Noise NN → Attestation verify
  ↓ (encrypted byte channel)
enclavia router (transport-agnostic byte pump)
  ↓
enclavia-server (in-enclave byte pump, post enclavia#17)
  ↓
workload TCP socket (the user's application code)
```

PCRs surface back to the user as `X-Enclavia-PCR0..2` on every
response. Inside the encrypted channel, the wire is the existing
`ClientMessage::OpenStream { id, payload }` /
`StreamData{id,payload}` / `StreamClose{id, half}` envelope from
`enclavia-protocol`. The in-enclave server treats it as a raw byte
pump (it has no HTTP awareness either; PR enclavia#17 was the work
to make that true).

## Status

Spike. Goal of the first agent run: working tunnel against a local
test responder, then a smoke against a live beta enclave (which we
already verified end-to-end with the `enclavia` Rust SDK).
