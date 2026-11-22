#!/usr/bin/env bash
# Timed session-recovery run for the fake-TCP stand.
#
# Builds the client and the server, creates the isolated topology from
# stand/topology.sh, drives UDP datagrams through the tunnel, kills the old
# fake-TCP flow in the reply direction and verifies that:
#
# * the old session ends by keepalive exhaustion at t0 + K + R x I = 35 s
#   (20/5/3) within a 2 s tolerance, where t0 is the last incoming fake-TCP
#   packet of the old session;
# * a new session with a different wire 4-tuple is established without
#   restarting the client, and at least 10 datagrams sent after it is
#   established are answered;
# * the client PID and the old-flow DROP stay in place for the whole run.
#
# Run limits: 120 s for the run itself, 20 s for initial readiness, 20 s for
# recovery, and a separate 10 s budget for cleanup. A violation exits non-zero
# and the logs are kept.
#
# Usage:
#   stand/run.sh run                 # full timed recovery run
#   stand/run.sh cleanup-check       # cleanup for normal/error/TERM/timeout
#   stand/run.sh cleanup             # external cleanup after a forced kill
#
# Environment:
#   STAND_LOG_ROOT   directory for kept logs (default: $TMPDIR/phantun-stand-logs-UID)
#   STAND_LOG_DIR    exact log directory (default: $STAND_LOG_ROOT/<timestamp>-PID)
#   STAND_RUN_DIR    topology state directory (default: $TMPDIR/phantun-stand-state-UID-PID)
#   STAND_ALLOW_NETWORK=1   allow preflight to fetch dependencies
#   STAND_FIREWALL_BACKEND  pin the firewall backend
#
# Exit codes: 0 all checks passed, 1 a check failed, 2 usage error.

set -euo pipefail

SCRIPT_DIR=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
SELF=$SCRIPT_DIR/run.sh
TOPOLOGY=$SCRIPT_DIR/topology.sh
PREFLIGHT=$SCRIPT_DIR/preflight.sh
PROBE_SRC=$SCRIPT_DIR/udp-probe.rs
ROOT=${STAND_PROJECT_ROOT:-$(CDPATH='' cd -- "$SCRIPT_DIR/.." && pwd)}

# Scenario parameters fixed by the plan.
KEEPALIVE_TIME=20
KEEPALIVE_INTERVAL=5
KEEPALIVE_RETRIES=3
UDP_INTERVAL_MS=100
EXPECTED_CLOSE_US=$(((KEEPALIVE_TIME + KEEPALIVE_RETRIES * KEEPALIVE_INTERVAL) * 1000000))
CLOSE_TOLERANCE_US=2000000
MIN_POST_REPLIES=10

CLIENT_UDP_PORT=40001
ECHO_PORT=40002
CLIENT_TUN_LOCAL=192.168.200.1

READY_TIMEOUT_S=20
RECOVERY_TIMEOUT_S=20
TOTAL_TIMEOUT_S=120
CLEANUP_TIMEOUT_S=10

LOG_ROOT=${STAND_LOG_ROOT:-${TMPDIR:-/tmp}/phantun-stand-logs-$(id -u)}
# TOOLS_DIR is resolved in bin_paths(), which refuses a path this user did not
# create because rustc writes into it while the stand may run as root.

SUDO=()
CHILD_PIDS=()
CLEANED=0
TOPOLOGY_STARTED=0
CLIENT_PID=
BIN_DIR=
RUN_LOG=

usage() {
    sed -n '2,32p' "$0" | sed 's/^# \{0,1\}//' >&2
    exit 2
}

now_us() {
    date +%s%6N
}

# Logs to stdout and, once initialized, to the run log.
log() {
    local line
    line="$(now_us) $*"
    printf '%s\n' "$line"
    if [ -n "$RUN_LOG" ]; then
        printf '%s\n' "$line" >>"$RUN_LOG"
    fi
    return 0
}

die() {
    log "ERROR: $*"
    exit 1
}

load_sudo() {
    SUDO=()
    if [ -n "${STAND_SUDO:-}" ]; then
        # The preflight contract already selected how to escalate.
        read -r -a SUDO <<<"$STAND_SUDO"
    elif [ "${STAND_PRIV:-}" = sudo-n ]; then
        SUDO=(sudo -n)
    fi
}

