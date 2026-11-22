# Client-side fake-TCP session self-recovery

## Why

**Observation.** In operation, data transfer was reported to stop while the
sent counter kept growing and the handshake of the inner tunnel stayed
fresh. The link between that incident and each defect below is **not yet
confirmed by reproduction** — this is the original report, not proof of
causality.

**Found in the code:**

- with sufficiently frequent traffic notifications, recreating the `sleep`
  can postpone the timeout checks indefinitely
  (`phantun/src/bin/client.rs:349-391`);
- the result of a send into an existing session is ignored, and the
  read lock of the `connections` table is held across the `await`
  (`phantun/src/bin/client.rs:220-222`): the packet is lost and the dead
  socket stays in the table;
- a receive error from the local peer is neither logged nor does it start
  session termination; the branch is disabled until the next `select!` call
  (`phantun/src/bin/client.rs:300-302`);
- the cause of a TUN write failure is lost in both methods — `Socket::send`
  and `Socket::send_keepalive` (`fake-tcp/src/lib.rs:172-177, 190-205`):
  only a bare `Option` travels up.

## Goal

With keepalive enabled, the client detects the absence of incoming
fake-TCP packets and releases the dead session. Given a reachable server,
a healthy TUN and continued local UDP traffic, a new session is established
**without restarting the process**. Checking that the final UDP application
is reachable is out of scope for this iteration.

## Constraints and decisions

- Session behaviour changes in the client; the shared `fake-tcp` library
  changes for diagnostics (preserving the TUN write error cause); `server.rs`
  is not touched.
- `server.rs:211-215` (the suppressed `udp_sock.recv` error and the missing
  diagnostics on `sock.send`) is split into a separate task.
- This iteration verifies recovery of the **client** session while the server
  is healthy. It does not guarantee detection of a UDP → fake-TCP direction
  failure while service ACKs keep flowing.
- The stand uses a server built from this tree: with the modified library,
  but without `server.rs` changes.
- Changing the public `Option` → `Result` API is beyond this iteration.
- Order of work: session and termination contract → timer and its tests →
  send/receive errors and activity accounting → diagnostics → tests → stand →
  final checks.

## Rules for the executor

- **The plan is not edited.** Wording does not change, items are neither
  removed nor added, tasks are neither rewritten nor reordered.
- One change is allowed — marking a completed item as `[x]`.
- If an item is impossible, hits a contradiction, needs a decision outside
  the code, or shows that the plan is wrong — **the executor stops and
  reports it**. That is the plan author's oversight: the plan goes back for
  rework instead of being patched during implementation.
- A workaround that "does it differently but similarly" is forbidden exactly
  like editing the text.

## Acceptance criteria

Runs on Linux; the closing report carries the commands, results and the path
to the stand log. Verified in Task 9:

- the build, clippy with `-D warnings` and the tests of the whole workspace pass;
- the stand from Task 8 completes successfully;
- without `--keepalive-time` no probes are sent, and the CLI values and
  meaning are preserved;
- the error-handling and idle-timer fixes also hold without keepalive;
- useful traffic pushes the idle deadline; without useful traffic the idle
  deadline arrives after `UDP_TTL` = 180 s (`phantun/src/lib.rs:5`), unless
  the session is terminated earlier for another reason; empty ACKs do not
  push the idle deadline.

## Tasks

### Task 1: session contract and a single termination path

- [x] a single idempotent session termination path is introduced, reachable
      from the manager, the main loop and all workers
- [x] the session record carries a generation id and a cancellation handle
- [x] termination forbids further use of the old record, cancels all of its
      workers and interrupts pending sends — including a send from the main
      loop — and removes only the record of **the same generation**
- [x] late termination of an old session does not affect the new one
- [x] regression tests cover repeated termination and late cleanup

### Task 2: a probe timer independent of traffic

- [x] `t0` is the time of the last incoming fake-TCP packet (an empty ACK
      included); outgoing UDP packets do not move that anchor
- [x] probes are scheduled at `t0 + K + n × I`, termination at `t0 + K + R × I`;
      send delays do not accumulate in the schedule
- [x] `n` is an integer from 0 to `R−1`; the initial `t0` is fixed when the
      session is established
- [x] an incoming fake-TCP packet resets the probe budget
- [x] with `20/5/3` the probes fall at 20, 25 and 30 seconds, termination —
      at 35 seconds of virtual time
- [x] the idle timer works independently of keepalive
- [x] a send error or timeout may terminate the session earlier — that is
      intended behaviour, not a schedule failure

### Task 3: send and receive errors, activity accounting

- [x] a `Socket::send` error while sending a payload datagram from the main
      loop or from any worker terminates the corresponding generation through
      the shared mechanism
- [x] cancelling a generation interrupts a pending `Socket::send` in the main
      loop as well as in the workers, and the workers' UDP sends too; after
      cancellation the main loop releases the reference to the old generation
      and keeps handling local UDP traffic; the table lock is not held across
      the `await`
- [x] waiting for keepalive does not block handling incoming activity,
      cancellation, the idle deadline or the probe-exhaustion deadline; the
      keepalive has an explicit send timeout and a termination policy for it
- [x] the `Result` from `udp_sock.recv` is fully handled: on `Err` the cause,
      the local UDP peer address and the session id are written, and then the
      session is terminated through the shared mechanism
