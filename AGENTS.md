# phantun

UDP-to-fake-TCP tunnel: a client and a server exchange UDP payload over a
user-space TCP stack bound to a TUN device. User documentation is in
`README.md`.

## Crates

- `fake-tcp/` — published library: the user-space TCP stack over a TUN device.
- `phantun/` — the client/server binaries and the shared support library.

## Module layout (`phantun/src`)

- `bin/client.rs`, `bin/server.rs` — CLI parsing and process wiring.
- `client.rs` — client main loop, fast-path workers and the per-session
  liveness manager; generic over the injected I/O traits.
- `session.rs` — `Session`/`Connections`: generation ids and the single,
  idempotent session-termination path.
- `keepalive.rs` — probe schedule anchored to the last incoming fake-TCP
  packet.
- `utils.rs` — socket and TUN helpers.

## Testing

- `cargo build --workspace --locked`
- `cargo clippy --workspace --all-targets --locked -- -D warnings`
- `cargo test --workspace --locked`
- Time-dependent client tests use paused Tokio time
  (`#[tokio::test(start_paused = true)]`). `tokio`'s `test-util` feature is a
  dev-dependency of `phantun` only; enabling it in the workspace dependency
  would leak into the published `fake-tcp` crate.
- The client loops are generic over `LocalUdpIngress`, `UdpPeer`,
  `FakeTcpSocket`, `FakeTcpConnector` and `PeerFactory`. Inject fakes in new
  tests instead of opening real sockets or TUN devices.

## Stand

`stand/` is a Linux-only acceptance harness for session recovery
(`preflight.sh`, `topology.sh`, `run.sh`). It needs root or passwordless
`sudo -n`, network namespaces, `/dev/net/tun`, a firewall backend and a
`timeout` tool. A skipped stand is not an acceptance. See `stand/README.md`.

## Plans

Active plans live in `docs/plans/`; finished plans move to
`docs/plans/completed/` (status is the location).
