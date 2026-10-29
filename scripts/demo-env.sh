#!/usr/bin/env bash
# One command to bring up everything a demo needs, and to put it right again when something drifts.
#
#   ./scripts/demo-env.sh up        start the Lightning network, server, demo wallet and web app; seed the cast
#   ./scripts/demo-env.sh refresh   mine a block (LND reports "not synced" when the newest block is over 2 hours old)
#   ./scripts/demo-env.sh status    what is running, and each node's spendable balance
#   ./scripts/demo-env.sh down      stop the server, wallet and web app (the Lightning network keeps running)
#   ./scripts/demo-env.sh reset     down, then forget the demo database and cast so the next `up` starts clean
#   ./scripts/demo-env.sh sample    add a realistic mix of escrows for the cast, so a dashboard has something to show
#   ./scripts/demo-env.sh repair    wipe the Lightning network and the demo state, for when Docker comes back broken
#                                   after a restart ("RWLayer ... is unexpectedly nil"); then run `up` again
#
#   PUBLIC_URL=https://you.ngrok-free.dev ./scripts/demo-env.sh up   also let the demo wallet answer a tunnelled page
#   (A new withdrawal PIN works at once in the demo; a real server makes it wait 24 hours: PIN_COOLOFF_HOURS.)
#   PLATFORM_ARBITER_ID=u_... ./scripts/demo-env.sh up   make that account the arbiter of escrows that name none
#                                   (the seeded arbiter is .demo/accounts.json "arbiter"; this changes every escrow's flow)
#
# Local regtest only. State lives in .demo/ (gitignored): the operator token, the key that encrypts
# sellers' node credentials, the database, logs, pids, and the seeded accounts with their tokens.
set -euo pipefail