- [x] a successful payload datagram through the main path counts as activity
      of the same session — it prevents idle termination but **does not**
      reset the no-incoming-fake-TCP timer
- [x] the main loop and the workers are verified separately with
      never-completing stub sends: generation termination interrupts the
      pending sends and releases the old UDP sockets; the next local datagram
      lets a new generation be established without restarting the process

### Task 4: diagnostics of termination paths

- [x] causes are distinguished at least for: keepalive exhaustion, idle
      timeout, TUN write error, send timeout, UDP receive error, fake-TCP
      receive close
- [x] log records contain the session id and the local UDP peer
- [x] the original TUN write error is kept in the log in both methods
      (`Socket::send`, `Socket::send_keepalive`) before it turns into `None`;
      the public `Option` contract is preserved

### Task 5: automated tests

- [ ] minimal seams for time and I/O results are introduced
- [ ] test: frequent outgoing-traffic notifications do not move the probes
      and the termination
- [ ] test: an incoming fake-TCP packet, an empty ACK included, resets the budget
- [ ] test: the idle timeout works independently of keepalive
- [ ] test: a send error, a UDP receive error and a hung keepalive send
      lead to the intended termination
- [ ] test: a late cleanup does not remove the new session
- [ ] the actual asynchronous manager loop is exercised, not only the
      deadline-computation function; Tokio `test-util` is wired in where
      needed (the workspace currently sets only `features = ["full"]`,
      `Cargo.toml:9-10`)

### Task 6: stand environment contract

- [ ] a Linux runner/VM with pre-authorised root or a non-interactive
      `sudo -n` is fixed
- [ ] the presence of network namespaces, `/dev/net/tun`, `iproute2`, the
      chosen firewall backend and a generic timeout tool is checked
- [ ] Rust tooling and dependencies are prepared, or network access to fetch
      them is allowed
- [ ] when capabilities are missing — a non-zero exit, no interactive
      prompts; skipping the stand does not count as acceptance

### Task 7: isolated stand

- [ ] processes and the veth network live in their own namespaces; forwarding,
      routes and firewall are changed only there
- [ ] the scenario needs no external network — numeric local addresses are
      enough
- [ ] the old fake-TCP 4-tuple and the point where DROP applies relative to
      NAT are recorded; the rule also cuts empty ACKs of the old flow
- [ ] the fact that the fake-TCP port is chosen randomly is taken into
      account (`fake-tcp/src/lib.rs:443-454`): the newness of the tuple is
      checked, and a collision is handled by an explicit policy

### Task 8: verifying stand run

- [ ] parameters: `20/5/3`, UDP packets every 100 ms
- [ ] the last incoming packet of the old session and the moment of DROP are
      recorded
- [ ] termination specifically by keepalive exhaustion is verified, then a new
      session and at least 10 answers to datagrams sent after it was
      established
- [ ] the client PID and the DROP of the old flow are kept for the whole check
- [ ] run limits: total — 120 s, initial readiness — 20 s, recovery — 20 s,
      cleanup — a separate 10 s; the closing time is compared with `t0 + 35 s`
      and a 2 s tolerance
- [ ] cleanup is verified separately for a normal exit, an error, `TERM`
      and a timeout; for a forced process kill an external cleanup or a
      disposable VM is provided
- [ ] the script returns a non-zero code when conditions are violated and
      keeps the logs

### Task 9: acceptance criteria check

- [ ] `cargo build --release --workspace --locked`
- [ ] `cargo clippy --workspace --all-targets --locked -- -D warnings`
- [ ] `cargo test --workspace --locked`
- [ ] the stand command from Task 8 completes successfully
- [ ] without `--keepalive-time` no probes are sent; the CLI values and
      meaning are preserved
- [ ] the error-handling and idle-timer fixes also hold without keepalive
- [ ] useful traffic pushes the idle deadline; without useful traffic the
      idle deadline arrives after `UDP_TTL` = 180 s
      (`phantun/src/lib.rs:5`), unless the session is terminated earlier for
      another reason; empty ACKs do not push the idle deadline

## Technical Details

- Timer: `t0` is the time of the last incoming fake-TCP packet; probes at
  `t0 + K + n × I` (`n = 0…R−1`), termination at `t0 + K + R × I`. With
  `20/5/3` that is 20, 25, 30 and 35 seconds. Outgoing activity does not move
  the timer; an incoming fake-TCP packet, an empty ACK included, resets the
  budget.
- Session: the record carries a generation id and a cancellation handle;
  termination is idempotent, removes only the record of its own generation,
  interrupts pending sends (main loop and workers) and UDP sends.
- Termination causes are distinguished: keepalive exhaustion, idle timeout,
  TUN write error, send timeout, UDP receive error, fake-TCP receive close.
  The log carries the session id and the local UDP peer.
- `Socket::send` and `Socket::send_keepalive` log the original error before
  it turns into `None`; the `Option` contract does not change.

## Post-Completion

- Build the client and the server from this tree and update both ends of the
  customer's setup.
- Live check: a channel under load must recover on its own, without
  `systemctl restart`; the log must show the termination cause and the fact
  that a new session was established.
- As a separate task: the `server.rs:211-215` changes (the suppressed
  `udp_sock.recv` error, diagnostics on `sock.send`) — without them a
  UDP → fake-TCP direction failure is not detected while service ACKs are alive.
