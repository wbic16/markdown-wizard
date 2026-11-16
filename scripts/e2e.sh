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

# ——— the login rule: all of this happens signed out ———
# Nothing in this run has an account, a session, or a token of
# federation: /api/me must say so, and the publish above must
# have worked anyway. Signing in is a door, never a gate.
curl -sf "$BASE/api/me" | python3 -c "
import json, sys
me = json.load(sys.stdin)
assert me.get('signed_in') is False, 'the e2e walk was signed in'
" || fail "the signed-out walk could not be proven"
note "signed out throughout — and publishing worked anyway"

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
grep -q '<rss version="2.0"' "$WORK/feed.xml" || fail "feed is not RSS 2.0"
grep -q "First release" "$WORK/feed.xml" || fail "feed lacks the change line"
grep -q "secret" "$WORK/feed.xml" && fail "feed lists the vault"
curl -sf "$BASE/rss.xml" > "$WORK/site-feed.xml"
python3 - "$WORK/feed.xml" "$WORK/site-feed.xml" <<'PYEOF' || fail "the feeds do not validate as enriched XML"
import sys, xml.etree.ElementTree as ET

def local(tag):
    return tag.rsplit("}", 1)[-1]

def child(item, name):
    for c in item:
        if local(c.tag) == name:
            return c
    return None

tree = ET.parse(sys.argv[1])
root = tree.getroot()
assert root.tag == "rss", root.tag
assert root.get("version") == "2.0"
items = list(root.iter("item"))
assert items, "the bundle feed has no items"
one = None
for it in items:
    link = child(it, "link")
    if link is not None and link.text and link.text.endswith("/essays/one"):
        one = it
assert one is not None, "no item for essays/one"
src = child(one, "markdown")
assert src is not None, "the item lacks source:markdown"
assert (src.text or "") == "# One\n\nFirst version.\n", f"source:markdown is not the file byte for byte: {src.text!r}"
content = child(one, "encoded")
assert content is not None and "First version." in (content.text or ""), "content:encoded lacks the full text"
guid = child(one, "guid")
assert guid is not None and guid.text and guid.text.endswith("/b/e2e-bundle/essays/one"), "the GUID is not the item's path address"
for it in items:
    link = child(it, "link")
    assert link is not None and link.text and "://" in link.text, "an item link is not absolute"
    assert child(it, "pubDate") is not None, "an item lacks a pubDate"

site = ET.parse(sys.argv[2]).getroot()
assert site.tag == "rss", site.tag
site_items = list(site.iter("item"))
assert site_items, "the site-wide feed has no items"
assert any((child(it, "link").text or "").endswith("/b/e2e-bundle/essays/one") for it in site_items), "the site-wide feed lacks the bundle's items"
assert all("secret" not in (child(it, "link").text or "") for it in site_items), "the site-wide feed names the vault"
print("feeds validate: XML parses, source:markdown byte-exact, content full, GUIDs are addresses")
PYEOF
note "feeds parse as XML: full content, byte-exact source:markdown, path GUIDs, site-wide merge"
curl -sf "$BASE/b/e2e-bundle.zip" > "$WORK/dl.zip"
python3 - "$WORK/bundle-v1.zip" "$WORK/dl.zip" <<'PYEOF' || fail "the public download is not the published bundle with its vault left out"
import sys, zipfile
src = zipfile.ZipFile(sys.argv[1])
dl = zipfile.ZipFile(sys.argv[2])
expected = {n: src.read(n) for n in src.namelist() if not n.startswith("vault/") and not n.endswith("/")}
got = {n: dl.read(n) for n in dl.namelist() if not n.endswith("/")}
assert got == expected, f"public zip differs: {sorted(got)} vs {sorted(expected)}"
assert not any(n.startswith("vault/") for n in got), "the vault is in the public download"
PYEOF
note "index, reading view, feed, vault privacy, download: all as designed"

