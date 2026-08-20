#!/usr/bin/env bash
# Two throwaway peer boxes in Docker, with their own key and ssh config.
#
# Nothing here touches ~/.ssh: the key lives in .secrets/ and the generated
# ssh config is passed explicitly to ssh with -F.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
secrets="$here/.secrets"
key="$secrets/id_ed25519"
config="$secrets/ssh_config"
repo_root="$(cd "$here/../.." && pwd)"

ensure_key() {
  mkdir -p "$secrets"
  if [[ ! -f "$key" ]]; then
    ssh-keygen -t ed25519 -N '' -f "$key" -C herdr-peer-box -q
  fi
  cp "$key.pub" "$here/authorized_keys"
  # StrictHostKeyChecking is off and known_hosts is discarded because these
  # containers are recreated constantly and their host keys change every time.
  # Safe only because the target is a loopback port we just started ourselves.
  cat > "$config" <<CONF
Host box1 box2 box3
  HostName 127.0.0.1
  User herdr
  IdentityFile $key
  IdentitiesOnly yes
  StrictHostKeyChecking no
  UserKnownHostsFile /dev/null
  LogLevel ERROR

Host box1
  Port 2201

Host box2
  Port 2202

Host box3
  Port 2203
CONF
  chmod 600 "$config"
  # The boxes' own view of each other: same key, but they reach each other by
  # container hostname on port 22, not through the host's published ports.
  cat > "$secrets/box_ssh_config" <<CONF
Host box1 box2 box3
  User herdr
  IdentityFile /home/herdr/.ssh/id_ed25519
  IdentitiesOnly yes
  StrictHostKeyChecking no
  UserKnownHostsFile /dev/null
  LogLevel ERROR
CONF
  chmod 600 "$secrets/box_ssh_config"
}

container() { echo "herdr-$1"; }

require_running() {
  local name
  name="$(container "$1")"
  [[ "$(docker inspect -f '{{.State.Running}}' "$name" 2>/dev/null)" == "true" ]] \
    || { echo "$1 is not running; boxes.sh up first" >&2; exit 2; }
}

# A box's address on the peers network, as the other boxes see it. Looked up
# live rather than pinned: Docker hands these out afresh on every recreate.
box_ip() {
  docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' "$(container "$1")"
}

# The interface the box reaches everything through. These containers have one,
# but asking the routing table beats hardcoding `eth0`.
box_dev() {
  docker exec "$(container "$1")" \
    sh -c "ip -o -4 route show default | awk '{print \$5; exit}'"
}

# Shape a box's *egress*. With `--to`, only traffic destined for another box is
# touched: band 3 of the prio qdisc is unreachable from the priomap, so nothing
# but the destination filter can put a packet in it. That is what keeps the ssh
# this script runs on healthy while the two boxes are 100% partitioned -- the
# host reaches the box through its published port, not through the peer address.
netem_apply() {
  local box="$1" to="$2" delay="$3" loss="$4"
  require_running "$box"
  local dev script
  dev="$(box_dev "$box")"
  [[ -n "$dev" ]] || { echo "netem: $box has no default route" >&2; exit 2; }
  script="tc qdisc del dev $dev root 2>/dev/null || true;"
  if [[ -n "$to" ]]; then
    require_running "$to"
    local ip
    ip="$(box_ip "$to")"
    [[ -n "$ip" ]] || { echo "netem: could not resolve $to's address" >&2; exit 2; }
    script+=" tc qdisc add dev $dev root handle 1: prio bands 4"
    script+=" priomap 1 2 2 2 1 2 0 0 1 1 1 1 1 1 1 1;"
    script+=" tc qdisc add dev $dev parent 1:4 handle 40: netem delay $delay loss $loss;"
    script+=" tc filter add dev $dev protocol ip parent 1:0 prio 4 u32"
    script+=" match ip dst $ip/32 flowid 1:4"
  else
    script+=" tc qdisc add dev $dev root netem delay $delay loss $loss"
  fi
  docker exec "$(container "$box")" sh -c "$script"
  echo "netem: $box${to:+ -> $to} delay $delay loss $loss (dev $dev)"
}

netem_clear() {
  local box="$1"
  require_running "$box"
  local dev
  dev="$(box_dev "$box")"
  docker exec "$(container "$box")" sh -c "tc qdisc del dev $dev root 2>/dev/null || true"
  echo "netem: $box cleared (dev $dev)"
}

netem_show() {
  local box="$1"
  require_running "$box"
  local dev
  dev="$(box_dev "$box")"
  docker exec "$(container "$box")" sh -c "tc qdisc show dev $dev; tc filter show dev $dev"
}

case "${1:-up}" in
  up)
    ensure_key
    [[ -x "$repo_root/target/debug/herdr" ]] || {
      echo "target/debug/herdr is missing; build it first" >&2
      exit 2
    }
    docker compose -f "$here/compose.yml" up -d --build
    for box in box1 box2 box3; do
      for _ in $(seq 1 30); do
        if ssh -F "$config" -o ConnectTimeout=2 "$box" true 2>/dev/null; then
          break
        fi
        sleep 1
      done
      ssh -F "$config" -o ConnectTimeout=2 "$box" true \
        || { echo "$box never accepted ssh" >&2; exit 1; }
    done
    echo "up: box1 box2 box3 (ssh config: $config)"
    ;;
  down)
    docker compose -f "$here/compose.yml" down -v --remove-orphans
    ;;
  status)
    docker compose -f "$here/compose.yml" ps
    for box in box1 box2 box3; do
      printf '%s: ' "$box"
      ssh -F "$config" -o ConnectTimeout=2 "$box" \
        'echo "$(uname -n) herdr=$(command -v herdr || echo none) $(herdr --version 2>/dev/null || true)"' \
        2>/dev/null || echo unreachable
    done
    ;;
  config)
    echo "$config"
    ;;
  netem)
    shift
    box="${1:-}"
    [[ -n "$box" ]] || {
      echo "usage: boxes.sh netem <box> [--to <box>] <delay> <loss>|clear|show" >&2
      exit 2
    }
    shift
    to=""
    if [[ "${1:-}" == "--to" ]]; then
      to="${2:-}"
      [[ -n "$to" ]] || { echo "netem: --to needs a box" >&2; exit 2; }
      shift 2
    fi
    case "${1:-}" in
      show) netem_show "$box" ;;
      clear) netem_clear "$box" ;;
      "")
        echo "usage: boxes.sh netem <box> [--to <box>] <delay> <loss>|clear|show" >&2
        exit 2
        ;;
      *) netem_apply "$box" "$to" "$1" "${2:-0%}" ;;
    esac
    ;;
  *)
    echo "usage: boxes.sh [up|down|status|config|netem]" >&2
    exit 2
    ;;
esac
