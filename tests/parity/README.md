# Workflow-parity tests — md.phext.io vs. Dave Winer's prototype

Dave Winer's prototype (markdown-publish-dw.davew.chatgpt.site) is a
"wizzy markdown editor with public storage": a document list, a
visual editor, drafts that stay private until publish, a stable URL
per document serving the raw markdown, and an RSS feed — with
writing gated behind a ChatGPT/OpenAI sign-in.

This suite proves, workflow by workflow, that each of those has a
replacement workflow here. Each Playwright test is named for the
prototype workflow it replaces (W1–W13) and proves the *outcome* in
a real browser against the real server binary. Where the models
differ by design — a zip of markdown files as the document instead
of a single document, the markdown source as the editor instead of
a visual mode, an edit key instead of an account — the test proves
the same outcome by this app's own road.

## Run it

```sh
scripts/parity.sh          # local instance, all write workflows
PARITY_LIVE=1 scripts/parity.sh   # + read-only smoke on md.phext.io
```

The script boots the release binary on a scratch port with a fresh
data directory, installs the suite's one dependency
(`@playwright/test`) on first run, and tears everything down after.
Write workflows never touch the live site; the live pass only loads
the editor shell.

## The map

| # | Prototype workflow | Replacement here | Verdict |
| --- | --- | --- | --- |
| W1 | Sidebar document list | Bundle file tree; paths are addresses | Covered |
| W2 | Document search | Sidebar fuzzy filter + Cmd/Ctrl-K jump | Covered |
| W3 | New document | "New file" in the bundle | Covered |
| W4 | Visual editor + toolbar (styles, bold/italic, link, lists, quote, undo/redo) | Markdown source + rendered Cloth face; status-bar insert operations (a note, an essay, a list, a link); textarea undo/redo | Covered, differently |
| W5 | Markdown-source mode | The Pattern face is the source; there is no other mode | Covered |
| W6 | Title field per document | The file's first line is its title (incipit) | Covered, differently |
| W7 | Live word + character count | Word count of the current selection only | **Gap** — no character count, no always-on document count |
| W8 | Save draft; private until publish | Autosave to the browser; nothing exists server-side until release; the vault is never served | Covered, differently |
| W9 | Publish | Release screen: slug + one button, atomic bundle swap | Covered |
| W10 | Stable URL serving the raw markdown | `/b/<slug>/<path>.md`, byte-exact, unchanged across re-publish | Covered |
| W11 | Rendered reading view | `/b/<slug>/<path>` pre-rendered page + bundle index | Covered |
| W12 | RSS 2.0 feed of published documents | `/b/<slug>/feed.xml`, one item per file, per-file dates | Covered |
| W13 | Writing gated by sign-in | No gate: local-first by default; an optional edit.phext.io sign-in (federation) keeps drafts in a phext vault; publishing's only credential is the bundle's edit key | Covered, differently — the gate is removed, the protection kept |