# ——— the keeper's assets ———
curl -sf "$BASE/mascot-badge.webp" > "$WORK/badge.webp"
python3 -c "
d = open('$WORK/badge.webp','rb').read()
assert d[:4] == b'RIFF' and d[8:12] == b'WEBP', 'badge is not a WebP'
"
curl -sf "$BASE/favicon.png" > "$WORK/favicon.png"
python3 -c "
d = open('$WORK/favicon.png','rb').read()
assert d[:4] == b'\x89PNG', 'favicon is not a PNG'
"
grep -q "mascot-badge" "$WORK/index.html" || fail "bundle index lacks the keeper"
grep -q "mascot-badge" "$WORK/view.html" && fail "a reading page carries the keeper"
curl -sf "$BASE/" > "$WORK/editor-brand.html"
grep -q "mascot-badge" "$WORK/editor-brand.html" || fail "editor brand lacks the keeper"
note "the keeper: badge and favicon serve; index and editor wear it; reading pages stay clean"
# ——— the derived outputs: pre-rendered page, PDF, static site ———
curl -sf "$BASE/b/e2e-bundle/essays/one.html" > "$WORK/one-static.html"
python3 - "$WORK/view.html" "$WORK/one-static.html" <<'PYEOF' || fail "the .html page is not the reading view's rendering"
import re, sys
def article(p):
    t = open(p).read()
    m = re.search(r"<article>\n?(.*?)</article>", t, re.S)
    assert m, f"no article in {p}"
    return m.group(1)
assert article(sys.argv[1]) == article(sys.argv[2]), "articles differ"
PYEOF
note "pre-rendered .html carries the reading view's own article"
if [ -x "$CHROME_BIN" ]; then
  curl -sf "$BASE/b/e2e-bundle/essays/one.pdf" > "$WORK/one.pdf" || fail "PDF address failed though a Chromium exists"
  [ "$(head -c 5 "$WORK/one.pdf")" = "%PDF-" ] || fail "the .pdf address does not serve PDF bytes"
  [ "$(wc -c < "$WORK/one.pdf")" -gt 1000 ] || fail "the PDF is a stub"
  note "PDF printed at publish time, real bytes"
else
  CODE=$(curl -s -o "$WORK/nopdf.html" -w '%{http_code}' "$BASE/b/e2e-bundle/essays/one.pdf")
  [ "$CODE" = "404" ] && grep -q "no Chromium" "$WORK/nopdf.html" || fail "no Chromium, but the .pdf address did not say so plainly"
  note "no Chromium here — the .pdf address degrades to the unavailable page, as designed"
fi
curl -sf "$BASE/b/e2e-bundle/site.zip" > "$WORK/site.zip"
python3 - "$WORK/site.zip" "$WORK/bundle-v1.zip" <<'PYEOF' || fail "the static site download is wrong"
import sys, zipfile, posixpath
z = zipfile.ZipFile(sys.argv[1])
names = set(z.namelist())
for expected in ["index.html", "about.html", "essays/one.html", "index.md",
                 "about.md", "essays/one.md", "feed.xml", "bundle.zip",
                 "README.txt", "favicon.svg", "favicon.png", "mascot-badge.webp"]:
    assert expected in names, f"site.zip lacks {expected}: {sorted(names)}"
assert not any(n.startswith("vault/") for n in names), f"vault in the site: {sorted(names)}"
import io
src = zipfile.ZipFile(sys.argv[2])
inner = zipfile.ZipFile(io.BytesIO(z.read("bundle.zip")))
expected = {n: src.read(n) for n in src.namelist() if not n.startswith("vault/") and not n.endswith("/")}
got = {n: inner.read(n) for n in inner.namelist() if not n.endswith("/")}
assert got == expected, "bundle.zip inside is not the published bundle with its vault left out"
import re
def check_links(page):
    html = z.read(page).decode("utf-8")
    base = posixpath.dirname(page)
    for href in re.findall(r'(?:href|src)="([^"]+)"', html):
        if href.startswith(("#", "mailto:", "data:")) or "://" in href:
            continue
        target = href.split("#")[0].split("?")[0]
        if not target:
            continue
        resolved = posixpath.normpath(posixpath.join(base, target))
        assert resolved in names, f"{page}: link {href} -> {resolved} not in the tree"
check_links("index.html")
check_links("essays/one.html")
PYEOF
note "static site: full tree, source zip inside, every internal link resolves in the tree"
# ——— wrong key, then the right one ———
CODE=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/api/publish" -H "X-Slug: e2e-bundle" \
  -H "X-Edit-Key: not-the-key" --data-binary @"$WORK/bundle-v2.zip")