ns_exec() {
    local ns=$1
    shift
    # sudo resets the environment by default, so the log level has to be
    # passed explicitly; without RUST_LOG the client and server would start at
    # the error level and the readiness and exhaustion greps would never see
    # the info lines they wait for.
    "${SUDO[@]}" ip netns exec "$ns" env RUST_LOG="${RUST_LOG:-info}" "$@"
}

# --- preparation -----------------------------------------------------------

acquire_contract() {
    local contract
    contract=$(cd "$ROOT" && "$PREFLIGHT" --print-env) || die "preflight failed; the stand cannot run"
    # The preflight output is KEY=VALUE with printf %q and is trusted.
    eval "$contract"
    load_sudo
    export STAND_PRIV STAND_SUDO STAND_FIREWALL_BACKEND STAND_TIMEOUT STAND_PROJECT_ROOT STAND_ALLOW_NETWORK
}

bin_paths() {
    local target_dir=${CARGO_TARGET_DIR:-$ROOT/target}
    case "$target_dir" in
        /*) ;;
        *) target_dir=$ROOT/$target_dir ;;
    esac
    BIN_DIR=$target_dir/release
    CLIENT_BIN=$BIN_DIR/client
    SERVER_BIN=$BIN_DIR/server
    resolve_tools_dir
    export BIN_DIR CLIENT_BIN SERVER_BIN TOOLS_DIR
}

# The stand may run as root (preflight picks PRIV_MODE=root for id 0), and
# rustc writes an executable into this directory. In a world-writable $TMPDIR
# another local user could pre-create the path and turn the build into a
# root-owned file write (for example via a symlinked udp-probe). The path stays
# stable so the external `cleanup` mode can still match leftover udp-probe
# processes, but anything this user did not create, or a symlink, is refused;
# a directory we created cannot be swapped afterwards in a sticky $TMPDIR.
# topology.sh guards its state directory the same way.
resolve_tools_dir() {
    if [ -z "${TOOLS_DIR:-}" ]; then
        TOOLS_DIR=${TMPDIR:-/tmp}/phantun-stand-tools-$(id -u)
    fi
    [ -L "$TOOLS_DIR" ] && die "refusing to use symlinked tools directory $TOOLS_DIR"
    if [ -e "$TOOLS_DIR" ]; then
        [ -d "$TOOLS_DIR" ] || die "refusing to use tools path $TOOLS_DIR: not a directory"
        [ -O "$TOOLS_DIR" ] || die "refusing to use tools directory $TOOLS_DIR: not owned by the current user"
        chmod 700 "$TOOLS_DIR" 2>/dev/null || true
    else
        mkdir -p "$TOOLS_DIR"
    fi
    return 0
}

prepare() {
    command -v cargo >/dev/null 2>&1 || die "cargo is not installed"
    command -v rustc >/dev/null 2>&1 || die "rustc is not installed"

    log "building phantun client and server"
    if ! (cd "$ROOT" && "$STAND_TIMEOUT" 600 cargo build --release --locked --bins -p phantun) \
        >>"$RUN_LOG" 2>&1; then
        die "cargo build failed; see $RUN_LOG"
    fi

    bin_paths
    [ -x "$CLIENT_BIN" ] || die "the client binary was not built at $CLIENT_BIN"
    [ -x "$SERVER_BIN" ] || die "the server binary was not built at $SERVER_BIN"

    mkdir -p "$TOOLS_DIR"
    if ! rustc -O -o "$TOOLS_DIR/udp-probe" "$PROBE_SRC" >>"$RUN_LOG" 2>&1; then
        die "unable to build $TOOLS_DIR/udp-probe"
    fi
}

init_logging() {
    LOG_DIR=${STAND_LOG_DIR:-$LOG_ROOT/$(date +%Y%m%d-%H%M%S)-$$}
    mkdir -p "$LOG_DIR"
    RUN_LOG=$LOG_DIR/run.log
    STAND_RUN_DIR=${STAND_RUN_DIR:-${TMPDIR:-/tmp}/phantun-stand-state-$(id -u)-$$}
    export STAND_LOG_DIR=$LOG_DIR STAND_RUN_DIR STAND_SERVER_LOG=$LOG_DIR/server.log
    CLIENT_LOG=$LOG_DIR/client.log
    SERVER_LOG=$LOG_DIR/server.log
    ECHO_LOG=$LOG_DIR/echo.log
    DRIVER_LOG=$LOG_DIR/driver.log
    EVIDENCE=$LOG_DIR/evidence.txt
    # The child processes inherit STAND_LOG_DIR and call this again. Truncate
    # only in the process that created the directory, so the parent's log
    # (build output, scenario start) is not discarded by the child.
    if [ "${STAND_LOG_READY:-}" != 1 ]; then
        : >"$RUN_LOG"
        : >"$CLIENT_LOG"
        : >"$SERVER_LOG"
        : >"$ECHO_LOG"
        : >"$DRIVER_LOG"
        : >"$EVIDENCE"
        STAND_LOG_READY=1
        export STAND_LOG_READY
        log "log directory: $LOG_DIR"
    fi
}

setup_topology() {
    TOPOLOGY_STARTED=1
    log "topology: setup"
    if ! "$TOPOLOGY" setup >>"$RUN_LOG" 2>&1; then
        die "topology setup failed; see $RUN_LOG"
    fi
    if ! "$TOPOLOGY" check >>"$RUN_LOG" 2>&1; then
        die "topology isolation check failed; see $RUN_LOG"
    fi
}

load_topology_env() {
    local env_file=$STAND_RUN_DIR/env
    [ -f "$env_file" ] || die "topology state file $env_file is missing"
    # shellcheck disable=SC1090
    . "$env_file"
    load_sudo
}

# --- processes -------------------------------------------------------------

start_echo() {
    ns_exec "$STAND_SERVER_NS" "$TOOLS_DIR/udp-probe" echo "127.0.0.1:$ECHO_PORT" \
        >>"$ECHO_LOG" 2>&1 &
    CHILD_PIDS+=($!)
}

start_server() {
    ns_exec "$STAND_SERVER_NS" "$SERVER_BIN" \
        --local "$STAND_SERVER_PORT" \
        --remote "127.0.0.1:$ECHO_PORT" \
        --ipv4-only >>"$SERVER_LOG" 2>&1 &
    CHILD_PIDS+=($!)
}

start_client() {
    ns_exec "$STAND_CLIENT_NS" "$CLIENT_BIN" \
        --local "127.0.0.1:$CLIENT_UDP_PORT" \
        --remote "$STAND_SERVER_ADDR:$STAND_SERVER_PORT" \
        --tun-local "$CLIENT_TUN_LOCAL" \
        --tun-peer "$STAND_CLIENT_TUN_PEER" \
        --ipv4-only \
        --keepalive-time "$KEEPALIVE_TIME" \
        --keepalive-interval "$KEEPALIVE_INTERVAL" \
        --keepalive-retries "$KEEPALIVE_RETRIES" >>"$CLIENT_LOG" 2>&1 &
    CHILD_PIDS+=($!)
}

start_driver() {
    ns_exec "$STAND_CLIENT_NS" "$TOOLS_DIR/udp-probe" drive \
        "127.0.0.1:$CLIENT_UDP_PORT" "$UDP_INTERVAL_MS" >>"$DRIVER_LOG" 2>&1 &
    CHILD_PIDS+=($!)
}

kill_stand_processes() {
    local signal=$1 exe
    [ -n "${CLIENT_BIN:-}" ] || return 0
    for exe in "$CLIENT_BIN" "$SERVER_BIN" "$TOOLS_DIR/udp-probe"; do
        [ -n "$exe" ] || continue
        "${SUDO[@]}" pkill "-$signal" -f -- "^$exe( |$)" 2>/dev/null || true
    done
}

stop_stand_processes() {
    kill_stand_processes TERM
    local tries=0
    while [ "$tries" -lt 20 ]; do
        processes_present || break
        sleep 0.05
        tries=$((tries + 1))
    done
    kill_stand_processes KILL
}

cleanup() {
    if [ "$CLEANED" -eq 1 ]; then
        return 0
    fi
    CLEANED=1
    local start end pid
    start=$(now_us)
    log "cleanup: start"
    # The background jobs are wrapper subshells; the binaries themselves run
    # as their children inside `ip netns exec`, so kill by executable path.
    for pid in ${CHILD_PIDS[@]+"${CHILD_PIDS[@]}"}; do
        kill "$pid" 2>/dev/null || true
    done
    stop_stand_processes
    if [ "$TOPOLOGY_STARTED" -eq 1 ]; then
        "$STAND_TIMEOUT" "$CLEANUP_TIMEOUT_S" "$TOPOLOGY" teardown >>"$RUN_LOG" 2>&1 \
            || log "cleanup: topology teardown reported failure"
        TOPOLOGY_STARTED=0
    fi
    end=$(now_us)
    log "cleanup: done in $(((end - start) / 1000)) ms"
}

on_exit() {
    local rc=$?
    cleanup
    exit "$rc"
}

install_traps() {
    trap on_exit EXIT
    trap 'exit 143' TERM
    trap 'exit 130' INT
}

# --- observation helpers ---------------------------------------------------

wait_for_log() {
    local file=$1 pattern=$2 deadline=$3
    while :; do
        if [ -f "$file" ] && grep -qE -- "$pattern" "$file"; then
            return 0
        fi
        if [ "$(now_us)" -ge "$deadline" ]; then
            return 1
        fi
        sleep 0.1
    done
}

wait_for_first_reply() {
    local deadline=$1
    while :; do
        if grep -q '^recv ' "$DRIVER_LOG"; then
            return 0
        fi
        if [ "$(now_us)" -ge "$deadline" ]; then
            return 1
        fi
        sleep 0.1
    done
}

last_recv_us() {
    awk '$1 == "recv" { last = $2 } END { if (last != "") print last }' "$DRIVER_LOG"
}

count_post_replies() {
    awk -v since="$1" '
        $1 == "send" && ($2 + 0) >= (since + 0) { sent[$3] = 1 }
        $1 == "recv" && ($3 in sent) { got[$3] = 1 }
        END { print length(got) + 0 }
    ' "$DRIVER_LOG"
}

client_pids() {
    pgrep -f -- "^$CLIENT_BIN( |$)" 2>/dev/null || true
}

count_lines() {
    grep -c . <<<"$1" || true
}

read_drop_stats() {
    local stats
    stats=$("$TOPOLOGY" drop-stats 2>>"$RUN_LOG") || die "the old-flow DROP rules are gone"
    REPLY_DROPPED=$(awk -F= '/server_to_client_packets/ { print $2 }' <<<"$stats")
    FORWARD_DROPPED=$(awk -F= '/client_to_server_packets/ { print $2 }' <<<"$stats")
    REPLY_DROPPED=${REPLY_DROPPED:-0}
    FORWARD_DROPPED=${FORWARD_DROPPED:-0}
}

require_client_alive() {
    local pids
    pids=$(client_pids)
    if [ "$pids" != "$CLIENT_PID" ]; then
        die "client PID changed or the client is gone: expected '$CLIENT_PID', found '$pids'"
    fi
}

require_drop_installed() {
    read_drop_stats
    if [ "$REPLY_DROPPED" -lt 1 ]; then
        die "no old-flow server-to-client packet reached the DROP"
    fi
}

# --- timed recovery run ----------------------------------------------------

run_body() {
    install_traps
    init_logging
    load_sudo
    setup_topology
    load_topology_env
    export RUST_LOG=info

    local env_client_ns=$STAND_CLIENT_NS env_server_ns=$STAND_SERVER_NS
    log "session: keepalive ${KEEPALIVE_TIME}/${KEEPALIVE_INTERVAL}/${KEEPALIVE_RETRIES}, datagram every ${UDP_INTERVAL_MS} ms"

    # One readiness budget covers the server, the client and the first reply.
    local deadline
    deadline=$(($(now_us) + READY_TIMEOUT_S * 1000000))
    start_echo
    log "echo: started on 127.0.0.1:$ECHO_PORT in $env_server_ns"
    start_server
    log "server: started on port $STAND_SERVER_PORT in $env_server_ns"
    wait_for_log "$SERVER_LOG" 'Listening on' "$deadline" \
        || die "server did not start listening within ${READY_TIMEOUT_S}s"

    start_client
    local pids count
    while :; do
        pids=$(client_pids)
        count=$(count_lines "$pids")
        if [ "$count" -eq 1 ]; then
            break
        fi
        if [ "$count" -gt 1 ]; then
            die "expected one client process, found '$pids'"
        fi
        if [ "$(now_us)" -ge "$deadline" ]; then
            die "client did not start within ${READY_TIMEOUT_S}s"
        fi
        sleep 0.1
    done
    CLIENT_PID=$pids
    log "client: PID $CLIENT_PID, local UDP 127.0.0.1:$CLIENT_UDP_PORT in $env_client_ns"

    start_driver
    log "driver: sending every ${UDP_INTERVAL_MS} ms"
    wait_for_first_reply "$deadline" \
        || die "no reply within ${READY_TIMEOUT_S}s of the readiness phase start"
    local first_reply
    first_reply=$(last_recv_us)
    log "readiness: first reply at $first_reply"
    require_client_alive

    # The old session is the only one the server has accepted so far.
    local old_tuple
    if ! old_tuple=$("$TOPOLOGY" old-tuple 2>>"$RUN_LOG"); then
        die "unable to identify a single old fake-TCP tuple; see $SERVER_LOG"
    fi
    log "old tuple: $old_tuple"

    # Cut the reply direction of the old flow: the client's probes still reach
    # the server, but neither data replies nor empty ACKs come back.
    local drop_at
    drop_at=$(now_us)
    if ! "$TOPOLOGY" drop --reply-only "$old_tuple" >>"$RUN_LOG" 2>&1; then
        die "unable to install the old-flow DROP"
    fi
    log "drop: old-flow reply direction cut at $drop_at"
    sleep 0.3

    local t0
    t0=$(last_recv_us)
    [ -n "$t0" ] || die "no reply of the old session was recorded"
    if [ "$t0" -gt $((drop_at + 1000000)) ]; then
        die "last old-session packet at $t0 is later than the DROP at $drop_at"
    fi
    local expected_close
    expected_close=$((t0 + EXPECTED_CLOSE_US))
    log "t0 (last old-session packet seen at the local UDP peer): $t0"
    log "expected close: $expected_close (t0 + 35 s)"

    # Wait for the keepalive exhaustion record. Probe at 100 ms so the observed
    # time stays well inside the 2 s tolerance, and re-check the client PID and
    # the DROP every 5 s so both are proven to hold for the whole wait.
    local close_deadline close_observed end_line next_check
    close_deadline=$((expected_close + 15000000))
    next_check=$(($(now_us) + 5000000))
    while :; do
        end_line=$(grep -m1 'closing session' "$CLIENT_LOG" || true)
        if [ -n "$end_line" ]; then
            close_observed=$(now_us)
            break
        fi
        if [ "$(now_us)" -ge "$next_check" ]; then
            require_client_alive
            require_drop_installed
            next_check=$((next_check + 5000000))
        fi
        if [ "$(now_us)" -ge "$close_deadline" ]; then
            die "the old session did not end by its exhaustion deadline; see $CLIENT_LOG"
        fi
        sleep 0.1
    done

    case "$end_line" in
        *"keepalive probes exhausted"*) ;;
        *) die "session ended for another reason: $end_line" ;;
    esac
    local delta=$((close_observed - expected_close))
    if [ "$delta" -lt 0 ]; then
        delta=$((-delta))
    fi
    if [ "$delta" -gt "$CLOSE_TOLERANCE_US" ]; then
        die "close observed at $close_observed deviates from t0 + 35 s by $((delta / 1000)) ms"
    fi
    log "close: observed at $close_observed, ${delta}us from t0 + 35 s"
    require_client_alive
    read_drop_stats
    log "drop counters after close: forward=$FORWARD_DROPPED reply=$REPLY_DROPPED"
    require_drop_installed

    # Recovery: the next local datagram must build a new session with a new
    # tuple; at least 10 datagrams sent after it is established must be
    # answered.
    local new_tuple recovery_deadline post_replies
    recovery_deadline=$((close_observed + RECOVERY_TIMEOUT_S * 1000000))
    while :; do
        if new_tuple=$("$TOPOLOGY" require-new-tuple "$old_tuple" 2>>"$LOG_DIR/recovery.log"); then
            break
        else
            local rc=$?
            if [ "$rc" -ne 4 ]; then
                die "unexpected tuple state while waiting for recovery (exit $rc)"
            fi
        fi
        if [ "$(now_us)" -ge "$recovery_deadline" ]; then
            die "no new session within ${RECOVERY_TIMEOUT_S}s of the old session closing"
        fi
        sleep 0.1
    done
    local new_observed
    new_observed=$(now_us)
    log "new tuple: $new_tuple (observed at $new_observed)"
    require_client_alive

    while :; do
        post_replies=$(count_post_replies "$new_observed")
        if [ "$post_replies" -ge "$MIN_POST_REPLIES" ]; then
            break
        fi
        if [ "$(now_us)" -ge "$recovery_deadline" ]; then
            die "only $post_replies of $MIN_POST_REPLIES post-recovery datagrams were answered"
        fi
        sleep 0.1
    done
    log "recovery: $post_replies datagrams sent after the new session were answered"
    require_client_alive
    read_drop_stats
    log "drop counters at end: forward=$FORWARD_DROPPED reply=$REPLY_DROPPED"
    require_drop_installed

    local end_count
    end_count=$(grep -c 'closing session' "$CLIENT_LOG" || true)
    if [ "$end_count" -ne 1 ]; then
        die "expected exactly one ended session, found $end_count in $CLIENT_LOG"
    fi
    if grep -qE '(^|[[:space:]])ERROR([[:space:]]|$)' "$CLIENT_LOG"; then
        die "the client logged an error; see $CLIENT_LOG"
    fi

    {
        printf 'old_tuple=%s\n' "$old_tuple"
        printf 'new_tuple=%s\n' "$new_tuple"
        printf 't0_us=%s\n' "$t0"
        printf 't0_source=last old-session reply observed at the local UDP peer\n'
        printf 'drop_us=%s\n' "$drop_at"
        printf 'expected_close_us=%s\n' "$expected_close"
        printf 'observed_close_us=%s\n' "$close_observed"
        printf 'close_deviation_us=%s\n' "$delta"
        printf 'session_end=%s\n' "$end_line"
        printf 'client_pid=%s\n' "$CLIENT_PID"
        printf 'server_to_client_dropped=%s\n' "$REPLY_DROPPED"
        printf 'client_to_server_dropped=%s\n' "$FORWARD_DROPPED"
        printf 'post_recovery_answered=%s\n' "$post_replies"
    } >>"$EVIDENCE"

    log "run: OK (evidence in $EVIDENCE)"
}

# --- cleanup behaviour per exit mode ---------------------------------------

cleanup_scenario() {
    local mode=$1
    install_traps
    init_logging
    load_sudo
    setup_topology
    load_topology_env
    export RUST_LOG=info

    local deadline
    deadline=$(($(now_us) + READY_TIMEOUT_S * 1000000))
    start_echo
    start_server
    wait_for_log "$SERVER_LOG" 'Listening on' "$deadline" \
        || die "server did not start listening"
    start_client

    while :; do
        if [ -n "$(client_pids)" ]; then
            break
        fi
        if [ "$(now_us)" -ge "$deadline" ]; then
            die "client did not start"
        fi
        sleep 0.1
    done

    case "$mode" in
        normal)
            log "scenario: normal exit"
            exit 0
            ;;
        error)
            log "scenario: simulated failure"
            exit 3
            ;;
        term)
            log "scenario: TERM to self"
            kill -TERM "$$"
            sleep 5
            exit 99
            ;;
        long)
            log "scenario: waiting for the external timeout"
            sleep 60
            exit 98
            ;;
        *)
            die "unknown cleanup scenario: $mode"
            ;;
    esac
}

ns_list_present() {
    local ns
    for ns in "$@"; do
        if "${SUDO[@]}" ip netns list | awk -v ns="$ns" '$1 == ns { found = 1 } END { exit !found }'; then
            return 0
        fi
    done
    return 1
}

processes_present() {
    local exe
    [ -n "${CLIENT_BIN:-}" ] || return 1
    for exe in "$CLIENT_BIN" "$SERVER_BIN" "$TOOLS_DIR/udp-probe"; do
        [ -n "$exe" ] || continue
        if pgrep -f -- "^$exe( |$)" >/dev/null 2>&1; then
            return 0
        fi
    done
    return 1
}

# Namespace deletion and signal delivery are asynchronous: give the kernel a
# moment before declaring leftovers.
wait_for_clean() {
    local deadline
    deadline=$(($(now_us) + 3000000))
    while ns_list_present "$@" || processes_present; do
        if [ "$(now_us)" -ge "$deadline" ]; then
            return 1
        fi
        sleep 0.1
    done
    return 0
}

report_leftovers() {
    local ns exe
    for ns in "$@"; do
        if "${SUDO[@]}" ip netns list | awk -v ns="$ns" '$1 == ns { found = 1 } END { exit !found }'; then
            log "leftover namespace: $ns"
        fi
    done
    [ -n "${CLIENT_BIN:-}" ] || return 0
    for exe in "$CLIENT_BIN" "$SERVER_BIN" "$TOOLS_DIR/udp-probe"; do
        [ -n "$exe" ] || continue
        if pgrep -f -- "^$exe( |$)" >/dev/null 2>&1; then
            log "leftover process: $exe"
        fi
    done
}

run_cleanup_scenario() {
    local mode=$1
    local dir expect=0 rc
    dir=$LOG_ROOT/$(date +%Y%m%d-%H%M%S)-cleanup-$mode
    export STAND_LOG_DIR=$dir
    export STAND_RUN_DIR=$dir/state
    export STAND_SERVER_LOG=$dir/server.log
    export STAND_CLIENT_NS="pcc-$mode-client"
    export STAND_ROUTER_NS="pcc-$mode-router"
    export STAND_SERVER_NS="pcc-$mode-server"
    # Each scenario gets a fresh log directory, so let init_logging truncate.
    unset STAND_LOG_READY
    init_logging

    log "cleanup-check/$mode: starting"
    case "$mode" in
        normal) expect=0 ;;
        error) expect=3 ;;
        term) expect=143 ;;
        timeout) expect=124 ;;
    esac

    set +e
    if [ "$mode" = timeout ]; then
        "$STAND_TIMEOUT" --signal=TERM --kill-after="$CLEANUP_TIMEOUT_S" 2 \
            "$SELF" __cleanup-scenario long
        rc=$?
    else
        "$SELF" __cleanup-scenario "$mode"
        rc=$?
    fi
    set -e

    local ok=1
    if [ "$rc" -ne "$expect" ]; then
        log "cleanup-check/$mode: FAIL: exit $rc, expected $expect"
        ok=0
    fi
    if ! grep -q 'cleanup: done in' "$RUN_LOG"; then
        log "cleanup-check/$mode: FAIL: the cleanup step did not complete"
        ok=0
    fi
    local cleanup_ms
    cleanup_ms=$(sed -nE 's/.*cleanup: done in ([0-9]+) ms.*/\1/p' "$RUN_LOG" | tail -n 1)
    if [ -z "$cleanup_ms" ] || [ "$cleanup_ms" -gt $((CLEANUP_TIMEOUT_S * 1000)) ]; then
        log "cleanup-check/$mode: FAIL: cleanup took ${cleanup_ms:-unknown} ms"
        ok=0
    fi
    if ! wait_for_clean "$STAND_CLIENT_NS" "$STAND_ROUTER_NS" "$STAND_SERVER_NS"; then
        log "cleanup-check/$mode: FAIL: stand resources left behind"
        report_leftovers "$STAND_CLIENT_NS" "$STAND_ROUTER_NS" "$STAND_SERVER_NS"
        ok=0
        "$STAND_TIMEOUT" "$CLEANUP_TIMEOUT_S" "$TOPOLOGY" teardown >>"$RUN_LOG" 2>&1 || true
    fi

    if [ "$ok" -eq 1 ]; then
        log "cleanup-check/$mode: OK (exit $rc, cleanup ${cleanup_ms} ms, logs in $dir)"
        return 0
    fi
    return 1
}

