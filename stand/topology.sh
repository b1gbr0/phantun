#!/usr/bin/env bash
# Isolated network topology for the fake-TCP session-recovery stand.
#
# The topology never touches the host network namespace. Three dedicated
# namespaces are created, each connected only by veth pairs:
#
#   client --veth-- router --veth-- server
#
# The client namespace runs the Phantun client and SNATs its fake-TCP packets
# to the client veth address. The server namespace runs the Phantun server and
# DNATs the listening port to the server TUN peer. The router namespace only
# forwards between the two; it has no default route and no external
# connectivity. All forwarding, routing and firewall state lives in these
# namespaces.
#
# The old wire 4-tuple of a session is taken from the server log: the server
# logs every accepted connection as "New connection: (Fake TCP connection
# from <tun peer>:<port> to <client wire address>:<port>)", which is the
# post-SNAT client address and the listening port. The DROP rule is installed
# in the router's forward chain, where the tuple is that post-NAT wire tuple,
# and it matches whole flows by addresses and ports only, so empty ACKs of the
# old flow are cut as well.
#
# Usage:
#   stand/topology.sh setup            # create the topology (requires preflight contract)
#   stand/topology.sh teardown         # remove everything created by setup
#   stand/topology.sh check            # verify isolation invariants
#   stand/topology.sh status           # print the topology state
#   stand/topology.sh server-log       # path of the server log to write
#   stand/topology.sh wire-tuples      # list accepted post-NAT 4-tuples
#   stand/topology.sh old-tuple        # the single pre-DROP tuple
#   stand/topology.sh require-new-tuple 'OLD'   # distinct tuple or exit 4
#   stand/topology.sh drop 'CLIENT' 'SERVER'    # install the old-flow DROP
#   stand/topology.sh drop --reply-only 'CLIENT' 'SERVER'  # diagnostic phase
#   stand/topology.sh drop-stats       # packets dropped per direction
#   stand/topology.sh drop-remove      # remove the DROP rules
#   stand/topology.sh ns-exec client|router|server -- CMD...
#
# Exit codes: 0 ok, 1 failure, 2 usage/state error, 4 no tuple distinct from
# the old one (explicit tuple-collision policy, see stand/README.md).

set -euo pipefail

SCRIPT_DIR=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
RUN_DIR=${STAND_RUN_DIR:-${TMPDIR:-/tmp}/phantun-stand-$(id -u)}
ENV_FILE=$RUN_DIR/env
SERVER_LOG=${STAND_SERVER_LOG:-$RUN_DIR/server.log}

# Fixed numeric topology. No external addresses are used or needed.
CLIENT_NS=${STAND_CLIENT_NS:-phantun-client}
ROUTER_NS=${STAND_ROUTER_NS:-phantun-router}
SERVER_NS=${STAND_SERVER_NS:-phantun-server}

CLIENT_ADDR=172.31.0.2
ROUTER_CLIENT_ADDR=172.31.0.1
ROUTER_SERVER_ADDR=172.31.0.5
SERVER_ADDR=172.31.0.6
CLIENT_NET=172.31.0.0/30
SERVER_NET=172.31.0.4/30

CLIENT_TUN_PEER=192.168.200.2
SERVER_TUN_PEER=192.168.201.2
SERVER_PORT=4567

DROP_TABLE=phantun_stand_drop
DROP_CHAIN=PHANTUN_STAND_DROP
MARKER=phantun-stand
NFT_MARKER=phantun_stand

SUDO=()
IPT=${STAND_FIREWALL_BACKEND:-iptables}

usage() {
    sed -n '2,40p' "$0" | sed 's/^# \{0,1\}//' >&2
    exit 2
}

die() {
    printf 'topology: %s\n' "$*" >&2
    exit 1
}

state_error() {
    printf 'topology: %s\n' "$*" >&2
    exit 2
}

command_name=${1:-}
if [ -z "$command_name" ]; then
    usage
fi
shift

is_nft() { [ "${STAND_FIREWALL_BACKEND:-}" = nft ]; }

load_sudo() {
    SUDO=()
    IPT=${STAND_FIREWALL_BACKEND:-iptables}
    if [ "${STAND_PRIV:-}" = sudo-n ]; then
        SUDO=(sudo -n)
    fi
}

run() {
    "${SUDO[@]}" "$@"
}

nsrun() {
    local ns=$1
    shift
    "${SUDO[@]}" ip netns exec "$ns" "$@"
}