[ "$CODE" = "403" ] || fail "wrong key answered $CODE, not 403"
curl -sf "$BASE/b/e2e-bundle/essays/one.md" > "$WORK/one-after-refusal.md"
grep -q "First version." "$WORK/one-after-refusal.md" || fail "a refused publish changed the bundle"
RESP2=$(curl -sf -X POST "$BASE/api/publish" -H "X-Slug: e2e-bundle" -H "X-Edit-Key: $KEY" \
  -H "X-Changed: Second release" --data-binary @"$WORK/bundle-v2.zip")
V2=$(printf '%s' "$RESP2" | python3 -c "import json,sys; print(json.load(sys.stdin)['version'])")
URL2=$(printf '%s' "$RESP2" | python3 -c "import json,sys; print(json.load(sys.stdin)['url'])")
[ "$V2" = "2" ] || fail "re-publish version is $V2, not 2"
[ "$URL2" = "/b/e2e-bundle/" ] || fail "re-publish moved the URL: $URL2"
curl -sf "$BASE/b/e2e-bundle/essays/one.md" > "$WORK/one-v2.md"
grep -q "Second version." "$WORK/one-v2.md" || fail "re-publish did not swap the content"
curl -sf "$BASE/b/e2e-bundle/essays/one.html" > "$WORK/one-v2.html"
grep -q "Second version." "$WORK/one-v2.html" || fail "re-publish did not rebuild the pre-rendered page"
grep -q "Version 2 · (Your Version)" "$WORK/one-v2.html" || fail "the rebuilt page lacks the new version line"
note "wrong key refused; re-publish swapped the bundle at the same URL, version 2"

# ——— restore, both ways ———
curl -sf -X POST "$BASE/api/restore" -H "X-Slug: e2e-bundle" -H "X-Edit-Key: $KEY" > /dev/null
curl -sf "$BASE/b/e2e-bundle/essays/one.md" > "$WORK/one-restored.md"
grep -q "First version." "$WORK/one-restored.md" || fail "restore did not bring v1 back"
curl -sf -X POST "$BASE/api/restore" -H "X-Slug: e2e-bundle" -H "X-Edit-Key: $KEY" > /dev/null
curl -sf "$BASE/b/e2e-bundle/essays/one.md" > "$WORK/one-restored2.md"
grep -q "Second version." "$WORK/one-restored2.md" || fail "second restore did not return to v2"
note "restore swaps the two versions cleanly"

# ——— the ways-to-use-it page ———
curl -sf "$BASE/integrate" > "$WORK/integrate.html"
grep -q "https://tluf.org" "$WORK/integrate.html" || fail "integrate page lacks the tluf.org link"
grep -q "md.phext.io/mcp" "$WORK/integrate.html" || fail "integrate page lacks the MCP endpoint"
grep -q "application password" "$WORK/integrate.html" || fail "integrate page lacks the WordPress steps"
curl -sf "$BASE/mascot-badge.webp" -o "$WORK/mascot-badge.webp" || fail "the mascot badge is not served"
head -c 4 "$WORK/mascot-badge.webp" | grep -q "RIFF" || fail "the mascot badge is not the webp"
curl -sf "$BASE/" > "$WORK/editor-link.html"
grep -q 'href="/integrate"' "$WORK/editor-link.html" || fail "the editor does not link the integrate page"
curl -sf "$BASE/b/e2e-bundle/" > "$WORK/bundle-index-link.html"
grep -q 'href="/integrate"' "$WORK/bundle-index-link.html" || fail "the bundle index does not link the integrate page"
note "integrate page served and linked: MCP, WordPress, hosting, tluf.org, the mascot"

# ——— the editor, in a real browser ———
if [ -x "$CHROME_BIN" ]; then
  "$CHROME_BIN" --no-sandbox --disable-gpu --hide-scrollbars \
    --user-data-dir="$WORK/profile-selftest" --virtual-time-budget=8000 \
    --window-size=1600,1000 \
    --dump-dom "$BASE/?selftest=1" > "$WORK/selftest.html" 2>/dev/null
  grep -q "SELFTEST PASS" "$WORK/selftest.html" || { grep -o "FAIL [^<]*" "$WORK/selftest.html" >&2 || true; fail "editor self-test failed"; }
  note "editor self-test passed (seed, vault, keep-private, silent rename, three states, resumption, hook, filter, count, link, theme, settings)"

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
  "$CHROME_BIN" --no-sandbox --disable-gpu --hide-scrollbars \
    --window-size=1200,1000 --screenshot="$SHOTS/10-integrate.png" "$BASE/integrate" 2>/dev/null
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

