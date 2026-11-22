#!/usr/bin/env bash
# Environment contract for the isolated session-recovery stand.
#
# Verifies that this machine can host the stand: Linux, non-interactive root
# (or `sudo -n`), network namespaces, /dev/net/tun, iproute2, a firewall
# backend, a general timeout tool and a usable Rust toolchain with the
# workspace dependencies. Exits non-zero when any capability is missing;
# skipping the stand is not acceptance (see docs/plans/2026-11-22-session-recovery.md).
#
# Usage:
#   stand/preflight.sh                 # human-readable check
#   stand/preflight.sh --allow-network # allow `cargo fetch` to use the network
#   stand/preflight.sh --print-env     # print KEY=VALUE contract on stdout
#
# Environment:
#   STAND_PROJECT_ROOT       workspace root (default: parent of this script)
#   STAND_ALLOW_NETWORK=1    same as --allow-network
#   STAND_FIREWALL_BACKEND   pin one of nft, iptables-nft, iptables-legacy, iptables
#
# Exit codes: 0 = contract satisfied, 1 = capability missing, 2 = usage error.

set -u

SCRIPT_DIR=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
PROJECT_ROOT=${STAND_PROJECT_ROOT:-$(CDPATH='' cd -- "$SCRIPT_DIR/.." && pwd)}
ALLOW_NETWORK=${STAND_ALLOW_NETWORK:-0}
FIREWALL_REQUEST=${STAND_FIREWALL_BACKEND:-}
PRINT_ENV=0
FAILURES=()

usage() {
    printf 'usage: %s [--allow-network] [--print-env] [--help]\n' "${0##*/}" >&2
}

for arg in "$@"; do
    case "$arg" in
        --allow-network) ALLOW_NETWORK=1 ;;
        --print-env) PRINT_ENV=1 ;;
        --help|-h) usage; exit 0 ;;
        *) printf 'unknown argument: %s\n' "$arg" >&2; usage; exit 2 ;;
    esac
done

# Human-readable diagnostics never pollute the machine-readable contract.
out() {
    if [ "$PRINT_ENV" -eq 1 ]; then
        printf '%s\n' "$*" >&2
    else
        printf '%s\n' "$*"
    fi
}

pass() { out "ok: $*"; }

fail() {
    FAILURES+=("$*")
    printf 'preflight: FAIL: %s\n' "$*" >&2
}

die_early() {
    printf 'preflight: FAIL: %s\n' "$*" >&2
    printf 'preflight: environment does not satisfy the stand contract\n' >&2
    exit 1
}

# No check may ever wait for input: stdin is closed and sudo is always -n.
exec </dev/null

if [ "$(uname -s)" != "Linux" ]; then
    die_early "the stand requires Linux (running on $(uname -s))"
fi
pass "Linux kernel"

# --- privileges -----------------------------------------------------------
SUDO=()
PRIV_MODE=
if [ "$(id -u)" -eq 0 ]; then
    PRIV_MODE=root
    pass "root privileges"
elif command -v sudo >/dev/null 2>&1 && sudo -n true >/dev/null 2>&1; then
    SUDO=(sudo -n)
    PRIV_MODE=sudo-n
    pass "non-interactive sudo -n"
else
    fail "neither root nor non-interactive 'sudo -n' is available"
fi

# --- iproute2 and network namespaces --------------------------------------
IP_OK=0
if command -v ip >/dev/null 2>&1; then
    IP_OK=1
    pass "iproute2 ($(command -v ip))"
else
    fail "iproute2 ('ip' command) is not installed"
fi

NETNS=phantun-preflight-$$
TUN=phunpre$$
NETNS_CREATED=0
TUN_CREATED=0

# Invoked indirectly through the EXIT/INT/TERM trap below.
# shellcheck disable=SC2329
cleanup() {
    if [ "$TUN_CREATED" -eq 1 ]; then
        "${SUDO[@]}" ip link del "$TUN" >/dev/null 2>&1 || true
    fi
    if [ "$NETNS_CREATED" -eq 1 ]; then
        "${SUDO[@]}" ip netns del "$NETNS" >/dev/null 2>&1 || true
    fi
}
trap cleanup EXIT INT TERM

if [ "$PRIV_MODE" = "" ]; then
    fail "network namespaces not verified: privileges are unavailable"
elif [ "$IP_OK" -eq 1 ]; then
    if "${SUDO[@]}" ip netns add "$NETNS" >/dev/null 2>&1; then
        NETNS_CREATED=1
        if "${SUDO[@]}" ip netns del "$NETNS" >/dev/null 2>&1; then
            NETNS_CREATED=0
            pass "network namespaces"
        else
            fail "network namespaces: namespace created but not removed"
        fi
    else
        fail "network namespaces: 'ip netns add' failed"
    fi
else
    fail "network namespaces not verified: iproute2 is missing"
fi

# --- TUN device -----------------------------------------------------------
if [ "$PRIV_MODE" = "" ]; then
    fail "/dev/net/tun not verified: privileges are unavailable"
