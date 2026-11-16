#!/usr/bin/env bash
# Workflow-parity run: boot the real markdown-wizard binary on a
# scratch port with a fresh data dir, drive it with Playwright
# through every workflow Dave Winer's prototype offers, and prove
# the replacement workflow on md.phext.io's own app. Write workflows
# run ONLY against this local instance — nothing is published to
# the live site. With PARITY_LIVE=1, a read-only smoke pass against
# https://md.phext.io/ follows.
#
# Usage: scripts/parity.sh [port]
set -euo pipefail

cd "$(dirname "$0")/.."
PORT="${1:-18131}"
BASE="http://127.0.0.1:$PORT"
WORK="$(mktemp -d)"

# Always build: the binary under test must be the one this tree
# compiles to. Building only when missing once let a stale binary
# answer for a fresh tree (a phantom /rss.xml 404 against the feeds
# build). When the tree is unchanged this is a fast no-op check.
cargo build --release

./target/release/markdown-wizard --dir "$WORK/data" --port "$PORT" --host 127.0.0.1 \
  > "$WORK/server.log" 2>&1 &
SERVER_PID=$!
trap 'kill $SERVER_PID 2>/dev/null || true' EXIT
for i in $(seq 1 50); do
  curl -sf "$BASE/api/status" > /dev/null && break
  sleep 0.1
done

cd tests/parity
if [ ! -d node_modules ]; then
  npm install --no-audit --no-fund
fi
npx playwright install --only-shell chromium > /dev/null 2>&1 || npx playwright install chromium

PARITY_BASE_URL="$BASE" npx playwright test --config playwright.config.js

if [ "${PARITY_LIVE:-0}" = "1" ]; then
  npx playwright test --config playwright.live.config.js
fi

echo "PARITY OK"