# ——— the MCP endpoint: an agent session, end to end ———
# The same endpoint an LLM client drives: initialize, list the
# tools, stream two writes into a working copy, refuse a wrong
# key, publish, and read the agent's words back at the public
# addresses.
MCP="$BASE/mcp"
mcp_rpc() { # mcp_rpc <json> [edit-key]
  if [ -n "${2:-}" ]; then
    curl -sf -X POST "$MCP" -H 'Content-Type: application/json' -H "X-Edit-Key: $2" -d "$1"
  else
    curl -sf -X POST "$MCP" -H 'Content-Type: application/json' -d "$1"
  fi
}
INIT=$(mcp_rpc '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"e2e","version":"0"}}}')
echo "$INIT" | grep -q '"markdown-wizard"' || fail "MCP initialize did not name the server: $INIT"
TOOLS=$(mcp_rpc '{"jsonrpc":"2.0","id":2,"method":"tools/list"}')
for t in list_bundles list_files read_file search_bundle write_file publish_bundle; do
  echo "$TOOLS" | grep -q "\"$t\"" || fail "MCP tools/list is missing $t"
done
W1=$(mcp_rpc '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"write_file","arguments":{"slug":"agent-bundle","path":"index.md","content":"# From the Agent\n\nStreamed in over MCP.\n"}}}')
AGENT_KEY=$(echo "$W1" | python3 -c 'import json,sys; print(json.loads(json.loads(sys.stdin.read())["result"]["content"][0]["text"])["edit_key"])')
[ -n "$AGENT_KEY" ] || fail "the first MCP write did not mint an edit key: $W1"
W2=$(mcp_rpc '{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"write_file","arguments":{"slug":"agent-bundle","path":"notes/deep.md","content":"# Deep Note\n\nThe agent was here.\n"}}}' "$AGENT_KEY")
echo "$W2" | grep -q '"isError":false' || fail "the second MCP write failed: $W2"
BAD=$(mcp_rpc '{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"write_file","arguments":{"slug":"agent-bundle","path":"index.md","content":"# Hijack\n","edit_key":"not-the-key"}}}')
echo "$BAD" | grep -q '"isError":true' || fail "a wrong-key MCP write was not refused: $BAD"
PUB=$(mcp_rpc '{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"publish_bundle","arguments":{"slug":"agent-bundle","changed":"Agent release"}}}' "$AGENT_KEY")
echo "$PUB" | grep -qF '\"version\": 1' || fail "the MCP publish did not land at version 1: $PUB"
printf '# Deep Note\n\nThe agent was here.\n' > "$WORK/agent-deep.md"
curl -sf "$BASE/b/agent-bundle/notes/deep.md" -o "$WORK/agent-deep-got.md" || fail "the agent's file is not public"
cmp "$WORK/agent-deep.md" "$WORK/agent-deep-got.md" || fail "the public bytes differ from the agent's write"
curl -sf "$BASE/b/agent-bundle/notes/deep" > "$WORK/agent-deep.html"
grep -q 'The agent was here.' "$WORK/agent-deep.html" || fail "the reading view lacks the agent's words"

# ——— federation surface: the door is visible signed out ———
# The full OAuth + vault flow runs against a local stub of
# edit.phext.io in tests/fed_e2e.rs; this leg only proves the
# deployment answers the editor's boot question honestly, keeps
# the vault endpoints behind the session, and shows the door.
ME=$(curl -sf "$BASE/api/me")
echo "$ME" | grep -q '"federation":true' || fail "federation is not on: $ME"
echo "$ME" | grep -q '"signed_in":false' || fail "a fresh visitor is somehow signed in: $ME"
CODE=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/api/vault")
[ "$CODE" = "401" ] || fail "the vault endpoint answered $CODE signed-out, not 401"
curl -sf "$BASE/" > "$WORK/editor.html"
grep -q 'id="accountBtn"' "$WORK/editor.html" || fail "the editor lacks the sign-in affordance"
note "federation surface: honest signed-out answers, vault behind the session, door visible"

echo "E2E OK"
