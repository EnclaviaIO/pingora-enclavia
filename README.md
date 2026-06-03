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

Beta. The crate runs in production against the beta deployment as the
attested upstream for `*.enclaves.beta.enclavia.io` traffic, exposed
as the hosted `/proxy/<path>` shape documented at
[docs.enclavia.io](https://docs.enclavia.io). Per-enclave
configuration is driven by a watched directory of JSON files (one
per running enclave), written by whatever controller you front the
proxy with; the public beta uses the `enclavia-backend` launcher.

## Install

Three supported install paths.

### NixOS module

```nix
{
  inputs.pingora-enclavia.url = "github:EnclaviaIO/pingora-enclavia";

  outputs = { self, nixpkgs, pingora-enclavia, ... }: {
    nixosConfigurations.myhost = nixpkgs.lib.nixosSystem {
      modules = [
        pingora-enclavia.nixosModules.default
        ({ ... }: {
          services.pingora-enclavia.enable = true;
        })
      ];
    };
  };
}
```

Options live in `nix/module.nix`. The module installs the binary as a
hardened systemd unit and creates a tmpfiles-managed targets directory
(`/var/lib/pingora-enclavia/targets` by default). Front it with nginx
or another proxy that terminates TLS and handles the WebSocket upgrade;
see [docs.enclavia.io](https://docs.enclavia.io) for a worked example.

### Docker

```bash
docker run \
  -v $(pwd)/targets:/etc/pingora-enclavia/targets \
  -p 6188:6188 \
  enclaviaio/pingora-enclavia:0.1.0
```

The image reads target JSON files from `/etc/pingora-enclavia/targets`
and listens on `0.0.0.0:6188` by default.

### From source

```bash
cargo build --release --bin pingora-enclavia
./target/release/pingora-enclavia --help
```

## Licensing

Dual licensed under [Apache-2.0](./LICENSE-APACHE) or
[MIT](./LICENSE-MIT) at your option.

### Beta surface

- Dynamic per-enclave targets via filesystem watch (`notify`).
- Host-header → enclave UUID dispatch (leftmost label of `Host`).
- Structured JSON logging via `tracing`, one info line per request,
  error lines with a `failure_kind` field.
- Configurable timeouts: `--tunnel-timeout-secs` (default 10s) for the
  full WSS+Noise+attestation dance, `--request-timeout-secs` (default
  30s) for the per-request upstream read/write window.
- `X-Enclavia-Tunnel-Error: <kind>` header on failures
  (`config_not_found`, `bad_config`, `tunnel_dial`).
- `GET /healthz` returns 200 when the config dir is readable.
- Graceful shutdown via SIGTERM (Pingora native, default 5s drain).
- Underlying tunnel is the upstream `enclavia` SDK's
  `Client::open_stream` primitive (no parallel implementation).

### CLI

```
pingora-enclavia \
  --config-dir /run/enclavia/proxy-targets \
  --listen 127.0.0.1:6188 \
  --tunnel-timeout-secs 10 \
  --request-timeout-secs 30
```

All flags also accept env var equivalents (`PROXY_TARGETS_DIR`,
`LISTEN`, `TUNNEL_TIMEOUT_SECS`, `REQUEST_TIMEOUT_SECS`).

### Config file shape

```json
{
  "enclave_id": "<uuid>",
  "endpoint": "wss://<uuid>.enclaves.beta.enclavia.io",
  "pcrs": { "pcr0": "<hex>", "pcr1": "<hex>", "pcr2": "<hex>" },
  "debug_mode": true
}
```

Filename is `<uuid>.json`; create/modify/delete events update the
in-memory registry without restart.

### Not in beta

Out of scope for the first cut, called out so reviewers don't expect
them:

- Connection / tunnel pooling. Every request opens a fresh attested
  tunnel.
- HTTP/2 listener. HTTP/1.1 only.
- Real production Nitro attestation. Today the `pcr_mismatch` /
  `attestation_parse` / `noise_handshake` distinctions collapse into a
  single `tunnel_dial` failure kind; pulling them apart needs SDK error
  enrichment.
- Prometheus metrics. Logging only.