load_env() {
    [ -f "$ENV_FILE" ] || state_error "no topology state at $ENV_FILE; run 'topology.sh setup' first"
    # The state file is written by setup with printf %q and is trusted.
    # shellcheck disable=SC1090
    . "$ENV_FILE"
    SERVER_LOG=${STAND_SERVER_LOG:-$RUN_DIR/server.log}
    load_sudo
}

ns_exists() {
    run ip netns list | awk -v ns="$1" '$1 == ns { found = 1 } END { exit !found }'
}

ns_name_for() {
    case "$1" in
        client) printf '%s\n' "$CLIENT_NS" ;;
        router) printf '%s\n' "$ROUTER_NS" ;;
        server) printf '%s\n' "$SERVER_NS" ;;
        *) state_error "unknown namespace alias: $1 (use client, router or server)" ;;
    esac
}

# --- namespace lifecycle ---------------------------------------------------

destroy_namespaces() {
    local ns
    for ns in "$CLIENT_NS" "$ROUTER_NS" "$SERVER_NS"; do
        run ip netns del "$ns" >/dev/null 2>&1 || true
    done
    if [ -n "${RUN_ID:-}" ]; then
        local link
        for link in "vhc$RUN_ID" "vhrc$RUN_ID" "vhs$RUN_ID" "vhrs$RUN_ID"; do
            run ip link del "$link" >/dev/null 2>&1 || true
        done
    fi
}

setup_failed() {
    destroy_namespaces
    rm -rf "$RUN_DIR"
    printf 'topology: setup failed; created namespaces removed\n' >&2
}

setup_nft_nat() {
    nsrun "$CLIENT_NS" nft add table ip "${MARKER//-/_}_nat"
    nsrun "$CLIENT_NS" nft add chain ip "${MARKER//-/_}_nat" postrouting \
        '{ type nat hook postrouting priority 100; policy accept; }'
    nsrun "$CLIENT_NS" nft add rule ip "${MARKER//-/_}_nat" postrouting \
        ip saddr "$CLIENT_TUN_PEER" oifname eth0 snat to "$CLIENT_ADDR"

    nsrun "$SERVER_NS" nft add table ip "${MARKER//-/_}_nat"
    nsrun "$SERVER_NS" nft add chain ip "${MARKER//-/_}_nat" prerouting \
        '{ type nat hook prerouting priority -100; policy accept; }'
    nsrun "$SERVER_NS" nft add rule ip "${MARKER//-/_}_nat" prerouting \
        iifname eth0 tcp dport "$SERVER_PORT" dnat ip to "$SERVER_TUN_PEER:$SERVER_PORT"
}

setup_iptables_nat() {
    nsrun "$CLIENT_NS" "$IPT" -t nat -A POSTROUTING \
        -s "$CLIENT_TUN_PEER" -o eth0 -j SNAT --to-source "$CLIENT_ADDR"
    nsrun "$SERVER_NS" "$IPT" -t nat -A PREROUTING \
        -i eth0 -p tcp --dport "$SERVER_PORT" \
        -j DNAT --to-destination "$SERVER_TUN_PEER:$SERVER_PORT"
}