cd "$(dirname "$0")/.."
ROOT=$(pwd)
DEMO="$ROOT/.demo"
DC="docker compose -f regtest/docker-compose.yml"
API=${API:-http://127.0.0.1:3000}
WEB=${WEB:-http://localhost:3001}
WALLET=${WALLET:-http://127.0.0.1:3100}
mkdir -p "$DEMO"

say()  { printf '\033[1m==> %s\033[0m\n' "$*"; }
warn() { printf '\033[33mwarning:\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[31merror:\033[0m %s\n' "$*" >&2; exit 1; }
up_http() { curl -fsS -o /dev/null --max-time 3 "$1" 2>/dev/null; }
btc() { $DC exec -T bitcoind bitcoin-cli -regtest -rpcuser=toshi -rpcpassword=toshi "$@"; }

wait_http() { # name url seconds
  for _ in $(seq 1 "$3"); do up_http "$2" && return 0; sleep 1; done
  die "$1 did not come up at $2 (see $DEMO/$1.log)"
}

stop_pid() { # name
  local f="$DEMO/$1.pid"
  [ -f "$f" ] || return 0
  local pid; pid=$(cat "$f")
  if kill -0 "$pid" 2>/dev/null; then
    pkill -P "$pid" 2>/dev/null || true   # children first (npm run dev spawns node)
    kill "$pid" 2>/dev/null || true
  fi
  rm -f "$f"
}

# Stop whatever is listening on a TCP port (used when a pid file was lost, e.g. .demo/ was deleted).
kill_port() {
  local pids
  pids=$(ss -ltnp "sport = :$1" 2>/dev/null | grep -o 'pid=[0-9]*' | cut -d= -f2 | sort -u || true)
  [ -n "$pids" ] && kill $pids 2>/dev/null || true
}

mine_one() {
  btc loadwallet miner >/dev/null 2>&1 || true
  btc -rpcwallet=miner -generate=1 >/dev/null
}

preflight() {
  for c in docker cargo node npm curl openssl python3 protoc; do
    command -v "$c" >/dev/null || die "$c is not installed"
  done
  docker info >/dev/null 2>&1 || die "docker is not running"
  [ "$(node -p 'process.versions.node.split(".")[0]')" -ge 20 ] || die "Node 20 or newer is needed"
  # Some container hosts route bridged traffic through a legacy firewall that drops it, so LND never
  # reaches bitcoind. Turning that off is harmless and lasts until the machine restarts.
  if [ -r /proc/sys/net/bridge/bridge-nf-call-iptables ] && [ "$(cat /proc/sys/net/bridge/bridge-nf-call-iptables)" = 1 ]; then
    if sudo -n sysctl -w net.bridge.bridge-nf-call-iptables=0 >/dev/null 2>&1; then
      say "set net.bridge.bridge-nf-call-iptables=0 so containers can reach each other"
    else
      warn "net.bridge.bridge-nf-call-iptables is 1; if LND never syncs run: sudo sysctl -w net.bridge.bridge-nf-call-iptables=0"
    fi
  fi
}

load_secrets() {
  [ -f "$DEMO/env" ] || {
    umask 077
    {
      echo "OPERATOR_TOKEN=$(openssl rand -hex 16)"
      echo "NODE_CREDENTIAL_KEY=$(openssl rand -hex 32)"
    } > "$DEMO/env"
  }
  # shellcheck disable=SC1091
  set -a; . "$DEMO/env"; set +a
}

start_lightning() {
  say "Lightning regtest network (bitcoind + buyer, router, seller, seller2)"
  if ! ./regtest/setup.sh > "$DEMO/regtest.log" 2>&1; then
    warn "setup did not finish; mining a block (stale chain is the usual cause) and retrying once"
    $DC up -d >/dev/null 2>&1 || true
    mine_one || true
    ./regtest/setup.sh > "$DEMO/regtest.log" 2>&1 || { tail -5 "$DEMO/regtest.log" >&2; die "the regtest network would not start (log: .demo/regtest.log)"; }
  fi
  mine_one   # always leave a fresh block so LND reports it is synced
}

# Lightning nodes learn each other's channels by gossip, which takes a few seconds after the network
# is built. Until the buyer can find a route to a seller, a payment fails, so wait for that before
# reporting that everything is up.
wait_for_routes() {
  say "waiting for the buyer to learn the routes to both sellers"
  local node dest
  for node in seller seller2; do
    dest=$($DC exec -T "$node" lncli --network=regtest getinfo | python3 -c 'import sys,json; print(json.load(sys.stdin)["identity_pubkey"])')
    for _ in $(seq 1 45); do
      $DC exec -T buyer lncli --network=regtest queryroutes --dest "$dest" --amt 1000 >/dev/null 2>&1 && continue 2
      sleep 2
    done
    warn "the buyer cannot find a route to $node yet; a payment right now may fail (try again in a minute)"
  done
}

start_server() {
  say "toshi-server on $API"
  if up_http "$API/health"; then say "already running"; return; fi
  cargo build -q -p escrow-api --bin toshi-server 2> "$DEMO/build.log" || { cat "$DEMO/build.log" >&2; die "build failed"; }
  # Each service gets its own redirected, detached shell and is exec'd, so the pid file holds the real
  # process and nothing keeps this script's output pipe open after it exits.
  # A room full of people behind one network address would trip the default 10 sign-ups a minute.
  # Regtest has no market, so the fiat values use fixed, clearly labelled "demo rate"s.
  # Sellers on the shared node get a balance they can withdraw, and naira payments go through the
  # pretend "demo" provider, whose Lightning side is the buyer node. Setup bakes both macaroons.
  local payout=""
  [ -f regtest/creds/seller/payout.macaroon ] && payout="regtest/creds/seller/payout.macaroon"
  [ -n "$payout" ] || warn "no regtest/creds/seller/payout.macaroon: balances can't be withdrawn (re-run regtest/setup.sh)"
  local ramp=off
  [ -f regtest/creds/buyer/payout.macaroon ] && ramp="${RAMP_PROVIDER:-demo}"
  ( exec env DATABASE_PATH="$DEMO/toshi.db" ALLOW_PRIVATE_NODE_ADDRESSES=1 REGISTER_LIMIT_PER_MIN=200 \
      FIAT_PROVIDER="${FIAT_PROVIDER:-fixed:USD=65000,EUR=60000,GBP=51000,NGN=97500000}" \
      PAYOUT_MACAROON="$payout" RAMP_PROVIDER="$ramp" PLATFORM_ARBITER_ID="${PLATFORM_ARBITER_ID:-}" PIN_COOLOFF_HOURS="${PIN_COOLOFF_HOURS:-0}" \
      ./target/debug/toshi-server ) \
    > "$DEMO/server.log" 2>&1 < /dev/null &
  echo $! > "$DEMO/server.pid"
  wait_http server "$API/health" 40
}

# In a Codespace (or through a tunnel such as ngrok, given as PUBLIC_URL) the page is opened through
# a forwarded address, not localhost. The demo wallet only answers requests from the web app, so tell
# it that address as well.
public_origin() {
  if [ -n "${PUBLIC_URL:-}" ]; then
    echo "${PUBLIC_URL%/}"
  elif [ -n "${CODESPACE_NAME:-}" ] && [ -n "${GITHUB_CODESPACES_PORT_FORWARDING_DOMAIN:-}" ]; then
    echo "https://${CODESPACE_NAME}-3001.${GITHUB_CODESPACES_PORT_FORWARDING_DOMAIN}"
  fi
}

start_wallet() {
  say "demo wallet on $WALLET"
  if up_http "$WALLET/health"; then say "already running"; return; fi
  local origins="http://localhost:3001,http://127.0.0.1:3001"
  [ -n "$(public_origin)" ] && origins="$origins,$(public_origin)"
  ( exec env DEMO_WALLET_ORIGINS="$origins" node scripts/demo-wallet.mjs ) > "$DEMO/wallet.log" 2>&1 < /dev/null &
  echo $! > "$DEMO/wallet.pid"
  wait_http wallet "$WALLET/health" 10
}

start_web() {
  say "web app on $WEB"
  if up_http "$WEB"; then say "already running (restart it with 'down' then 'up' if the demo wallet button is missing)"; return; fi
  [ -d web/node_modules ] || (cd web && npm install --no-audit --no-fund >/dev/null)
  # The browser only ever talks to this one port: the app proxies the API and the demo wallet, so it
  # works the same through a forwarded Codespace address as on localhost, with nothing else exposed.
  ( cd web && exec env API_PROXY_TARGET="$API" DEMO_WALLET_TARGET="$WALLET" \
      NEXT_PUBLIC_API_URL="" NEXT_PUBLIC_DEMO_WALLET_URL="/demo-wallet" npm run dev -- -p 3001 ) \
    > "$DEMO/web.log" 2>&1 < /dev/null &
  echo $! > "$DEMO/web.pid"
  wait_http web "$WEB" 60
}

balances() {
  for n in buyer seller seller2; do
    local sats
    sats=$($DC exec -T "$n" lncli --network=regtest channelbalance 2>/dev/null | python3 -c 'import sys,json; print(json.load(sys.stdin)["local_balance"]["sat"])' 2>/dev/null || echo "?")
    printf '  %-8s %s sats spendable\n' "$n" "$sats"
  done
}

summary() {
  cat <<EOS

Everything is up.

  App            $WEB$([ -n "$(public_origin)" ] && printf '\n  In your browser %s  (Codespaces: port 3001)' "$(public_origin)")   (fiat values use demo rates: 65,000 USD / 97,500,000 NGN per bitcoin)
  API            $API   (health: $(curl -fsS "$API/health" 2>/dev/null || echo unreachable))
  Operator token $OPERATOR_TOKEN   (sign in at $WEB/operator)

Cast and tokens: $DEMO/accounts.json
  Arbiter key backup (paste on the Arbiter page > Restore): $DEMO/arbiter-key.json

Node balances:
$(balances)

Run the scripted rehearsal:  (cd web && npm run rehearse)
Before going on stage:       ./scripts/demo-env.sh refresh   (keeps LND synced)
EOS
}

cmd=${1:-}
case "$cmd" in
  up)
    preflight
    load_secrets
    start_lightning
    wait_for_routes
    export OPERATOR_TOKEN NODE_CREDENTIAL_KEY
    start_server
    start_wallet
    start_web
    say "seeding the demo cast"
    API="$API" node scripts/seed-demo.mjs
    summary
    ;;
  refresh)
    mine_one
    say "mined a block"
    curl -fsS "$API/health" 2>/dev/null && echo || warn "server not reachable at $API"
    ;;
  status)
    for pair in "server:$API/health" "wallet:$WALLET/health" "web:$WEB"; do
      name=${pair%%:*}; url=${pair#*:}
      if up_http "$url"; then printf '  %-8s up    %s\n' "$name" "$url"; else printf '  %-8s DOWN  %s\n' "$name" "$url"; fi
    done
    echo "  health   $(curl -fsS --max-time 3 "$API/health" 2>/dev/null || echo unreachable)"
    echo "Node balances:"; balances
    ;;
  down)
    for p in web wallet server; do stop_pid "$p"; done
    # Anything still holding the demo's ports (a lost pid file) is stopped too.
    for port in 3001 3100 3000; do kill_port "$port"; done
    sleep 1
    say "stopped the server, demo wallet and web app (the Lightning network is still running)"
    ;;
  reset)
    "$0" down
    rm -f "$DEMO/toshi.db" "$DEMO/toshi.db-shm" "$DEMO/toshi.db-wal" "$DEMO/accounts.json" "$DEMO/arbiter-key.json"
    say "forgot the demo database and cast; run '$0 up' for a clean start"
    ;;
  sample)
    wait_for_routes
    node scripts/seed-escrows.mjs
    ;;
  repair)
    "$0" down
    say "wiping the Lightning network (regtest only: nothing real is lost) and the demo state"
    $DC --profile postgres down -v 2>&1 | tail -2
    rm -rf "$DEMO/toshi.db" "$DEMO/toshi.db-shm" "$DEMO/toshi.db-wal" "$DEMO/accounts.json" "$DEMO/arbiter-key.json"
    say "done; run '$0 up' for a clean start"
    ;;
  *)
    sed -n '2,16p' "$0" | sed 's/^# \{0,1\}//'
    exit 2
    ;;
esac