elif [ "$IP_OK" -eq 0 ]; then
    fail "/dev/net/tun not verified: iproute2 is missing"
else
    if [ ! -c /dev/net/tun ]; then
        fail "/dev/net/tun is missing or is not a character device"
    elif "${SUDO[@]}" ip tuntap add dev "$TUN" mode tun >/dev/null 2>&1; then
        TUN_CREATED=1
        if "${SUDO[@]}" ip link del "$TUN" >/dev/null 2>&1; then
            TUN_CREATED=0
            pass "/dev/net/tun usable"
        else
            fail "/dev/net/tun: TUN device created but not removed"
        fi
    else
        fail "/dev/net/tun exists but creating a TUN device failed"
    fi
fi

# --- firewall backend -----------------------------------------------------
FIREWALL_BIN=
FIREWALL_CANDIDATES=(nft iptables-nft iptables-legacy iptables)
if [ -n "$FIREWALL_REQUEST" ]; then
    case "$FIREWALL_REQUEST" in
        nft|iptables-nft|iptables-legacy|iptables)
            FIREWALL_CANDIDATES=("$FIREWALL_REQUEST") ;;
        *)
            printf 'invalid STAND_FIREWALL_BACKEND: %s\n' "$FIREWALL_REQUEST" >&2
            usage
            exit 2 ;;
    esac
fi

firewall_usable() {
    local bin=$1
    command -v "$bin" >/dev/null 2>&1 || return 1
    if [ "$bin" = "nft" ]; then
        "${SUDO[@]}" nft list ruleset >/dev/null 2>&1
    else
        "${SUDO[@]}" "$bin" -w -S >/dev/null 2>&1
    fi
}

if [ "$PRIV_MODE" = "" ]; then
    fail "firewall backend not verified: privileges are unavailable"
else
    for candidate in "${FIREWALL_CANDIDATES[@]}"; do
        if firewall_usable "$candidate"; then
            FIREWALL_BIN=$candidate
            break
        fi
    done
    if [ -n "$FIREWALL_BIN" ]; then
        pass "firewall backend ($FIREWALL_BIN)"
    elif [ -n "$FIREWALL_REQUEST" ]; then
        fail "selected firewall backend '$FIREWALL_REQUEST' is not usable"
    else
        fail "no usable firewall backend (tried: ${FIREWALL_CANDIDATES[*]})"
    fi
fi

# --- general timeout tool -------------------------------------------------
TIMEOUT_BIN=
for candidate in timeout gtimeout; do
    if command -v "$candidate" >/dev/null 2>&1; then
        TIMEOUT_BIN=$candidate
        break
    fi
done
if [ -n "$TIMEOUT_BIN" ]; then
    pass "timeout tool ($TIMEOUT_BIN)"
else
    fail "no general timeout tool ('timeout' or 'gtimeout') found"
fi

# --- Rust toolchain and dependencies --------------------------------------
if ! command -v cargo >/dev/null 2>&1; then
    fail "cargo is not installed"
elif ! command -v rustc >/dev/null 2>&1; then
    fail "rustc is not installed"
elif [ ! -f "$PROJECT_ROOT/Cargo.toml" ]; then
    fail "workspace not found at $PROJECT_ROOT (set STAND_PROJECT_ROOT)"
else
    pass "Rust toolchain ($(cargo --version 2>/dev/null))"
    if [ -z "$TIMEOUT_BIN" ]; then
        fail "workspace dependencies not verified: no timeout tool"
    elif (cd "$PROJECT_ROOT" && "$TIMEOUT_BIN" 300 cargo fetch --locked --offline >/dev/null 2>&1); then
        pass "workspace dependencies are cached for offline use"
    elif [ "$ALLOW_NETWORK" -eq 1 ]; then
        if (cd "$PROJECT_ROOT" && "$TIMEOUT_BIN" 600 cargo fetch --locked); then
            pass "workspace dependencies fetched from the network"
        else
            fail "workspace dependencies could not be fetched from the network"
        fi
    else
        fail "workspace dependencies are not cached; rerun with --allow-network or STAND_ALLOW_NETWORK=1"
    fi
fi

# --- verdict --------------------------------------------------------------
if [ "${#FAILURES[@]}" -ne 0 ]; then
    printf 'preflight: environment does not satisfy the stand contract (%d problem(s))\n' \
        "${#FAILURES[@]}" >&2
    exit 1
fi

if [ "$PRINT_ENV" -eq 1 ]; then
    printf 'STAND_PRIV=%q\n' "$PRIV_MODE"
    printf 'STAND_SUDO=%q\n' "${SUDO[*]:-}"
    printf 'STAND_FIREWALL_BACKEND=%q\n' "$FIREWALL_BIN"
    printf 'STAND_TIMEOUT=%q\n' "$TIMEOUT_BIN"
    printf 'STAND_PROJECT_ROOT=%q\n' "$PROJECT_ROOT"
    printf 'STAND_ALLOW_NETWORK=%q\n' "$ALLOW_NETWORK"
fi

out "preflight: OK"
exit 0