cmd_setup() {
    # shellcheck disable=SC2046
    eval "$("$SCRIPT_DIR/preflight.sh" --print-env)"
    load_sudo

    run ip netns list >/dev/null 2>&1 || die "network namespaces are not available"
    local ns
    for ns in "$CLIENT_NS" "$ROUTER_NS" "$SERVER_NS"; do
        if ns_exists "$ns"; then
            state_error "namespace $ns already exists; run 'topology.sh teardown' first"
        fi
    done

    local host_forward_before
    host_forward_before=$(sysctl -n net.ipv4.ip_forward 2>/dev/null || printf 'unknown')
    RUN_ID=$(od -An -N4 -tx4 /dev/urandom | tr -d ' \n')
    RUN_ID=${RUN_ID:0:6}

    trap setup_failed EXIT INT TERM

    run ip netns add "$CLIENT_NS"
    run ip netns add "$ROUTER_NS"
    run ip netns add "$SERVER_NS"

    # Client <-> router veth pair.
    run ip link add "vhc$RUN_ID" type veth peer name "vhrc$RUN_ID"
    run ip link set "vhc$RUN_ID" netns "$CLIENT_NS"
    run ip link set "vhrc$RUN_ID" netns "$ROUTER_NS"
    run ip -n "$CLIENT_NS" link set "vhc$RUN_ID" name eth0
    run ip -n "$ROUTER_NS" link set "vhrc$RUN_ID" name eth0

    # Server <-> router veth pair.
    run ip link add "vhs$RUN_ID" type veth peer name "vhrs$RUN_ID"
    run ip link set "vhs$RUN_ID" netns "$SERVER_NS"
    run ip link set "vhrs$RUN_ID" netns "$ROUTER_NS"
    run ip -n "$SERVER_NS" link set "vhs$RUN_ID" name eth0
    run ip -n "$ROUTER_NS" link set "vhrs$RUN_ID" name eth1

    # Client namespace: one link, no default route.
    run ip -n "$CLIENT_NS" link set lo up
    run ip -n "$CLIENT_NS" addr add "$CLIENT_ADDR/30" dev eth0
    run ip -n "$CLIENT_NS" link set eth0 up
    run ip -n "$CLIENT_NS" route add "$SERVER_NET" via "$ROUTER_CLIENT_ADDR" dev eth0
    nsrun "$CLIENT_NS" sysctl -qw net.ipv4.ip_forward=1

    # Router namespace: forwarding only between the two veths.
    run ip -n "$ROUTER_NS" link set lo up
    run ip -n "$ROUTER_NS" addr add "$ROUTER_CLIENT_ADDR/30" dev eth0
    run ip -n "$ROUTER_NS" addr add "$ROUTER_SERVER_ADDR/30" dev eth1
    run ip -n "$ROUTER_NS" link set eth0 up
    run ip -n "$ROUTER_NS" link set eth1 up
    nsrun "$ROUTER_NS" sysctl -qw net.ipv4.ip_forward=1

    # Server namespace: one link, no default route.
    run ip -n "$SERVER_NS" link set lo up
    run ip -n "$SERVER_NS" addr add "$SERVER_ADDR/30" dev eth0
    run ip -n "$SERVER_NS" link set eth0 up
    run ip -n "$SERVER_NS" route add "$CLIENT_NET" via "$ROUTER_SERVER_ADDR" dev eth0
    nsrun "$SERVER_NS" sysctl -qw net.ipv4.ip_forward=1

    if is_nft; then
        setup_nft_nat
    else
        setup_iptables_nat
    fi

    mkdir -p "$RUN_DIR"
    SERVER_LOG=${STAND_SERVER_LOG:-$RUN_DIR/server.log}
    {
        printf 'STAND_PRIV=%q\n' "${STAND_PRIV:-}"
        printf 'STAND_SUDO=%q\n' "${STAND_SUDO:-}"
        printf 'STAND_FIREWALL_BACKEND=%q\n' "${STAND_FIREWALL_BACKEND:-}"
        printf 'STAND_TIMEOUT=%q\n' "${STAND_TIMEOUT:-}"
        printf 'STAND_PROJECT_ROOT=%q\n' "${STAND_PROJECT_ROOT:-}"
        printf 'STAND_ALLOW_NETWORK=%q\n' "${STAND_ALLOW_NETWORK:-0}"
        printf 'STAND_CLIENT_NS=%q\n' "$CLIENT_NS"
        printf 'STAND_ROUTER_NS=%q\n' "$ROUTER_NS"
        printf 'STAND_SERVER_NS=%q\n' "$SERVER_NS"
        printf 'RUN_ID=%q\n' "$RUN_ID"
        printf 'STAND_CLIENT_ADDR=%q\n' "$CLIENT_ADDR"
        printf 'STAND_SERVER_ADDR=%q\n' "$SERVER_ADDR"
        printf 'STAND_CLIENT_TUN_PEER=%q\n' "$CLIENT_TUN_PEER"
        printf 'STAND_SERVER_TUN_PEER=%q\n' "$SERVER_TUN_PEER"
        printf 'STAND_SERVER_PORT=%q\n' "$SERVER_PORT"
        printf 'STAND_HOST_IP_FORWARD_BEFORE=%q\n' "$host_forward_before"
        printf 'STAND_SERVER_LOG=%q\n' "$SERVER_LOG"
    } >"$ENV_FILE"

    trap - EXIT INT TERM
    printf 'topology: ready (run id %s)\n' "$RUN_ID"
}

cmd_teardown() {
    if [ -f "$ENV_FILE" ]; then
        # shellcheck disable=SC1090
        . "$ENV_FILE"
        load_sudo
    fi
    destroy_namespaces
    rm -rf "$RUN_DIR"
    printf 'topology: removed\n'
}

# --- state checks ----------------------------------------------------------

