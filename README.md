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
- The previous version is kept. Restoring it is the same operation
  run with the old bytes, so a restore is itself reversible —
  reversibility replaces confirmation dialogs.
- The publish state is one word: **Drafting**, **Current**, or
  **Diverged**.

### The public URL scheme

| Address | What answers |
| --- | --- |
| `/` | The editor |
| `/b/<slug>/` | The bundle index: its own `index.md`, rendered, over a numbered tracklist |
| `/b/<slug>/<path>.md` | The raw markdown itself — the source is the destination |
| `/b/<slug>/<path>` | The reading view of the same file (extension dropped) |
| `/b/<slug>.zip` | The bundle itself, to take away |
| `/b/<slug>/feed.xml` | The bundle's RSS 2.0 feed, from the published snapshot only |
| `/b/<slug>/vault/…` | Never served. The vault is private by structure |

Inside a bundle, a relative link to `other.md` walks the reading
view; every reading page names its own source in its footer ("read
the source — the pattern"). Raw HTML in a markdown file is shown as
text, never executed.

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
- The test style: spawn the server on an ephemeral port and speak
  raw HTTP at it (`tests/http_e2e.rs`).
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
ETags). The editor page is one self-contained HTML file; its zip
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
cargo test            # unit tests + the raw-HTTP cycle tests
scripts/e2e.sh        # the full run against the release binary:
                      # publish → byte-compare → re-publish → restore,
                      # the editor self-test in headless Chromium,
                      # the theme legs over CDP (an emulated OS × the
                      # stored choice, on both surfaces), and the
                      # screenshots in docs/screenshots/
```

## Deploy it

`deploy/` carries the systemd unit, the nginx sites (bootstrap and
TLS), a one-run `install.sh`, and `DEPLOY.md` — prepared for awsbox
in the phext-edit pattern. Not deployed by this repo; the install is
a human's keystroke, as house rules require.

## License

MIT — see `LICENSE`.
