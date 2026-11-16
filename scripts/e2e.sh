#!/usr/bin/env bash
# End-to-end run of the real markdown-wizard server, the way a person
# (and a reader) would meet it: make a zip, publish it, read it back
# at its public addresses, re-publish, restore — and drive the editor
# itself in a headless browser for its self-test and screenshots.
#
# Usage: scripts/e2e.sh [port]
# Expects the release binary at target/release/markdown-wizard
# (builds it if missing) and a Chromium headless shell for the
# browser legs (CHROME_BIN overrides the path).
set -euo pipefail

cd "$(dirname "$0")/.."
PORT="${1:-18099}"
BASE="http://127.0.0.1:$PORT"
WORK="$(mktemp -d)"
SHOTS="${SHOTS_DIR:-$WORK/shots}"
mkdir -p "$SHOTS"
CHROME_BIN="${CHROME_BIN:-$HOME/.cache/ms-playwright/chromium_headless_shell-1255/chrome-headless-shell-linux64/chrome-headless-shell}"

if [ ! -x target/release/markdown-wizard ]; then
  cargo build --release
fi

./target/release/markdown-wizard --dir "$WORK/data" --port "$PORT" --host 127.0.0.1 \
  > "$WORK/server.log" 2>&1 &
SERVER_PID=$!
trap 'kill $SERVER_PID 2>/dev/null || true' EXIT
for i in $(seq 1 50); do
  curl -sf "$BASE/api/status" > /dev/null && break
  sleep 0.1
done

fail() { echo "E2E FAIL: $*" >&2; exit 1; }
note() { echo "e2e: $*"; }

# ——— a small bundle, deflated the way real zips are ———
mkdir -p "$WORK/src/essays"
cat > "$WORK/src/index.md" <<'EOF'
Dear reader,

This is where the end-to-end bundle begins.

## Notes