check_failure() {
    CHECK_FAILURES+=("$*")
    printf 'check: FAIL: %s\n' "$*" >&2
}

cmd_check() {
    load_env
    CHECK_FAILURES=()
    local ns

    for ns in "$CLIENT_NS" "$ROUTER_NS" "$SERVER_NS"; do
        if ! ns_exists "$ns"; then
            check_failure "namespace $ns is missing"
        fi
    done
    if [ "${#CHECK_FAILURES[@]}" -ne 0 ]; then
        exit 1
    fi

    # Every namespace keeps forwarding on; the host value is untouched.
    for ns in "$CLIENT_NS" "$ROUTER_NS" "$SERVER_NS"; do
        if [ "$(nsrun "$ns" sysctl -n net.ipv4.ip_forward)" != 1 ]; then
            check_failure "forwarding is not enabled inside $ns"
        fi
    done
    local host_forward_now
    host_forward_now=$(sysctl -n net.ipv4.ip_forward 2>/dev/null || printf 'unknown')
    if [ "$host_forward_now" != "$STAND_HOST_IP_FORWARD_BEFORE" ]; then
        check_failure "host ip_forward changed from $STAND_HOST_IP_FORWARD_BEFORE to $host_forward_now"
    fi

    # No external network: no default route and no route to a public address.
    for ns in "$CLIENT_NS" "$ROUTER_NS" "$SERVER_NS"; do
        if [ -n "$(nsrun "$ns" ip -4 route show default)" ]; then
            check_failure "namespace $ns has a default route"
        fi
        if nsrun "$ns" ip route get 8.8.8.8 >/dev/null 2>&1; then
            check_failure "namespace $ns can route to an external address"
        fi
    done

    # Topology state did not leak into the host namespace.
    local net
    for net in "$CLIENT_NET" "$SERVER_NET"; do
        if run ip -4 route show | grep -Fq "$net"; then
            check_failure "topology route $net leaked into the host namespace"
        fi
    done
    local link
    for link in "vhc$RUN_ID" "vhrc$RUN_ID" "vhs$RUN_ID" "vhrs$RUN_ID"; do
        if run ip link show "$link" >/dev/null 2>&1; then
            check_failure "topology veth $link leaked into the host namespace"
        fi
    done
    if is_nft; then
        if run nft list ruleset 2>/dev/null | grep -q "$NFT_MARKER"; then
            check_failure "nftables state leaked into the host namespace"
        fi
    else
        if run "${IPT}-save" 2>/dev/null | grep -q "$MARKER"; then
            check_failure "iptables state leaked into the host namespace"
        fi
    fi

    # NAT rules exist only in their namespaces.
    if is_nft; then
        nsrun "$CLIENT_NS" nft list table ip "${MARKER//-/_}_nat" >/dev/null 2>&1 \
            || check_failure "client SNAT table is missing"
        nsrun "$SERVER_NS" nft list table ip "${MARKER//-/_}_nat" >/dev/null 2>&1 \
            || check_failure "server DNAT table is missing"
    else
        nsrun "$CLIENT_NS" "$IPT" -t nat -S POSTROUTING | grep -q -- '--to-source' \
            || check_failure "client SNAT rule is missing"
        nsrun "$SERVER_NS" "$IPT" -t nat -S PREROUTING | grep -q -- '--to-destination' \
            || check_failure "server DNAT rule is missing"
    fi
    [ -n "${SERVER_LOG:-}" ] || check_failure "server log path is not configured"

    if [ "${#CHECK_FAILURES[@]}" -ne 0 ]; then
        printf 'check: %d problem(s)\n' "${#CHECK_FAILURES[@]}" >&2
        exit 1
    fi
    printf 'check: OK\n'
}

cmd_status() {
    load_env
    local ns
    printf 'run id: %s\n' "$RUN_ID"
    printf 'backend: %s\n' "$STAND_FIREWALL_BACKEND"
    printf 'server: %s:%s (tun peer %s)\n' "$SERVER_ADDR" "$SERVER_PORT" "$SERVER_TUN_PEER"
    for ns in "$CLIENT_NS" "$ROUTER_NS" "$SERVER_NS"; do
        if ns_exists "$ns"; then
            printf 'namespace %s: present\n' "$ns"
            nsrun "$ns" ip -4 route show | sed 's/^/  /'
        else
            printf 'namespace %s: MISSING\n' "$ns"
        fi
    done
    printf 'observed tuples:\n'
    observe_tuples | sed 's/^/  /'
}