cleanup_check() {
    local mode failed=0
    for mode in normal error term timeout; do
        if ! run_cleanup_scenario "$mode"; then
            failed=1
        fi
    done
    if [ "$failed" -ne 0 ]; then
        die "cleanup-check failed"
    fi
    log "cleanup-check: OK"
}

# --- entry points ----------------------------------------------------------

check_leftovers() {
    if wait_for_clean "$CLIENT_NS" "$ROUTER_NS" "$SERVER_NS"; then
        return 1
    fi
    report_leftovers "$CLIENT_NS" "$ROUTER_NS" "$SERVER_NS"
    return 0
}

cmd_run() {
    init_logging
    acquire_contract
    prepare
    install_traps
    CLIENT_NS=${STAND_CLIENT_NS:-phantun-client}
    ROUTER_NS=${STAND_ROUTER_NS:-phantun-router}
    SERVER_NS=${STAND_SERVER_NS:-phantun-server}

    local rc
    set +e
    "$STAND_TIMEOUT" --signal=TERM --kill-after="$CLEANUP_TIMEOUT_S" "$TOTAL_TIMEOUT_S" \
        "$SELF" __run-body
    rc=$?
    set -e

    if [ "$rc" -eq 124 ]; then
        log "run: FAIL: exceeded the ${TOTAL_TIMEOUT_S}s limit"
    elif [ "$rc" -ne 0 ]; then
        log "run: FAIL: exit $rc"
    fi

    if check_leftovers; then
        log "run: FAIL: stand resources left behind; tearing down"
        "$STAND_TIMEOUT" "$CLEANUP_TIMEOUT_S" "$TOPOLOGY" teardown >>"$RUN_LOG" 2>&1 || true
        rc=1
    fi

    if [ "$rc" -eq 0 ]; then
        log "run: OK; logs kept in $STAND_LOG_DIR"
    else
        log "run: logs kept in $STAND_LOG_DIR"
    fi
    exit "$rc"
}

