# Isolated session-recovery stand

Environment contract for the fake-TCP session-recovery stand described in
`docs/plans/2026-11-22-session-recovery.md` (Tasks 6-9). The stand verifies
that a client detects a dead fake-TCP session by keepalive exhaustion and
establishes a new session without restarting the process.

## Fixed environment

The stand runs on a fixed Linux runner: a Linux VM, or a privileged
container on a Linux host (`--privileged`, host `/dev/net/tun`) with
`iproute2`, a firewall backend and the Rust toolchain installed. It never
runs directly on a development host. Either shape must satisfy:

- root, or passwordless `sudo -n`; no interactive password prompt and no TTY
  question is allowed;
- network namespace support (`ip netns add`/`del`);
- a usable `/dev/net/tun` (creating a TUN device must succeed);
- iproute2 (`ip`);
- one firewall backend: `nft`, `iptables-nft`, `iptables-legacy` or
  `iptables`, able to list rules with the available privileges;
- a general timeout tool: `timeout` (coreutils) or `gtimeout`;
- a Rust toolchain (`cargo`, `rustc`) with the workspace dependencies either
  cached for `cargo fetch --locked --offline` or fetchable over the network.

`stand/preflight.sh` verifies exactly these properties. A missing capability
is reported and the script exits non-zero. There is no "skip" result: a stand
that did not run is not an acceptance, and Tasks 8-9 must fail on it.

## Usage

```sh
# Verify the contract; prints one line per check and exits 0 on success.
stand/preflight.sh

# Allow `cargo fetch` to download missing dependencies from the network.
stand/preflight.sh --allow-network

# Export the contract values for the stand scripts.
eval "$(stand/preflight.sh --print-env)"
```

Exit codes: `0` contract satisfied, `1` capability missing, `2` usage error.
The script never prompts: stdin is closed and every privileged command uses
`sudo -n`.

## Contract variables

`--print-env` writes these shell assignments on stdout (diagnostics go to
stderr), so later stand scripts consume the same selections:

| Variable | Meaning |
| --- | --- |
| `STAND_PRIV` | `root` or `sudo-n` |
| `STAND_SUDO` | empty for root, `sudo -n` otherwise |
| `STAND_FIREWALL_BACKEND` | backend binary chosen for the stand |
| `STAND_TIMEOUT` | `timeout` or `gtimeout` |
| `STAND_PROJECT_ROOT` | workspace root that was checked |
| `STAND_ALLOW_NETWORK` | `1` when network fetching is allowed |

Input overrides:

| Variable | Meaning |
| --- | --- |
| `STAND_PROJECT_ROOT` | workspace root (default: parent of the stand directory) |
| `STAND_ALLOW_NETWORK=1` | same as `--allow-network` |
| `STAND_FIREWALL_BACKEND` | pin one of `nft`, `iptables-nft`, `iptables-legacy`, `iptables` |

Without a pinned backend, preflight picks the first usable one in that order.
A pinned backend that is not usable is an error, not a fallback.

## Scope

The stand owns its processes, veth pair and network namespaces and changes
forwarding, routes and firewall rules only inside them. It needs no external
network: numeric local addresses are enough. Task 7 builds the isolated
topology, Task 8 the timed run, and both must treat a failed preflight as a
failed run.

## Isolated topology

`stand/topology.sh` builds and owns the whole path. Every process of the
stand runs through `ip netns exec`, so the client, the server and anything
they start live in their own namespaces:

```
client namespace            router namespace            server namespace
phantun-client              (forwarding only)           phantun-server
  eth0 172.31.0.2/30  <-->  eth0 172.31.0.1/30
                            eth1 172.31.0.5/30  <-->  eth0 172.31.0.6/30
  TUN local 192.168.200.1     no default route           TUN local 192.168.201.1
  TUN peer  192.168.200.2     no external route          TUN peer  192.168.201.2
```

Names are `phantun-client`, `phantun-router` and `phantun-server`. The veth
pair ends only exist inside the namespaces; the host gets no new link, route,
forwarding setting or firewall rule. Each namespace has exactly one connected
route plus the route to the other endpoint network; none has a default route,
so an external network is not required and not reachable, and the check fails
if one appears.

