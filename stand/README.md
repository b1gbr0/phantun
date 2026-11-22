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