cmd_cleanup_check() {
    LOG_DIR=$LOG_ROOT/$(date +%Y%m%d-%H%M%S)-cleanup-check
    mkdir -p "$LOG_DIR"
    RUN_LOG=$LOG_DIR/run.log
    : >"$RUN_LOG"
    log "cleanup-check: log directory: $LOG_DIR"
    acquire_contract
    prepare
    cleanup_check
}

# External cleanup for a run that was killed without running its trap (SIGKILL,
# a crashed container, a one-shot VM about to be destroyed). Callers pass the
# same STAND_RUN_DIR as the killed run when it was overridden.
cmd_cleanup() {
    LOG_DIR=$LOG_ROOT/$(date +%Y%m%d-%H%M%S)-external-cleanup
    mkdir -p "$LOG_DIR"
    RUN_LOG=$LOG_DIR/run.log
    : >"$RUN_LOG"
    log "external cleanup: log directory: $LOG_DIR"
    acquire_contract
    bin_paths
    CLIENT_NS=${STAND_CLIENT_NS:-phantun-client}
    ROUTER_NS=${STAND_ROUTER_NS:-phantun-router}
    SERVER_NS=${STAND_SERVER_NS:-phantun-server}

    log "external cleanup: stopping stand processes"
    stop_stand_processes

    log "external cleanup: removing the topology"
    "$STAND_TIMEOUT" "$CLEANUP_TIMEOUT_S" "$TOPOLOGY" teardown >>"$RUN_LOG" 2>&1 \
        || log "external cleanup: topology teardown reported failure"

    if check_leftovers; then
        log "external cleanup: FAIL: stand resources remain; see $LOG_DIR"
        exit 1
    fi
    log "external cleanup: OK"
}

main() {
    local command=${1:-run}
    case "$command" in
        run) cmd_run ;;
        cleanup-check) cmd_cleanup_check ;;
        cleanup) cmd_cleanup ;;
        __run-body) run_body ;;
        __cleanup-scenario)
            shift
            [ $# -eq 1 ] || usage
            cleanup_scenario "$1"
            ;;
        --help|-h|help) usage ;;
        *) printf 'run: unknown command: %s\n' "$command" >&2; usage ;;
    esac
}

main "$@"
