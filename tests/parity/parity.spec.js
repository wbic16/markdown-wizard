// Workflow-parity suite for md.phext.io.
//
// Each test is named for a workflow on Dave Winer's prototype
// (https://markdown-publish-dw.davew.chatgpt.site/) and proves the
// replacement workflow in this app, in a real browser, against the
// real server binary. Where the two models differ by design — a zip
// bundle instead of one document, markdown source instead of a
// visual editor, an edit key instead of an account — the test proves
// the OUTCOME the prototype's workflow delivers.
//
// Run via scripts/parity.sh (boots a local instance; the live site
// is never written to).

const { test, expect } = require('@playwright/test');

const BASE = process.env.PARITY_BASE_URL || 'http://127.0.0.1:18131';
const slug = (tag) =>
  `parity-${tag}-${Date.now().toString(36)}${Math.floor(Math.random() * 1e4)}`;

async function openEditor(page) {
  await page.goto('/');
  await expect(page.locator('#editor')).toBeVisible();
  await expect(page.locator('#sidebar .entry')).toHaveCount(3);
}

const entry = (page, path) => page.locator(`#sidebar .entry[data-path="${path}"]`);

// ————————————————————————————————————————————————————————————————————
// W1 — the prototype's sidebar document list ("YOUR DOCUMENTS").
// Ours: the bundle's files in a sidebar tree, paths as addresses.
// ————————————————————————————————————————————————————————————————————
test('W1 document list → the bundle file tree in the sidebar', async ({ page }) => {
  await openEditor(page);
  const paths = await page.locator('#sidebar .entry').evaluateAll((els) =>
    els.map((e) => e.getAttribute('data-path')).sort()
  );
  expect(paths).toEqual(['about.md', 'index.md', 'scratch.md']);
  await expect(page.locator('#fileCount')).toHaveText('3 files');
  // Each entry shows the file's first line as its title, path beneath.
  await expect(entry(page, 'index.md').locator('.inc')).toContainText('Dear reader,');
  await expect(entry(page, 'index.md').locator('.pth')).toHaveText('index.md');
});

// ————————————————————————————————————————————————————————————————————
// W2 — the prototype's document search.
// Ours: a fuzzy filter in the sidebar head, plus Cmd/Ctrl-K jump.
// ————————————————————————————————————————————————————————————————————
test('W2 document search → sidebar filter and Cmd-K jump', async ({ page }) => {
  await openEditor(page);
  await page.locator('#filterInput').fill('scratch');
  await expect(page.locator('#sidebar .entry')).toHaveCount(1);
  await expect(entry(page, 'scratch.md')).toBeVisible();
  await page.locator('#filterInput').fill('');
  await expect(page.locator('#sidebar .entry')).toHaveCount(3);

  await page.keyboard.press('Control+k');
  await expect(page.locator('#jumpInput')).toBeVisible();
  await page.locator('#jumpInput').fill('about');
  await expect(page.locator('#jumpList .jentry[data-path="about.md"]')).toBeVisible();
  await page.locator('#jumpInput').press('Enter');
  await expect(page.locator('#pathInput')).toHaveValue('about.md');
  await expect(entry(page, 'about.md')).toHaveClass(/on/);
});

