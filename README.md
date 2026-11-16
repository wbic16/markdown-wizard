# markdown-wizard — md.phext.io

A wizzy markdown editor with public storage, built by stripping
phext to first principles.

Phext says: text is plain text; every text lives at an address; the
container is self-contained and opens anywhere. Strip the eleven
dimensions away and what remains is this:

- **The document is a zip of markdown files.** Plain text inside an
  archive — no lock-in, no database, no account. Open the zip in any
  tool, forever; it is the whole of the thing.
- **The addresses are the paths.** `essays/on-beauty.md` inside the
  bundle is that file's name, its sidebar entry, and — once
  published — its public URL.
- **Publishing is an atomic swap.** The whole bundle replaces the
  whole bundle, or nothing changes. There is no half-published state.

It exists because Dave Winer asked, in public, for exactly this
shape of tool — and because its bigger sibling,
[edit.phext.io](https://edit.phext.io), is what the same idea looks
like with all eleven dimensions left in. The editor carries one
quiet line about that, in its footer and nowhere it would nag.

The keeper of the place is the Pencil Puffball — a paper-white
puff in an indigo cap, pencil held like a staff. It stands in the
editor's brand block and beside each bundle's title, and it is
the favicon; the reading pages themselves stay clean print.

## Writing

Open `md.phext.io` and a file is already open — a new bundle is born
threaded with three files:

- `index.md` — the bundle's self-description, seeded with a letter
  you edit rather than a page you invent. Its first line is the
  bundle's title; the published index is generated from the bundle
  alone.
- `about.md` — one line: the short version of who you are and what
  this is.
- `scratch.md` — the pad by the door. `Cmd/Ctrl+.` from anywhere
  jumps to it and starts a fresh timestamped heading.

One text, two faces: **Pattern** (the markdown source) and **Cloth**
(the rendered page), side by side on wide screens, one toggle on
narrow ones. Scrolling either scrolls the other; the paragraph your
caret rests in is softly lit in the other face. There is no Save
button — the draft autosaves to the browser as an invariant, and
reopening restores the last file, its scroll, and its caret exactly.
The status bar inserts a note, an essay, a list — or a link, which
wraps the selection, taking its address from the clipboard when the
clipboard holds one.
A file may end with a next-line hook, `<!-- next: … -->`; reopening
it places the caret on the note.

Files are registers: `Cmd/Ctrl-K` fuzzy-jumps by path, `Cmd/Ctrl-1…9`
opens the first nine, `Cmd/Ctrl+Shift+F` is focus mode. The sidebar's
head carries a filter box — the jump's fuzzy match on path and first
line, narrowing the tree as you type — and the brand line keeps a
quiet count of the bundle's files. Deleting is
called **Put in the vault**: the file moves to the bundle's `vault/`
folder, browsable at the sidebar's foot, restorable in one click.
Nothing is ever destroyed, and the vault is never published or
served — like a draft, it is private by structure.

The page has a night setting. A quiet toggle in the editor's chrome
cycles **System → Light → Dark**: your choice is stored in the
browser and wins; with no choice stored, the OS preference
(`prefers-color-scheme`) decides, live; with neither, light. The
choice is one per origin — the published pages run the same
resolution before first paint, so a reader who chose dark in the
editor reads dark too. Dark is the same printed page at night: the
same serif, the one indigo accent lifted, paper gone deep and warm.

## Publishing

Release is one screen: the slug (set once, remembered), the full
public URL rendered large *before* you commit, an optional one-line
"What changed?", and one button.

- First release returns an **edit key** — shown once, saved in that
  browser. Re-publishing to the slug requires it; nobody else can
  overwrite your address. The server stores only the key's SHA-256.
- The URL never changes. Each re-publish increments the version:
  published pages carry `Version N · (Your Version) · updated <date>`
  and the change line, which also enters the feed.
- Feed items carry their own file's dates, not the release's:
  re-releasing a bundle for one file's sake does not re-date the
  rest. A file's first-published date is kept; its last-changed
  advances only when its content does.
- A feed item is a whole document, not a pointer: the full
  rendering in `content:encoded` (addresses absolutized), the
  complete markdown source in `source:markdown` — Winer's
  extension, so a post can be re-edited as markdown downstream —
  and a short plain excerpt as its description. Items are newest
  first, capped at 25 in a bundle's feed and 50 in the site-wide
  feed at `/rss.xml`; GUIDs are the items' own addresses, never
  their titles. Every published page carries the bundle feed's
  autodiscovery link, the feeds answer conditional GETs (a quiet
  poll costs a 304), and with `MDWIZ_WEBSUB_HUB` set, each
  publish also pings the hub.
- The previous version is kept. Restoring it is the same operation
  run with the old bytes, so a restore is itself reversible —
  reversibility replaces confirmation dialogs.
- The publish state is one word: **Drafting**, **Current**, or
  **Diverged**.

### The public URL scheme

| Address | What answers |
| --- | --- |
| `/` | The editor |
| `/integrate` | Ways to use it — your LLM (MCP), WordPress, your own hosting, the edit.phext.io door |
| `/mascot-badge.webp` | The Pencil Puffball's round badge — worn on `/integrate`, beside bundle titles |
| `/b/<slug>/` | The bundle index: its own `index.md`, rendered, over a numbered tracklist |
| `/b/<slug>/<path>.md` | The raw markdown itself — the source is the destination |
| `/b/<slug>/<path>` | The reading view of the same file (extension dropped) |
| `/b/<slug>/<path>.html` | The same reading view, pre-rendered at publish time as a static page |
| `/b/<slug>/<path>.pdf` | The same page as a PDF, printed at publish time |
| `/b/<slug>.zip` | The bundle itself, to take away |
| `/b/<slug>/site.zip` | The bundle as a ready-made static site, to host anywhere |
| `/b/<slug>/feed.xml` | The bundle's RSS 2.0 feed, from the published snapshot only |
| `/rss.xml` | The site-wide feed: every published bundle's items, newest first |
| `/b/<slug>/vault/…` | Never served. The vault is private by structure |
| `POST /mcp` | The MCP endpoint for agent clients (next section) |

Inside a bundle, a relative link to `other.md` walks the reading
view; every reading page names its own source in its footer ("read
the source — the pattern"). Raw HTML in a markdown file is shown as
text, never executed.

## Ways to use it

`/integrate` answers the new writer's next question — where a
bundle can go. Four doors: an LLM over MCP (`POST /mcp`, the
bundle's edit key carried in the header), WordPress (an
application password; posts updated in place on re-release), the
reader's own hosting (the bundle taken away as a deployable
static site), and sign-in with edit.phext.io, which keeps drafts
in a phext vault there. The editor's chrome and side-foot link
the page, and so does every bundle index's footer. The page
closes with one sentence of kin for [tluf.org](https://tluf.org)
and is kept by the Pencil Puffball — the house mascot, served at
`/mascot-badge.webp` and worn once, as a round badge, in the page's
opening.
## Outputs

Publishing produces one state — the bundle — and three carriers of
it, all built at publish time and cached beside the bundle (in
`<data>/<slug>/site/`), then rebuilt whole at every re-publish and
every restore:

- **Pre-rendered HTML.** Every markdown file's reading view,
  rendered once by the one renderer and kept as a static page at
  its `.html` address. It is the export-shaped page below — its
  links are relative — served from the same depth as the reading
  view, so they resolve identically in both homes.
- **PDF.** The same rendered page, printed to PDF at publish time
  and cached with the snapshot; a reader's request never triggers
  a render. Printing is the one job the server hands to an outside
  tool: a headless Chromium, found on the PATH (or in the
  Playwright cache, or via the `MARKDOWN_WIZARD_CHROMIUM`
  environment variable). When no Chromium exists, the publish
  still lands — the bundle simply has no PDFs, and each `.pdf`
  address answers with a page that says so. See `deploy/DEPLOY.md`.
- **The static site.** The whole bundle as a deployable tree, at
  `/b/<slug>/site.zip`: the bundle index as `index.html`, one
  `.html` and one `.pdf` per file, the raw `.md` sources at their
  own paths, every carried file, the feed, the source zip as
  `bundle.zip`, and a `README.txt` that tells the owner what they
  are holding. Links inside the exported pages are rewritten to
  relative paths that resolve inside the tree, so the site works
  from any static host — an S3 website bucket is the named
  example — with no markdown-wizard server anywhere in the loop.

The vault is in none of them: it is private by structure, and a
derived carrier inherits the privacy of its source. Two collision
rules, both mirroring the live routes: a carried file sitting at a
page's `.html` or `.pdf` address keeps that address (no generated
page is written over it), and the site's own furniture —
`index.html`, `feed.xml`, `favicon.svg`, `bundle.zip`,
`README.txt` — shadows a carried file of the same name.
## Agents (MCP)

An LLM client can write here too. The server speaks the Model
Context Protocol at `POST /mcp` — the same native MCP surface
edit.phext.io runs (phext-lattice PR #41), distilled to this
server's scale: JSON-RPC over a single POST, `initialize`,
`tools/list`, `tools/call`, batches answered as arrays,
notifications answered with silence. POST is the whole transport;
a GET probe gets a JSON 405 that says so.

The bundle is the unit, exactly as on the web; files are the
addresses inside it. Six tools:

| Tool | What it does |
| --- | --- |
| `list_bundles` | The slugs this server holds, with version, file count, and whether a working copy exists |
| `list_files` | A bundle's file tree — paths, sizes, markdown flags (pass `draft: true` with the edit key to see the working copy instead) |
| `read_file` | One file's text, from the published bundle or (`draft: true`) the working copy |
| `search_bundle` | Case-insensitive substring search across a bundle's files, returning path + matching lines |
| `write_file` | Create or replace one file in the slug's working copy |
| `publish_bundle` | Commit the working copy — the same atomic publish as the web flow, with the same "What changed?" line |

**The working copy.** The web editor is local-first: a human's
drafts live in their browser until release. An agent has no
browser, so its drafts live server-side, one working copy per slug,
under `<data>/<slug>/work/`. The first `write_file` to a slug opens
the copy — seeded from the published bundle, or empty for a fresh
slug — and every write lands there, persisted with the store's own
durability profile. Nothing an agent writes is public until
`publish_bundle`, and a publish is the store's ordinary commit:
agent edits and human edits meet at that one commit point, never
in a half-published state. The copy remembers which published
version it was seeded from (its *base*); if the bundle has moved
since — a human re-published meanwhile — the publish is refused as
stale rather than overwriting the newer release, and the draft is
kept. After a successful publish the copy re-bases onto the new
version, so an agent session can keep writing.

**Auth.** The bundle's edit key is the credential, the same one
the web flow issues. `write_file` and `publish_bundle` require it;
so do reads of a working copy (`draft: true`). It travels as the
`X-Edit-Key` header on the MCP request, or as an `edit_key`
argument on the individual call — the argument wins. Reads of
published bundles are public, matching the site's own posture, and
the vault is excluded from them entirely: a vault path answers
exactly as a missing file does. One consequence of the model: the
first write to a *fresh* slug is what claims it, so that write's
result carries the newly minted edit key — shown once, like the
web's first release. Keep it; every later write needs it. A wrong
key is a clean tool error (`isError`), never a panic and never a
partial write.

A minimal session, by hand:

```
curl -X POST https://md.phext.io/mcp -H 'Content-Type: application/json' -d '{
  "jsonrpc": "2.0", "id": 1, "method": "tools/call",
  "params": {"name": "write_file", "arguments": {
    "slug": "field-notes", "path": "index.md",
    "content": "# Field Notes\n\nFirst entry.\n"}}}'
# → the result carries the slug's edit_key, once. Then, with it:
curl -X POST https://md.phext.io/mcp -H 'Content-Type: application/json' \
  -H 'X-Edit-Key: <the key>' -d '{
  "jsonrpc": "2.0", "id": 2, "method": "tools/call",
  "params": {"name": "publish_bundle", "arguments": {
    "slug": "field-notes", "changed": "First entry"}}}'
```

A generic MCP client config (streamable HTTP, key in the header):

```json
{
  "mcpServers": {
    "markdown-wizard": {
      "url": "https://md.phext.io/mcp",
      "headers": { "X-Edit-Key": "<your bundle's edit key>" }
    }
  }
}
```

## WordPress

A bundle can also live on a self-hosted WordPress site. The
connection is per bundle, held by whoever holds the bundle's edit
key, and set from the release screen (or `POST /api/wordpress` with
the key): the site's address, a username, and an application
password.

The mechanism is the simplest door WordPress offers, per its own
developer docs: a push to the site's core REST API. On every
publish, each markdown file's rendered HTML is POSTed to
`/wp-json/wp/v2/posts` — title from the file's own first line, slug
from its path (`essays/one.md` becomes the post slug `essays-one`).
The returned post ID is stored in a per-bundle mapping beside the
bundle, so a re-publish updates the same posts in place: same
permalink, no duplicates. The mapping is written as each post is
answered for, so a crash mid-sync cannot duplicate a post on the
next run. A file that leaves the bundle is not deleted on
WordPress — its mapping is dropped and its post is left as it is.

The bundle stays the source of truth: the push runs after the local
commit, and a WordPress failure never fails a publish. The per-file
result (created / updated / failed, with the site's own error) is
recorded with the bundle, returned with the publish result, and
shown on the release screen, which offers a retry that re-pushes
without re-publishing (`POST /api/wordpress/sync`).

Prerequisites, all on the WordPress side:

- WordPress >= 5.6 on HTTPS (application passwords only work over
  HTTPS, or on a site WordPress treats as local). For the same
  reason this server accepts an `http://` site address only when
  the site is on this machine.
- A user who can publish — Editor or Administrator, recommended.
  WordPress filters post HTML by the sender's capabilities: an
  Author's posts are silently stripped of some markup (the
  `unfiltered_html` capability is the line), so an Author-level
  connection can lose formatting without any error anywhere.
- An application password for that user, minted once in their
  profile (Users → Profile → Application Passwords) and pasted into
  the release screen. No plugin, no registration, no plan.

The password is a credential: it is stored server-side only, in the
bundle's own directory with owner-only permissions, and is never
returned by any API (reads report only that one is set), never
logged, and never part of the bundle, its download, or its feed.

WordPress.com-hosted sites are not supported by this build: their
door is an OAuth2 sign-in, not an application password, and a
wordpress.com address is refused at save time with that said
plainly.

## Your vault on edit.phext.io

Sign-in is a door, never a gate. Everything above works signed
out, and local drafts are never held hostage to an account. The
editor's chrome carries a quiet **Sign in with edit.phext.io**;
signed in, it becomes **Your vault on edit.phext.io**, and your
drafts gain a second home: a phext on edit.phext.io, written
under your own identity.

**How sign-in works.** OAuth 2.1 authorization code + PKCE
(S256) against edit.phext.io's authorization server, scopes
`phext:read phext:write`. There is no client secret and nothing
to register by hand: the server registers itself through the
provider's dynamic client registration the first time anyone
signs in, and caches the client id beside its state. It is a
public client — the PKCE verifier is the credential, which is
also the flow the provider's own tests exercise. The access and
refresh tokens never reach the browser: this server holds them
(in `<data>/fed/state.json`, mode 0600) behind its own HttpOnly
session cookie, and calls the provider's MCP endpoint (`/mcp`:
`list_phexts`, `read_scroll`, `write_scroll`) with the user's
own token. The vault is the *user's* phext under the *user's*
sign-in — never a service account's.

**The vault.** One phext per user, named
`mdwiz-vault-<ten random characters>` — recognisably ours,
collision-free, and the discovery filter: signing in from a new
browser lists the hosted `mdwiz-vault-*` phexts and offers yours
by the label you gave it. Inside, a scroll is a page:

| Address | Holds |
|---|---|
| `1.1.1/1.1.1/1.1.1` | the vault manifest: label, and one line per bundle |
| `1.2.n/1.1.1/1.1.1` | bundle *n*'s manifest: one line per file — revision, SHA-256, path |
| `2.1.n/1.1.m/1.1.1` | file *m* of bundle *n*: the markdown itself |

Each manifest is one contiguous scroll, as a phext manifest
should be. A vault begins life as a one-scroll phext file — the
manifest itself — which the editor serves as a download
(**Download `<name>.phext`** in the vault screen); the user hosts
it at edit.phext.io/submit (or an operator places it), because
the provider's MCP surface has no create-phext call. Until then
the vault is in its first-class *not hosted yet* state, which
the editor explains rather than errors on.

**The sync model, plainly.** Signing in opens the vault screen
with three doors: save this browser's bundle into the vault,
bring a vault bundle into this browser, or leave both side by
side — the vault holds every bundle; a browser opens one at a
time, and adopting one saves the open one first when it has a
name. While signed in, edits save through to the vault a few
seconds behind the local autosave, and the chrome shows the
quiet state beside *Saved ✓* — *In your vault ✓*. Sync is
per-file, by revision and SHA-256 against the last synced
state:

- only the local copy changed → it is written;
- only the vault copy changed → a pull offers it, a push leaves
  it and says so;
- **both changed → a conflict, never a silent overwrite.** The
  file waits in the vault screen as a choice: *keep mine* /
  *take the vault's*;
- a local delete clears the vault copy only if the vault copy
  has not moved since the last sync; otherwise that too is a
  conflict, and a pull never resurrects a file this browser
  deleted on purpose.

Text files only: a non-text file in a bundle is reported as not
carried, never silently dropped. The editor's local put-away
vault (`vault/` inside a bundle) is a different thing, keeps
its name, and is never synced.

**Off switches.** `MDWIZ_FEDERATION=off` removes the door
entirely (the editor is then exactly the local-first tool);
`MDWIZ_PHEXT_ISSUER` points the flow at another issuer (a
staging edit.phext.io) and defaults to `https://edit.phext.io`.

## Cloud sync — S3, Azure, Google Cloud

A bundle can also live in object storage its author owns — the
"deploy on your own" route, made automatic. One engine serves
three providers: **Amazon S3**, **Microsoft Azure Blob Storage**,
and **Google Cloud Storage**. The connection is per bundle, held
by whoever holds the bundle's edit key, and set from the release
screen (or `POST /api/cloud` with the key): the provider, its
bucket or container, an optional prefix, and its credential.

On every publish, the bundle's deployable tree — the same tree
the static-site download carries: the rendered pages, the PDFs,
the raw sources at their paths, the feed, the bundle zip — is
copied to the destination under the prefix, with the content
types a static host serves (`text/html`, `application/pdf`,
`text/markdown` for the sources, `application/rss+xml` for the
feed). Unchanged files are skipped: the server keeps the SHA-256
it last uploaded successfully for every path, beside the bundle,
keyed to the destination — switching provider, bucket, container,
or prefix starts the new destination unconfirmed. A file that
leaves the bundle is not deleted from the destination; its copy
stays.

The bundle stays the source of truth: the sync runs after the
local commit, and a provider failure never fails a publish. The
per-file result (uploaded / unchanged / failed, with the
provider's own error) is recorded with the bundle, returned with
the publish result, and shown on the release screen, which offers
a retry that re-syncs without re-publishing
(`POST /api/cloud/sync`).

The transports are each provider's own first door, over `ureq`,
the tree's one HTTP client:

- **S3** — PUT Object, path-style, signed with Signature Version
  4 (implemented in `src/cloud.rs` over the `sha2` the service
  already carries, proven against AWS's published signing test
  vector). An optional endpoint field serves S3-compatible stores
  with the same code path.
- **Google Cloud Storage** — the XML API with HMAC keys, which is
  deliberately S3-shaped: the same signer, the same path-style
  PUT, credential scope region `auto`, against
  storage.googleapis.com (or a set endpoint).
- **Azure Blob** — PUT Blob against the Blob service REST API,
  authorized with a Shared Key signature (HMAC-SHA256 over
  Azure's canonical string-to-sign, the account key base64-decoded
  first). With an endpoint set, addressing is path-style (the
  Azurite shape); without one, the account's own host.

Prerequisites, all on the owner's side:

- A bucket (S3, GCS) or a storage account and container (Azure).
- A credential that may write: an IAM key with `s3:PutObject` on
  the prefix (S3), a Cloud Storage HMAC key for the bucket (GCS),
  or the storage account's key (Azure). The sync only ever writes
  objects.
- If the destination should serve the site, website hosting
  configured on it with the provider: nothing here sets or touches
  its ACL, policy, or website configuration.

The credentials are stored server-side only, in the bundle's own
directory with owner-only permissions, and are never returned by
any API (reads name a key ID by its last four characters and
report only that a secret is set), never logged, and never part
of the bundle, its download, or its feed.

## What was distilled

This repo is a distillation of two of Will Bickford's, per his
direction — the smallest faithful extraction of each, not a fork.

**From [phext-lattice](https://github.com/wbic16/phext-lattice)**
(the edit.phext.io stack):

- The HTTP layer's shape, from `src/bin/phext_edit.rs` and
  `src/oauth.rs`: a std-only server — `TcpListener`, one thread per
  connection, hand-written HTTP/1.1 responses with
  `Connection: close`, strong SHA-256 ETags with `If-None-Match`
  revalidation, and the three cache classes (NoStore / Shell /
  Static).
- The security-header block and CSP from `oauth.rs`, verbatim but
  for one documented deviation (`img-src` also allows `data:` and
  `https:` so published pages can show linked images), plus its
  status-text table and `X-Forwarded-Proto` handling.
- Pages compiled into the binary and served from a route table, the
  way `web/heatmap.html` and the editor shell are.
- Raw-body POST APIs with header-carried metadata, after
  `/api/submissions`' `X-Filename` pattern: publish is
  `POST /api/publish` with `X-Slug`, `X-Edit-Key`, `X-Changed`, and
  the zip as the body.
- The MCP server, from phext-lattice PR #41's `handle_mcp` in the
  same file: the stateless POST-only JSON-RPC transport, version
  negotiation, tool results as JSON text content, errors as
  `isError` results rather than transport failures — re-pointed
  from scrolls to bundles and files. PR #42's OAuth 2.1 layer was
  deliberately *not* carried: at this scale the bundle's edit key
  is the credential, carried the way the web publish API already
  carries it.
- The test style: spawn the server on an ephemeral port and speak
  raw HTTP at it (`tests/http_e2e.rs`, `tests/mcp_e2e.rs`).
- The deploy shape: a systemd unit and an nginx vhost modeled on
  `deploy/phext-edit.service` and `deploy/nginx-edit.phext.io.conf`.

**From [holo-phext](https://github.com/wbic16/holo-phext)** (the
layer-2 protocol over phext — ACID multi-scroll transactions):

- **Atomicity by commit point.** A publish is applied iff the whole
  zip verifies in memory (every entry parsed, within caps) and the
  staged `bundle.zip` rename lands — the rename is the commit
  marker, as holo's is.
- **The durability profile**, holo's checkpoint sequence restated:
  stage in place, `sync_all`, rename, directory sync — then
  acknowledge.
- **Snapshot isolation.** Each bundle lives behind an `Arc`; a
  publish builds the new bundle completely and swaps one `Arc` under
  a short write lock. Readers see all of a version or none of it.
- **Absolute operations.** A publish is a whole-bundle replacement,
  so retries are idempotent and the startup loader simply re-reads
  committed state; a torn staging file is ignored, never applied.
- The previous-version slot is the same machinery read backwards:
  before a commit, the current state is *copied* (never renamed
  away) into `prev/`, so restore is a commit of the old bytes.

Holo-phext was distilled as patterns rather than consumed as a
dependency: it is a phext engine (tokio, a WAL of phext scrolls), and
this tool's whole premise is phext stripped away — there are no
scrolls here to transact over. What a zip of markdown needs from
layer 2 is its discipline, and the discipline is what was taken.

The dependency set is the whole of what's left: `zip` (reading
bundles), `pulldown-cmark` (one renderer behind both faces and the
published pages, so they can never diverge), `sha2` (edit keys,
ETags), `serde_json` (the MCP endpoint's JSON-RPC — the same
crate phext-lattice's MCP is built on), and `ureq` — the one HTTP
client, for the WordPress push and the cloud sync, which need
TLS; the server side stays std-only (phext-notepad's edit-sync
runs the same client).
The editor page is one self-contained HTML file; its zip
reader/writer is hand-rolled (stored on write, stored + deflate on
read) over the platform's own streams.

## Run it

```
cargo run --release -- --dir ./mdwiz-data --port 8090 --host 127.0.0.1
```

Then open `http://127.0.0.1:8090/`. Useful query strings on the
editor: `?demo` opens a sample bundle; `?release=<slug>` opens the
release screen; `?selftest=1` runs the in-page model tests.

## Test it

```
cargo test            # unit tests + the raw-HTTP cycle tests,
                      # including a scripted MCP agent session
scripts/e2e.sh        # the full run against the release binary:
                      # publish → byte-compare → re-publish → restore,
                      # the editor self-test in headless Chromium,
                      # the theme legs over CDP (an emulated OS × the
                      # stored choice, on both surfaces), an MCP agent
                      # session (write → publish → read back at the
                      # public addresses), and the
                      # screenshots in docs/screenshots/
scripts/parity.sh     # the workflow-parity suite (tests/parity/):
                      # every workflow on Dave Winer's prototype,
                      # proven against its replacement workflow
                      # here, driven by Playwright in a real browser
```

## Deploy it

`deploy/` carries the systemd unit, the nginx sites (bootstrap and
TLS), a one-run `install.sh`, and `DEPLOY.md` — prepared for awsbox
in the phext-edit pattern. Not deployed by this repo; the install is
a human's keystroke, as house rules require.

## License

MIT — see `LICENSE`.