# --- tuple observation -----------------------------------------------------
#
# The server logs every accepted session. Its line carries the tuple as seen
# on the wire after the client SNAT: the remote address is 172.31.0.2, the
# remote port is the client's random fake-TCP port, and the local port is the
# listening port. Only established sessions appear, which is exactly what the
# recovery check needs to attribute.
observe_tuples() {
    [ -f "$SERVER_LOG" ] || return 0
    sed -nE 's/.*Fake TCP connection from [0-9.]+:([0-9]+) to ([0-9.]+):([0-9]+)\).*/\2:\3 '"$SERVER_ADDR"':\1/p' \
        "$SERVER_LOG" | sort -u || true
}

cmd_server_log() {
    load_env
    printf '%s\n' "$SERVER_LOG"
}

cmd_wire_tuples() {
    load_env
    observe_tuples
}

cmd_old_tuple() {
    load_env
    local tuples count
    tuples=$(observe_tuples)
    count=$(printf '%s\n' "$tuples" | grep -c . || true)
    if [ "$count" -ne 1 ]; then
        die "expected exactly one fake-TCP tuple before the DROP, observed $count"
    fi
    printf '%s\n' "$tuples"
}

cmd_require_new_tuple() {
    [ $# -eq 1 ] || usage
    load_env
    local old=$1 new count
    new=$(observe_tuples | grep -vxF "$old" || true)
    count=$(printf '%s\n' "$new" | grep -c . || true)
    if [ "$count" -eq 1 ]; then
        printf '%s\n' "$new"
        exit 0
    fi
    if [ "$count" -eq 0 ]; then
        printf 'topology: no fake-TCP tuple distinct from %s\n' "$old" >&2
        printf 'topology: the random local port may have reproduced the old tuple, or no new session started\n' >&2
        printf 'topology: tuple-collision policy: fail the run (exit 4), never retry silently\n' >&2
        exit 4
    fi
    die "expected one tuple distinct from $old, observed $count"
}

# --- old-flow DROP ---------------------------------------------------------

parse_side() {
    local side=$1 name=$2
    [[ "$side" =~ ^([0-9]+\.[0-9]+\.[0-9]+\.[0-9]+):([0-9]+)$ ]] \
        || state_error "bad $name address '$side', expected IPv4:PORT"
}

split_side() {
    local side=$1 name=$2
    parse_side "$side" "$name"
    printf '%s %s\n' "${BASH_REMATCH[1]}" "${BASH_REMATCH[2]}"
}

cmd_drop() {
    local directions=both
    while [ $# -gt 0 ]; do
        case "$1" in
            --reply-only)
                directions=reply
                shift
                ;;
            --request-only)
                directions=request
                shift
                ;;
            *) break ;;
        esac
    done
    local client server
    case $# in
        1)
            [[ "$1" == *" "* ]] || state_error "expected 'CLIENT:PORT SERVER:PORT'"
            client=${1% *}
            server=${1#* }
            ;;
        2)
            client=$1
            server=$2
            ;;
        *)
            usage
            ;;
    esac
    load_env
    read -r c_addr c_port <<<"$(split_side "$client" client)"
    read -r s_addr s_port <<<"$(split_side "$server" server)"

    local want_request=0 want_reply=0
    case "$directions" in
        both) want_request=1; want_reply=1 ;;
        request) want_request=1 ;;
        reply) want_reply=1 ;;
    esac

    if is_nft; then
        nsrun "$ROUTER_NS" nft add table ip "$DROP_TABLE" 2>/dev/null || true
        nsrun "$ROUTER_NS" nft add chain ip "$DROP_TABLE" forward \
            '{ type filter hook forward priority 0; policy accept; }' 2>/dev/null || true
        nsrun "$ROUTER_NS" nft flush chain ip "$DROP_TABLE" forward
        if [ "$want_request" -eq 1 ]; then
            nsrun "$ROUTER_NS" nft add rule ip "$DROP_TABLE" forward \
                ip protocol tcp ip saddr "$c_addr" tcp sport "$c_port" \
                ip daddr "$s_addr" tcp dport "$s_port" \
                counter drop comment "phantun-stand-request"
        fi
        if [ "$want_reply" -eq 1 ]; then
            nsrun "$ROUTER_NS" nft add rule ip "$DROP_TABLE" forward \
                ip protocol tcp ip saddr "$s_addr" tcp sport "$s_port" \
                ip daddr "$c_addr" tcp dport "$c_port" \
                counter drop comment "phantun-stand-reply"
        fi
    else
        nsrun "$ROUTER_NS" "$IPT" -N "$DROP_CHAIN" 2>/dev/null || true
        nsrun "$ROUTER_NS" "$IPT" -C FORWARD -j "$DROP_CHAIN" 2>/dev/null \
            || nsrun "$ROUTER_NS" "$IPT" -A FORWARD -j "$DROP_CHAIN"
        nsrun "$ROUTER_NS" "$IPT" -F "$DROP_CHAIN"
        if [ "$want_request" -eq 1 ]; then
            nsrun "$ROUTER_NS" "$IPT" -A "$DROP_CHAIN" \
                -p tcp -s "$c_addr" --sport "$c_port" -d "$s_addr" --dport "$s_port" \
                -m comment --comment "$MARKER-old-fwd" -j DROP
        fi
        if [ "$want_reply" -eq 1 ]; then
            nsrun "$ROUTER_NS" "$IPT" -A "$DROP_CHAIN" \
                -p tcp -s "$s_addr" --sport "$s_port" -d "$c_addr" --dport "$c_port" \
                -m comment --comment "$MARKER-old-reply" -j DROP
        fi
    fi
    printf 'drop: installed %s direction between %s and %s in %s (post-NAT, post-DNAT)\n' \
        "$directions" "$client" "$server" "$ROUTER_NS"
}

