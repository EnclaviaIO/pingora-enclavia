# Pingora customization for the attested tunnel

## Goal

Hand Pingora a raw byte stream that talks to the enclave through our
attested WSS+Noise+CBOR tunnel, so Pingora can speak HTTP into it as
if it were a plain upstream TCP socket. No HTTP parsing in this
crate.

## Decision

Use **`PeerOptions::custom_l4` with a `pingora_core::connectors::l4::Connect`
implementation** that builds the tunnel, wraps it as a
`VirtualSocketStream`, and returns it as a `Stream`. This is Pingora's
documented extension point for "I want a different L4 transport". It
is exactly what we want.

## Why this works

`pingora-core` exposes two layered abstractions:

1. `PeerOptions::custom_l4: Option<Arc<dyn Connect>>` where

   ```rust
   #[async_trait]
   pub trait Connect: std::fmt::Debug {
       async fn connect(&self, addr: &SocketAddr) -> Result<Stream>;
   }
   ```

   The connector's main `connect()` checks this option before falling
   back to its default TCP/Unix path. If set, our impl runs and its
   returned `Stream` becomes the upstream the rest of Pingora reads
   and writes against.

2. `Stream = Box<dyn IO>` where `IO` is auto-implemented for any
   concrete type that satisfies a list of bounds: `AsyncRead +
   AsyncWrite + Shutdown + UniqueID + Ssl + GetTimingDigest +
   GetProxyDigest + GetSocketDigest + Peek + Unpin + Debug + Send +
   Sync + 'static`. Most of those have trivial blanket-style impls
   on the `VirtualSocketStream` adapter Pingora ships.

   `VirtualSocketStream::new(Box<dyn VirtualSocket>)` is the public
   constructor that takes any `AsyncRead + AsyncWrite + Unpin + Send
   + Sync + Debug` + a `set_socket_option(...)` method, and yields a
   value that satisfies all the `IO` bounds.

So the chain is:

```
AttestedTunnel (AsyncRead+AsyncWrite, our crate)
  -> impl VirtualSocket on a thin wrapper
  -> VirtualSocketStream::new(Box::new(wrapper))
  -> RawStream::Virtual(...)
  -> Stream (Box<dyn IO>)
```

`upstream_peer` returns an `HttpPeer` whose `peer.options.custom_l4 =
Some(Arc::new(EnclaviaConnect { ... }))`. Pingora calls our
`Connect::connect(_, addr)`, we ignore `addr` (the wss URL we want to
dial is in our connector state, not the dummy address Pingora was
asked to proxy to), and return the attested stream.

The `SocketAddr` Pingora hands the connector is the one we declared
on the `HttpPeer`. We still have to pick something valid; the
convention is the loopback address `127.0.0.1:1` — it is never
actually connected to.

## Alternatives considered

- **In-process listener bypass.** Bind a local Unix socket, accept
  connections, run the tunnel on the other side. Pingora dials the
  Unix socket via a normal `HttpPeer`. Functional but ugly: introduces
  a kernel boundary we do not need, and the address arithmetic for
  per-request fan-out is fiddly. Only worth it if `custom_l4` was
  not exposed.
- **Forking pingora.** No reason to: the extension point exists and
  is documented in tests.
- **Sit in front of Pingora as a CONNECT proxy.** Inverts the trust
  boundary in a way that makes Pingora do the wrong layering. Not
  considered seriously.

## Caveats

- `Connect::connect` is per-dial. There is no shared session pool by
  default at this layer; if we want warm-session reuse later, we
  either layer on Pingora's connection-pool keys or maintain our
  own. For the spike: cold-dial per request.
- The `Stream` we return is opaque to Pingora's connection pool. By
  default Pingora may try to reuse it across requests (the
  `keep-alive` story for upstream HTTP/1.1). We surface a fresh
  tunnel per request by returning a peer with a unique `group_key`
  per `(request, dest)` pair so the pool does not reuse our stream
  unintentionally. If we want pooling later we can opt into it
  knowingly; for now the safer default is "do not".
- `VirtualSocket::set_socket_option` is required but irrelevant for
  us — we have nothing to do with TCP-level socket options on a
  noise+cbor channel. The impl returns `Ok(())` unconditionally.

## References (pingora source paths used)

- `pingora-core/src/upstreams/peer.rs` — `Peer` trait, `HttpPeer`,
  `PeerOptions::custom_l4`.
- `pingora-core/src/connectors/l4.rs` — `Connect` trait and the
  branch in `connect()` that defers to `custom_l4`.
- `pingora-core/src/protocols/l4/stream.rs` — `Stream` enum,
  `RawStream::Virtual`.
- `pingora-core/src/protocols/l4/virt.rs` — `VirtualSocket` trait,
  `VirtualSocketStream::new` constructor.
- `pingora-proxy/src/lib.rs` — `ProxyHttp` trait,
  `upstream_peer` / `response_filter` / `connected_to_upstream`.