## Finished
EOF
printf 'The short version of the e2e bundle.\n' > "$WORK/src/about.md"
printf '# One\n\nFirst version.\n' > "$WORK/src/essays/one.md"
printf '# Hidden\n\nNever served.\n' > "$WORK/src/secret-note.md"
(cd "$WORK/src" && python3 -c "
import zipfile
z = zipfile.ZipFile('$WORK/bundle-v1.zip', 'w', zipfile.ZIP_DEFLATED)
for name in ['index.md', 'about.md', 'essays/one.md']:
    z.write(name)
z.writestr('vault/secret-note.md', open('secret-note.md').read())
z.close()
")
printf '# One\n\nSecond version.\n' > "$WORK/src/essays/one.md"
(cd "$WORK/src" && python3 -c "
import zipfile
z = zipfile.ZipFile('$WORK/bundle-v2.zip', 'w', zipfile.ZIP_DEFLATED)
for name in ['index.md', 'about.md', 'essays/one.md']:
    z.write(name)
z.close()
")

# ——— publish v1 ———
RESP=$(curl -sf -X POST "$BASE/api/publish" -H "X-Slug: e2e-bundle" \
  -H "X-Changed: First release" --data-binary @"$WORK/bundle-v1.zip")
KEY=$(printf '%s' "$RESP" | python3 -c "import json,sys; print(json.load(sys.stdin)['edit_key'])")
VERSION=$(printf '%s' "$RESP" | python3 -c "import json,sys; print(json.load(sys.stdin)['version'])")
[ "$VERSION" = "1" ] || fail "first publish version is $VERSION, not 1"
[ -n "$KEY" ] || fail "first publish returned no edit key"
note "published v1, key issued"

# ——— read it back ———
curl -sf "$BASE/b/e2e-bundle/essays/one.md" > "$WORK/got.md"
printf '# One\n\nFirst version.\n' | cmp - "$WORK/got.md" || fail "raw .md does not match the published source byte for byte"
note "raw .md byte-identical"
curl -sf "$BASE/b/e2e-bundle/essays/one" > "$WORK/view.html"
grep -q "First version." "$WORK/view.html" || fail "reading view lacks the text"
grep -q "Version 1 · (Your Version)" "$WORK/view.html" || fail "reading view lacks the version line"
grep -q "edit.phext.io" "$WORK/view.html" && fail "an individual page must not carry the suggestion"
curl -sf "$BASE/b/e2e-bundle/" > "$WORK/index.html"
grep -q "This is where the end-to-end bundle begins." "$WORK/index.html" || fail "index does not render the bundle's own index.md"
grep -q "edit.phext.io" "$WORK/index.html" || fail "index lacks the quiet suggestion"
grep -q "secret" "$WORK/index.html" && fail "index lists the vault"
[ "$(curl -s -o /dev/null -w '%{http_code}' "$BASE/b/e2e-bundle/vault/secret-note.md")" = "404" ] \
  || fail "the vault is served"
curl -sf "$BASE/b/e2e-bundle/feed.xml" > "$WORK/feed.xml"
grep -q '<rss version="2.0">' "$WORK/feed.xml" || fail "feed is not RSS 2.0"
grep -q "First release" "$WORK/feed.xml" || fail "feed lacks the change line"
grep -q "secret" "$WORK/feed.xml" && fail "feed lists the vault"
curl -sf "$BASE/b/e2e-bundle.zip" > "$WORK/dl.zip"
cmp "$WORK/bundle-v1.zip" "$WORK/dl.zip" || fail "downloaded bundle differs from the published zip"
note "index, reading view, feed, vault privacy, download: all as designed"

# ——— wrong key, then the right one ———
CODE=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/api/publish" -H "X-Slug: e2e-bundle" \
  -H "X-Edit-Key: not-the-key" --data-binary @"$WORK/bundle-v2.zip")
[ "$CODE" = "403" ] || fail "wrong key answered $CODE, not 403"
curl -sf "$BASE/b/e2e-bundle/essays/one.md" | grep -q "First version." || fail "a refused publish changed the bundle"
RESP2=$(curl -sf -X POST "$BASE/api/publish" -H "X-Slug: e2e-bundle" -H "X-Edit-Key: $KEY" \
  -H "X-Changed: Second release" --data-binary @"$WORK/bundle-v2.zip")
V2=$(printf '%s' "$RESP2" | python3 -c "import json,sys; print(json.load(sys.stdin)['version'])")
URL2=$(printf '%s' "$RESP2" | python3 -c "import json,sys; print(json.load(sys.stdin)['url'])")
[ "$V2" = "2" ] || fail "re-publish version is $V2, not 2"
[ "$URL2" = "/b/e2e-bundle/" ] || fail "re-publish moved the URL: $URL2"
curl -sf "$BASE/b/e2e-bundle/essays/one.md" | grep -q "Second version." || fail "re-publish did not swap the content"
note "wrong key refused; re-publish swapped the bundle at the same URL, version 2"

# ——— restore, both ways ———
curl -sf -X POST "$BASE/api/restore" -H "X-Slug: e2e-bundle" -H "X-Edit-Key: $KEY" > /dev/null
curl -sf "$BASE/b/e2e-bundle/essays/one.md" | grep -q "First version." || fail "restore did not bring v1 back"
curl -sf -X POST "$BASE/api/restore" -H "X-Slug: e2e-bundle" -H "X-Edit-Key: $KEY" > /dev/null
curl -sf "$BASE/b/e2e-bundle/essays/one.md" | grep -q "Second version." || fail "second restore did not return to v2"
note "restore swaps the two versions cleanly"

# ——— the editor, in a real browser ———
if [ -x "$CHROME_BIN" ]; then
  "$CHROME_BIN" --no-sandbox --disable-gpu --hide-scrollbars \
    --user-data-dir="$WORK/profile-selftest" --virtual-time-budget=8000 \
    --window-size=1600,1000 \
    --dump-dom "$BASE/?selftest=1" > "$WORK/selftest.html" 2>/dev/null
  grep -q "SELFTEST PASS" "$WORK/selftest.html" || { grep -o "FAIL [^<]*" "$WORK/selftest.html" >&2 || true; fail "editor self-test failed"; }
  note "editor self-test passed (seed, vault, silent rename, three states, resumption, hook, filter, count, link, theme)"

  "$CHROME_BIN" --no-sandbox --disable-gpu --hide-scrollbars \
    --user-data-dir="$WORK/profile-first" --virtual-time-budget=8000 \
    --window-size=1600,1000 --screenshot="$SHOTS/01-first-screen.png" "$BASE/" 2>/dev/null
  "$CHROME_BIN" --no-sandbox --disable-gpu --hide-scrollbars \
    --user-data-dir="$WORK/profile-first" --virtual-time-budget=8000 \
    --window-size=1600,1000 \
    --dump-dom "$BASE/" > "$WORK/first.html" 2>/dev/null
  python3 - "$WORK/first.html" <<'PYEOF' || fail "first screen text check failed"
import re, sys, html
raw = open(sys.argv[1]).read()
raw = re.sub(r"<script.*?</script>", "", raw, flags=re.S)
raw = re.sub(r"<style.*?</style>", "", raw, flags=re.S)
text = html.unescape(re.sub(r"<[^>]+>", " ", raw))
assert "Untitled" not in text, "first screen says Untitled"
assert "0 words" not in text, "first screen shows a zero counter"
assert "Dear reader," in text, "first screen lacks the seeded letter"
PYEOF
  note "seeded first screen clean: no Untitled, no zero counter"

  "$CHROME_BIN" --no-sandbox --disable-gpu --hide-scrollbars \
    --user-data-dir="$WORK/profile-demo" --virtual-time-budget=10000 \
    --window-size=1600,1000 --screenshot="$SHOTS/02-editor.png" "$BASE/?demo" 2>/dev/null
  "$CHROME_BIN" --no-sandbox --disable-gpu --hide-scrollbars \
    --window-size=1200,1000 --screenshot="$SHOTS/03-published-page.png" "$BASE/b/e2e-bundle/essays/one" 2>/dev/null
  "$CHROME_BIN" --no-sandbox --disable-gpu --hide-scrollbars \
    --window-size=1200,1000 --screenshot="$SHOTS/04-bundle-index.png" "$BASE/b/e2e-bundle/" 2>/dev/null
  "$CHROME_BIN" --no-sandbox --disable-gpu --hide-scrollbars \
    --user-data-dir="$WORK/profile-release" --virtual-time-budget=8000 \
    --window-size=1200,1000 --screenshot="$SHOTS/05-release.png" "$BASE/?demo&release=garden-notes" 2>/dev/null
  note "screenshots in $SHOTS"

  # ——— theme legs: an emulated OS, a stored choice, both surfaces ———
  # The headless shell's CLI flags cannot emulate prefers-color-scheme,
  # so these legs drive the browser over CDP (scripts/cdp.mjs).
  if command -v node > /dev/null; then
    node scripts/cdp.mjs "$CHROME_BIN" "$BASE" "$WORK" "$SHOTS" \
      || fail "theme legs failed"
    note "theme legs passed (dark/light OS × stored choice, editor + published page)"
  else
    note "no node on PATH — CDP theme legs skipped"
  fi
else
  note "no headless chrome at $CHROME_BIN — browser legs skipped"
fi

echo "E2E OK"