cmd_drop_stats() {
    load_env
    local fwd=0 rev=0
    if is_nft; then
        local table
        table=$(nsrun "$ROUTER_NS" nft list table ip "$DROP_TABLE" 2>/dev/null || true)
        [ -n "$table" ] || die "DROP rules are not installed"
        fwd=$(printf '%s\n' "$table" \
            | sed -nE 's/.*counter packets ([0-9]+) bytes [0-9]+.*phantun-stand-request.*/\1/p' \
            | sed -n 1p)
        rev=$(printf '%s\n' "$table" \
            | sed -nE 's/.*counter packets ([0-9]+) bytes [0-9]+.*phantun-stand-reply.*/\1/p' \
            | sed -n 1p)
    else
        local saved
        saved=$(nsrun "$ROUTER_NS" "${IPT}-save" -c 2>/dev/null || true)
        [ -n "$saved" ] || die "DROP rules are not installed"
        fwd=$(printf '%s\n' "$saved" \
            | sed -nE "s/^\[([0-9]+):[0-9]+\].*$MARKER-old-fwd.*/\1/p" | sed -n 1p)
        rev=$(printf '%s\n' "$saved" \
            | sed -nE "s/^\[([0-9]+):[0-9]+\].*$MARKER-old-reply.*/\1/p" | sed -n 1p)
    fi
    printf 'client_to_server_packets=%s\n' "${fwd:-0}"
    printf 'server_to_client_packets=%s\n' "${rev:-0}"
}

cmd_drop_remove() {
    load_env
    if is_nft; then
        nsrun "$ROUTER_NS" nft delete table ip "$DROP_TABLE" 2>/dev/null || true
    else
        nsrun "$ROUTER_NS" "$IPT" -D FORWARD -j "$DROP_CHAIN" 2>/dev/null || true
        nsrun "$ROUTER_NS" "$IPT" -F "$DROP_CHAIN" 2>/dev/null || true
        nsrun "$ROUTER_NS" "$IPT" -X "$DROP_CHAIN" 2>/dev/null || true
    fi
    printf 'drop: removed\n'
}

cmd_ns_exec() {
    [ $# -ge 2 ] || usage
    local alias=$1
    shift
    [ "$1" = -- ] && shift
    [ $# -gt 0 ] || usage
    load_env
    local ns
    ns=$(ns_name_for "$alias")
    nsrun "$ns" "$@"
}

case "$command_name" in
    setup) cmd_setup "$@" ;;
    teardown) cmd_teardown "$@" ;;
    check) cmd_check "$@" ;;
    status) cmd_status "$@" ;;
    server-log) cmd_server_log "$@" ;;
    wire-tuples) cmd_wire_tuples "$@" ;;
    old-tuple) cmd_old_tuple "$@" ;;
    require-new-tuple) cmd_require_new_tuple "$@" ;;
    drop) cmd_drop "$@" ;;
    drop-stats) cmd_drop_stats "$@" ;;
    drop-remove) cmd_drop_remove "$@" ;;
    ns-exec) cmd_ns_exec "$@" ;;
    --help|-h|help) usage ;;
    *) printf 'topology: unknown command: %s\n' "$command_name" >&2; usage ;;
esac
