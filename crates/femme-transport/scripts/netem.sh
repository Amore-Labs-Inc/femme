#!/bin/sh
# Make loopback behave like a bad network, for the P2 measurement.
#
# NEEDS ROOT (sudo) and `tc` for everything except `status`. Without root there is no netem: run the
# example with its own userspace relay instead (`p2_latency --emu 40,5,1`, see its docs).
#
#   sudo scripts/netem.sh up --host delay 40ms 5ms + 1% loss on the HOST's lo each way: RTT 80 ms +-10 ms
#   sudo scripts/netem.sh down      remove it
#   scripts/netem.sh status
#   sudo scripts/netem.sh netns CMD...
#                                   run CMD (as the calling user) in a throwaway network namespace
#                                   with that netem on its own lo, so nothing else on the machine
#                                   is slowed down; the namespace is deleted afterwards
#
# Then, with `up` applied (or through `netns`), from the repo root:
#
#   cargo run --release -p femme-transport --example p2_latency -- --rtt-ms 80
#
# `up` hits EVERY program that talks over 127.0.0.1 (databases, dev servers, other agents), so it
# refuses to run without `--host`. Prefer `netns` on a shared machine. Override DELAY, JITTER and LOSS
# for another profile.
set -eu

DELAY=${DELAY:-40ms}
JITTER=${JITTER:-5ms}
LOSS=${LOSS:-1%}
DEV=${DEV:-lo}

# Everything but `status` changes network state.
case "${1:-}" in
  up | down | netns)
    [ "$(id -u)" -eq 0 ] || { echo "$1 needs root: run it with sudo" >&2; exit 1; }
    ;;
esac

case "${1:-}" in
  up)
    [ "${2:-}" = "--host" ] || {
      echo "up slows EVERY program on this machine that talks over $DEV (databases, dev servers, other agents)." >&2
      echo "Use 'netns CMD...' to slow only CMD, or pass --host if the whole machine really is what you want." >&2
      exit 2
    }
    tc qdisc replace dev "$DEV" root netem delay "$DELAY" "$JITTER" loss "$LOSS"
    tc qdisc show dev "$DEV"
    ;;
  down)
    tc qdisc del dev "$DEV" root 2>/dev/null || true
    tc qdisc show dev "$DEV"
    ;;
  status)
    tc qdisc show dev "$DEV"
    ;;
  netns)
    shift
    [ $# -gt 0 ] || { echo "netns needs a command to run" >&2; exit 2; }
    NS=femme-netem-$$
    ip netns add "$NS"
    trap 'ip netns del "$NS" 2>/dev/null || true' EXIT INT TERM
    ip -n "$NS" link set lo up
    ip netns exec "$NS" tc qdisc add dev lo root netem delay "$DELAY" "$JITTER" loss "$LOSS"
    ip netns exec "$NS" tc qdisc show dev lo
    # Run as whoever called sudo, not as root.
    if [ -n "${SUDO_USER:-}" ]; then
      ip netns exec "$NS" sudo -u "$SUDO_USER" env "PATH=$PATH" "$@"
    else
      ip netns exec "$NS" "$@"
    fi
    ;;
  *)
    sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'
    exit 2
    ;;
esac