// ————————————————————————————————————————————————————————————————————
// W3 — the prototype's "+ New document".
// Ours: "New file" adds a file to the bundle and opens it.
// ————————————————————————————————————————————————————————————————————
test('W3 new document → New file creates and opens a file', async ({ page }) => {
  await openEditor(page);
  await page.locator('#newFileBtn').click();
  await expect(entry(page, 'note.md')).toBeVisible();
  await expect(page.locator('#fileCount')).toHaveText('4 files');
  await expect(page.locator('#pathInput')).toHaveValue('note.md');
  await expect(page.locator('#editor')).toHaveValue(/^# Note\n/);
});

// ————————————————————————————————————————————————————————————————————
// W4 — the prototype's visual editor + formatting toolbar (H1–H3,
// bold, italic, link, unlink, lists, block quote, undo, redo).
// Ours: one markdown source; the same formatting outcomes land in
// the rendered face (Cloth), helped by status-bar insert operations
// (a note, an essay, a list, a link). Undo/redo are the textarea's.
// ————————————————————————————————————————————————————————————————————
test('W4 visual editor toolbar → source editing, insert operations, live rendered preview', async ({ page }) => {
  await openEditor(page);
  await page.locator('#newFileBtn').click();
  const editor = page.locator('#editor');
  await editor.fill(
    '# Heading One\n\nSome **bold** words and *italic* ones and [a link](https://example.com).\n\n' +
    '## Heading Two\n\n### Heading Three\n\n> A quoted line.\n\n- First item\n- Second item\n\n1. Numbered one\n2. Numbered two\n'
  );
  const cloth = page.locator('#cloth');
  await expect(cloth.locator('h1')).toHaveText('Heading One');
  await expect(cloth.locator('h2')).toHaveText('Heading Two');
  await expect(cloth.locator('h3')).toHaveText('Heading Three');
  await expect(cloth.locator('strong')).toHaveText('bold');
  await expect(cloth.locator('em')).toHaveText('italic');
  await expect(cloth.locator('a[href="https://example.com"]')).toHaveText('a link');
  await expect(cloth.locator('blockquote')).toContainText('A quoted line.');
  await expect(cloth.locator('ul li').first()).toHaveText('First item');
  await expect(cloth.locator('ol li').first()).toHaveText('Numbered one');

  // Insert operation: "a list" lays down a list skeleton at the caret.
  await editor.click();
  await page.keyboard.press('Control+End');
  await page.locator('button[data-structure="list"]').click();
  await expect.poll(() => editor.inputValue()).toContain('- First');

  // Insert operation: "a link" wraps the selection as [text](url).
  await editor.evaluate((el) => {
    const i = el.value.indexOf('Numbered');
    el.focus();
    el.setSelectionRange(i, i + 'Numbered'.length);
  });
  await page.locator('#linkBtn').click();
  await expect.poll(() => editor.inputValue()).toContain('[Numbered](url)');

  // Undo / redo: the platform textarea's own, as the prototype's
  // toolbar buttons drive its editor's.
  await editor.click();
  await page.keyboard.press('Control+End');
  await page.keyboard.type('Undo this sentence.');
  await expect.poll(() => editor.inputValue()).toContain('Undo this sentence.');
  await page.keyboard.press('Control+z');
  await expect.poll(() => editor.inputValue()).not.toContain('Undo this sentence.');
  await page.keyboard.press('Control+Shift+z');
  if (!(await editor.inputValue()).includes('Undo this sentence.')) {
    await page.keyboard.press('Control+y');
  }
  await expect.poll(() => editor.inputValue()).toContain('Undo this sentence.');
});

// ————————————————————————————————————————————————————————————————————
// W5 — the prototype's Markdown-source mode (a plain textarea).
// Ours: the Pattern face IS that textarea; the source is the file.
// ————————————————————————————————————————————————————————————————————
test('W5 markdown-source mode → the Pattern face is a plain textarea holding the exact source', async ({ page }) => {
  await openEditor(page);
  const editor = page.locator('#editor');
  const tag = await editor.evaluate((el) => el.tagName);
  expect(tag).toBe('TEXTAREA');
  const source = '# Exact\n\nThe source, *verbatim* — no rendering in between.\n';
  await editor.fill(source);
  await expect(editor).toHaveValue(source);
  // Both faces at once on a wide screen: source and rendered page.
  await expect(page.locator('#cloth')).toBeVisible();
  await expect(page.locator('#cloth em')).toHaveText('verbatim');
});

// ————————————————————————————————————————————————————————————————————
// W6 — the prototype's per-document title field.
// Ours: no separate title store — a file's first line is its title
// (its incipit), shown in the sidebar and used as its public name.
// ————————————————————————————————————————————————————————————————————
test('W6 title field → a file’s first line is its title', async ({ page }) => {
  await openEditor(page);
  await page.locator('#newFileBtn').click();
  await page.locator('#editor').fill('# My Shining Title\n\nBody text.\n');
  // A file still carrying its birth name (note.md) is quietly
  // renamed after its title: the title becomes the address, and the
  // sidebar shows the title as the entry's name.
  await expect(page.locator('#pathInput')).toHaveValue('my-shining-title.md');
  await expect(entry(page, 'my-shining-title.md').locator('.inc')).toHaveText('My Shining Title');
});

// ————————————————————————————————————————————————————————————————————
// W7 — the prototype's live "N words · N characters" counter.
// Ours, PARTIAL: selecting text shows its word count ("N in
// selection") in the status bar. There is no character count and
// no always-on document counter — recorded as a gap in the parity
// table; this test proves the part that exists.
// ————————————————————————————————————————————————————————————————————
test('W7 live word/character count → selection word count (partial: words only, on selection)', async ({ page }) => {
  await openEditor(page);
  const editor = page.locator('#editor');
  await editor.fill('one two three four five');
  await editor.click();
  await page.keyboard.press('Control+a');
  await expect(page.locator('#countWord')).toHaveText('5 in selection');
});

// ————————————————————————————————————————————————————————————————————
// W8 — the prototype's "Save draft": drafts stay private until
// publish. Ours: no Save button and no account — the draft autosaves
// to the browser, survives a reload, and nothing is public (the
// slug does not exist on the server) until a release.
// ————————————————————————————————————————————————————————————————————
test('W8 save draft, private until publish → autosaved local draft, nothing public', async ({ page, request }) => {
  const s = slug('draft');
  await openEditor(page);
  await page.locator('#newFileBtn').click();
  const marker = `PARITY-DRAFT-${Date.now()}`;
  await page.locator('#editor').fill(`# Parity Draft Note\n\n${marker}\n`);
  await expect(page.locator('#stateWord')).toHaveText('Drafting');
  // Autosave is debounced (300 ms): wait until the draft, under its
  // title-given name, is actually in the browser's storage.
  await page.waitForFunction(
    (m) => (localStorage.getItem('mdwiz.v1') || '').includes(m),
    marker
  );
  await page.reload();
  await expect(entry(page, 'parity-draft-note.md')).toBeVisible();
  await page.locator('#editor').click();
  await expect.poll(() => page.locator('#editor').inputValue()).toContain(marker);
  // Never released: the server has no such bundle at all.
  const raw = await request.get(`/b/${s}/note.md`);
  expect(raw.status()).toBe(404);
  const idx = await request.get(`/b/${s}/`);
  expect(idx.status()).toBe(404);
});

// ————————————————————————————————————————————————————————————————————
// W9 publish · W10 stable raw URL · W11 reading view · W12 RSS feed.
// One chain, as a writer lives it: release the bundle, read the file
// back at its public addresses, edit, re-release at the same URL.
// The vault rides along: a put-away file is never published.
// ————————————————————————————————————————————————————————————————————
test('W9–W12 publish → stable raw .md URL, reading view, RSS feed', async ({ page, request }) => {
  const s = slug('pub');
  await openEditor(page);

  // A file for the bundle, written in the editor. Titling it
  // renames it (note.md -> parity-page.md): the title is the address.
  await page.locator('#newFileBtn').click();
  const editor = page.locator('#editor');
  await editor.fill('# Parity Page\n\nThe parity test was here.\n');
  const source1 = await editor.inputValue();

  // Put scratch.md in the vault: private by structure, never served.
  await entry(page, 'scratch.md').click();
  await expect(page.locator('#pathInput')).toHaveValue('scratch.md');
  await page.locator('#vaultBtn').click();
  await expect(page.locator('#sidebar .entry')).toHaveCount(3);

  // W9 — publish: the Release screen, a slug, one button.
  await page.locator('#releaseBtn').click();
  await expect(page.locator('#slugInput')).toBeVisible();
  await page.locator('#slugInput').fill(s);
  await expect(page.locator('#slugNote')).toContainText('free');
  await page.locator('#changedInput').fill('Parity release one');
  await page.locator('#releaseGo').click();
  await expect(page.locator('#liveBlock')).toBeVisible();
  await expect(page.locator('#liveUrl')).toHaveText(`${BASE}/b/${s}/`);
  await expect(page.locator('#liveMeta')).toContainText('Version 1');
  // The browser's memory of the release (slug + edit key) lands via
  // the same debounced autosave as the draft: wait for it to be in
  // storage before navigating anywhere, as a careful user would.
  await page.waitForFunction(
    (sl) => {
      try { return JSON.parse(localStorage.getItem('mdwiz.v1')).slug === sl; }
      catch { return false; }
    },
    s
  );
  await page.locator('#releaseBack').click();
  await expect(page.locator('#stateWord')).toHaveText('Current');

  // W10 — the raw .md URL serves the source itself, byte for byte.
  const rawUrl = `/b/${s}/parity-page.md`;
  const raw1 = await request.get(rawUrl);
  expect(raw1.status()).toBe(200);
  expect((await raw1.body()).equals(Buffer.from(source1, 'utf8'))).toBe(true);
  // The vault file stayed home.
  expect((await request.get(`/b/${s}/scratch.md`)).status()).toBe(404);

  // W11 — the reading view: the same file, rendered, at the
  // extensionless address; and the bundle index over it all.
  const view = await page.goto(`/b/${s}/parity-page`);
  expect(view.status()).toBe(200);
  await expect(page.locator('h1')).toHaveText('Parity Page');
  await expect(page.locator('body')).toContainText('Version 1 · (Your Version)');
  await page.goto(`/b/${s}/`);
  await expect(page.locator('body')).toContainText('Dear reader,');
  await expect(page.locator('body')).toContainText('parity-page.md');

  // Edit and re-release: same URL, new bytes, version increments.
  await page.goto('/');
  await entry(page, 'parity-page.md').click();
  await editor.click();
  await page.keyboard.press('Control+End');
  await page.keyboard.type('\nEdited after release.\n');
  const source2 = await editor.inputValue();
  await page.locator('#releaseBtn').click();
  await expect(page.locator('#slugInput')).toHaveValue(s);
  await expect(page.locator('#releaseGo')).toHaveText(/Re-publish/);
  await page.locator('#changedInput').fill('Parity release two');
  await page.locator('#releaseGo').click();
  await expect(page.locator('#liveMeta')).toContainText('Version 2');
  const raw2 = await request.get(rawUrl);
  expect((await raw2.body()).equals(Buffer.from(source2, 'utf8'))).toBe(true);

  // W12 — the bundle's RSS 2.0 feed lists the published files, each
  // with its own date; the vault file is not in it.
  const feed = await request.get(`/b/${s}/feed.xml`);
  expect(feed.status()).toBe(200);
  const xml = await feed.text();
  expect(xml).toContain('<rss version="2.0"');
  expect(xml).toContain('<source:markdown>');
  expect(xml).toContain(`/b/${s}/parity-page`);
  expect(xml).toContain(`/b/${s}/about`);
  expect(xml).not.toContain('scratch');
  const dates = [...xml.matchAll(/<pubDate>([^<]+)<\/pubDate>/g)].map((m) => m[1]);
  expect(dates.length).toBeGreaterThanOrEqual(2);
  for (const d of dates) expect(Number.isNaN(Date.parse(d))).toBe(false);
});

// ————————————————————————————————————————————————————————————————————
// W13 — the prototype gates writing behind a ChatGPT/OpenAI
// sign-in. Ours: sign-in exists only as an optional door (drafts in
// an edit.phext.io vault, via federation); it is never a gate. The
// whole flow runs signed out; the only credential publishing needs
// is the bundle's edit key, issued by the first publish itself —
// and a wrong key is refused.
// ————————————————————————————————————————————————————————————————————
test('W13 sign-in gate → a door, never a gate; the edit key is the credential', async ({ page, browser }) => {
  const s = slug('key');
  await openEditor(page);
  // The account door is present in the chrome — and is never
  // touched in this test: everything below happens signed out.
  await expect(page.locator('#accountBtn')).toBeVisible();

  // The full create → publish flow, with no credential presented.
  await page.locator('#releaseBtn').click();
  await page.locator('#slugInput').fill(s);
  await page.locator('#releaseGo').click();
  await expect(page.locator('#liveMeta')).toContainText('Version 1');

  // A different browser (no saved key) is asked for the key, and a
  // wrong one is refused: the key, not an account, opens the slug.
  const ctx2 = await browser.newContext({ baseURL: BASE });
  const page2 = await ctx2.newPage();
  await page2.goto(`/?release=${s}`);
  await expect(page2.locator('#keyInput')).toBeVisible();
  await page2.locator('#keyInput').fill('not-the-key');
  await page2.locator('#releaseGo').click();
  await expect(page2.locator('#releaseMsg')).toHaveClass(/err/);
  await expect(page2.locator('#releaseMsg')).toContainText(/key/i);
  await ctx2.close();
});