The two NAT rules mirror a real deployment:

- client namespace, nat postrouting: fake-TCP packets from the client TUN peer
  `192.168.200.2` are SNATed to `172.31.0.2` on `eth0`;
- server namespace, nat prerouting: TCP to `172.31.0.6:4567` is DNATed to the
  server TUN peer `192.168.201.2:4567`.

The client and server need `net.ipv4.ip_forward=1` in their own namespaces:
their fake-TCP stack writes packets into the TUN device, so the packets are
forwarded from the TUN interface to the veth and back. The host setting is
recorded at setup and must not change.

### Old 4-tuple and the DROP point

The old fake-TCP 4-tuple is the **post-NAT wire tuple**
`172.31.0.2:<random client port> -> 172.31.0.6:4567`. It is taken from the
server log: the server writes every accepted session as

```
New connection: (Fake TCP connection from 192.168.201.2:4567 to 172.31.0.2:<client port>)
```

The remote address and port in that line are the client side after the SNAT,
and the local port is the listening port, so the wire tuple follows from it.
The line exists only for an established session, which is what the recovery
check must attribute; the server must therefore run with `RUST_LOG=info` and
its output must be written to `stand/topology.sh server-log`. Both values are
under the sole control of the stand, so no kernel log, conntrack table or
backend-specific observation facility is required.

The DROP is installed in the **router namespace forward chain**, not in either
endpoint: at that point the SNAT and DNAT have already run, so the rule matches
the wire tuple directly, and both directions are dropped:

```
172.31.0.2:<client port> -> 172.31.0.6:4567   drop
172.31.0.6:4567 -> 172.31.0.2:<client port>   drop
```

The rule constrains addresses, ports and TCP protocol only. There is no
payload, flag or `--syn` condition, so the empty keepalive ACKs of the old
flow are cut exactly like its data packets. `drop-stats` reports both
directions; the server-to-client counter is the evidence that the old flow's
empty ACKs no longer reset the client's probe budget. A reply-only phase
(`drop --reply-only`) makes that evidence measurable: keepalive probes still
reach the server, its empty ACKs are dropped, and the reverse counter grows
while the client exhausts its probe budget.

### Random fake-TCP port

`Stack::connect` picks a random local port in `32768..=60999`
(`fake-tcp/src/lib.rs`), so the tuple cannot be predicted in advance. The
stand takes the old tuple from the server log before installing the DROP. A
new session must have a tuple **different** from the old one;
`require-new-tuple` enforces that and exits `4` with a distinct message when
no distinct tuple appears. If the random port reproduces the old tuple, the
new flow is indistinguishable from the dropped one and the run cannot prove
recovery: the explicit policy is to fail the run (exit `4`) and rerun from a
fresh preflight/setup, never to remove or widen the DROP and never to count
it as a success.

### Commands

```sh
eval "$(stand/preflight.sh --print-env)"   # or let setup run preflight itself
stand/topology.sh setup
stand/topology.sh check
stand/topology.sh server-log               # where the server log must be written
stand/topology.sh wire-tuples
stand/topology.sh old-tuple
stand/topology.sh drop 172.31.0.2:34567 172.31.0.6:4567
stand/topology.sh drop --reply-only 172.31.0.2:34567 172.31.0.6:4567
stand/topology.sh drop-stats
stand/topology.sh require-new-tuple 172.31.0.2:34567 172.31.0.6:4567
stand/topology.sh drop-remove
stand/topology.sh ns-exec client -- phantun-client --local 127.0.0.1:40001 ...
stand/topology.sh teardown
```

`setup` runs the preflight contract and refuses to start when it fails; it
also refuses to replace existing namespaces, so a crashed run is torn down
explicitly first. `teardown` removes only the three namespaces and the
run directory. The run directory defaults to `${TMPDIR:-/tmp}/phantun-stand-<uid>`
and can be overridden with `STAND_RUN_DIR`; the server log defaults to
`<run directory>/server.log` and can be overridden with `STAND_SERVER_LOG`
before setup.
