//! Markdown rendering and the published pages.
//!
//! One renderer for everything: the editor's Cloth face and the
//! public reading view both call `render_markdown`, so what an author
//! sees while writing is exactly what a reader gets. (phext-lattice's
//! editor likewise renders server-side from one code path.)
//!
//! Two editorial rules, enforced in the event stream:
//!
//! * Raw HTML in a markdown file is shown as text, never executed.
//!   Markdown is the language here; a published page must not be able
//!   to script itself under this domain.
//! * A relative link to another markdown file (`other.md`,
//!   `docs/deep.md#part`) is rewritten to its reading-view address
//!   (extension dropped), so a bundle's internal links walk the
//!   reading view. The raw `.md` — the pattern — is always one step
//!   away: every reading page names it in its footer.
//!
//! The pages are printed pages: one serif, one measure, one accent
//! (deep indigo), hairline rules. A reading page's footer carries the
//! file's address, its bundle, the version line, and the pattern
//! line — nothing else, and in particular no suggestion; the quiet
//! word for edit.phext.io lives on the bundle index and in the
//! editor, nowhere else.

use pulldown_cmark::{CowStr, Event, Options, Parser, Tag, TagEnd};

use crate::bundle::{bundle_title, incipit, is_root_index, markdown_stem, ordered_files, BundleFile};
use crate::http::html_escape;
use crate::rss::long_date;
use crate::store::Bundle;

fn options() -> Options {
    Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES
}

/// A file may end with a next-line hook — `<!-- next: … -->`, a note
/// from the writer to themselves. It belongs to the pattern (it stays
/// in the raw `.md`, and the editor puts the caret on it at reopen)
/// but it is not cloth: strip it before rendering, here, so the
/// preview and the published page agree.
fn strip_hook(md: &str) -> &str {
    let trimmed = md.trim_end();
    if let Some(start) = trimmed.rfind("<!--") {
        let tail = &trimmed[start..];
        let on_own_line = start == 0 || trimmed.as_bytes()[start - 1] == b'\n';
        if on_own_line
            && tail.starts_with("<!-- next:")
            && tail.ends_with("-->")
            && !tail[..tail.len() - 3].contains('\n')
        {
            return trimmed[..start].trim_end();
        }
    }
    md
}

/// Render markdown to an HTML fragment.
pub fn render_markdown(md: &str) -> String {
    let md = strip_hook(md);
    let parser = Parser::new_ext(md, options());
    let filtered = parser.map(|event| match event {
        // Raw HTML becomes visible text.
        Event::Html(html) => Event::Text(html),
        Event::InlineHtml(html) => Event::Text(html),
        // Internal markdown links point at the reading view.
        Event::Start(Tag::Link {
            link_type,
            dest_url,
            title,
            id,
        }) => {
            let rewritten = rewrite_internal_link(&dest_url);
            let dest_url = if rewritten == dest_url.as_ref() {
                dest_url
            } else {
                CowStr::Boxed(rewritten.into_boxed_str())
            };
            Event::Start(Tag::Link {
                link_type,
                dest_url,
                title,
                id,
            })
        }
        Event::End(TagEnd::Link) => Event::End(TagEnd::Link),
        other => other,
    });
    let mut out: Vec<u8> = Vec::new();
    pulldown_cmark::html::write_html_io(&mut out, filtered).expect("write to a Vec cannot fail");
    String::from_utf8(out).expect("the HTML writer emits valid UTF-8")
}

/// `docs/other.md` → `docs/other`; `a.md#sec` → `a#sec`. Absolute
/// URLs, site-absolute paths, and pure fragments pass through.
fn rewrite_internal_link(dest: &str) -> String {
    if dest.starts_with('#') || dest.starts_with('/') || dest.contains("://") {
        return dest.to_string();
    }
    let (path, frag) = match dest.split_once('#') {
        Some((p, f)) => (p, Some(f)),
        None => (dest, None),
    };
    match markdown_stem(path) {
        Some(stem) => match frag {
            Some(f) => format!("{stem}#{f}"),
            None => stem,
        },
        None => dest.to_string(),
    }
}

/// The first `# heading` of a markdown text, for page titles.
pub fn first_heading(md: &str) -> Option<String> {
    for line in md.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("# ") {
            let title = rest.trim().trim_end_matches('#').trim();
            if !title.is_empty() {
                return Some(title.to_string());
            }
        }
    }
    None
}

/// Remove a leading `<h1>…</h1>` from a rendered fragment — the page
/// already sets the title large above its rule, and a markdown file
/// that opens with its own H1 would otherwise say it twice.
fn strip_leading_h1(rendered: &str) -> String {
    let trimmed = rendered.trim_start();
    if trimmed.starts_with("<h1>") {
        if let Some(end) = trimmed.find("</h1>") {
            return trimmed[end + 5..].trim_start_matches('\n').to_string();
        }
    }
    rendered.to_string()
}

/// Theme resolution, run before first paint on every published
/// page — the same snippet the editor carries in its head. The
/// reader's stored choice ("mdwiz.theme", set by the editor's
/// toggle on this origin) wins; otherwise the OS preference;
/// otherwise light. The stylesheet below also carries a no-JS
/// fallback keyed on the media query alone.
pub const THEME_SNIPPET: &str = r#"(function () {
  var t = null;
  try { t = localStorage.getItem("mdwiz.theme"); } catch (e) {}
  if (t !== "light" && t !== "dark") {
    t = (window.matchMedia && matchMedia("(prefers-color-scheme: dark)").matches) ? "dark" : "light";
  }
  document.documentElement.setAttribute("data-theme", t);
})();"#;

/// The shared stylesheet: quiet, editorial, paper and ink with one
/// deep-indigo accent. System serif for display and body, system sans
/// for the small chrome, mono for addresses. No webfonts, no
/// requests — the page is the whole of it. Dark is the same printed
/// page at night: same serif, the one accent lifted, paper gone
/// deep and warm, ink gone soft.
pub const SITE_CSS: &str = r#"
:root {
  --paper: #f7f6f1;
  --card: #fffefb;
  --ink: #20211c;
  --soft: #565a52;
  --faint: #8b8f85;
  --line: #dcdacd;
  --accent: #3c3f94;
  --accent-soft: #eceef7;
  --code-bg: #22242a;
  --serif: "Iowan Old Style", "Palatino Linotype", Palatino, Georgia, serif;
  --sans: -apple-system, BlinkMacSystemFont, "Segoe UI", Helvetica, Arial, sans-serif;
  --mono: ui-monospace, "SF Mono", Menlo, Consolas, monospace;
  color-scheme: light;
}
:root[data-theme="dark"] {
  --paper: #1a1915;
  --card: #23221c;
  --ink: #eae7db;
  --soft: #b7b4a7;
  --faint: #918e80;
  --line: #3b3a30;
  --accent: #969ae9;
  --accent-soft: #262741;
  --code-bg: #101014;
  color-scheme: dark;
}
@media (prefers-color-scheme: dark) {
  :root:not([data-theme]) {
    --paper: #1a1915;
    --card: #23221c;
    --ink: #eae7db;
    --soft: #b7b4a7;
    --faint: #918e80;
    --line: #3b3a30;
    --accent: #969ae9;
    --accent-soft: #262741;
    --code-bg: #101014;
    color-scheme: dark;
  }
}
* { box-sizing: border-box; }
body {
  margin: 0; background: var(--paper); color: var(--ink);
  font-family: var(--sans); font-size: 16px; line-height: 1.55;
}
.page { max-width: 44rem; margin: 0 auto; padding: 3.5rem 1.25rem 4rem; }
.eyebrow {
  font-size: 0.72rem; letter-spacing: 0.14em; text-transform: uppercase;
  color: var(--faint); margin: 0 0 0.6rem;
}
.title-block { border-bottom: 1px solid var(--ink); padding-bottom: 1rem; margin-bottom: 1.8rem; }
.title-block.keeper { display: flex; align-items: center; gap: 1.1rem; }
.keeper-badge {
  width: 52px; height: 52px; flex: none; border-radius: 50%;
  object-fit: cover; border: 1px solid var(--line); background: #f4ecd9;
}
h1.display {
  font-family: var(--serif); font-weight: 600; font-size: 2.4rem;
  line-height: 1.15; margin: 0;
}
.lede { color: var(--soft); margin: 0 0 2rem; font-family: var(--serif); font-size: 1.1rem; }
article {
  font-family: var(--serif); font-size: 1.1875rem; line-height: 1.7;
}
article h1, article h2, article h3, article h4 {
  font-family: var(--serif); line-height: 1.25; margin: 1.6em 0 0.5em;
}
article h1 { font-size: 1.75rem; }
article h2 { font-size: 1.45rem; }
article h3 { font-size: 1.2rem; }
article p { margin: 1em 0; }
article a { color: var(--accent); }
article code {
  font-family: var(--mono); font-size: 0.84em;
  background: var(--accent-soft); padding: 0.1em 0.35em; border-radius: 4px;
}
article pre {
  background: var(--code-bg); color: #eef0ea; border-radius: 8px;
  padding: 1rem 1.1rem; overflow-x: auto; font-size: 0.92rem; line-height: 1.5;
}
article pre code { background: none; padding: 0; color: inherit; }
article blockquote {
  margin: 1em 0; padding: 0.2em 1em; border-left: 3px solid var(--accent);
  color: var(--soft);
}
article table { border-collapse: collapse; margin: 1em 0; font-size: 0.95em; }
article th, article td { border: 1px solid var(--line); padding: 0.35em 0.7em; }
article img { max-width: 100%; }
.era {
  font-family: var(--serif); font-style: italic; font-size: 1.05rem;
  color: var(--soft); margin: 1.8rem 0 0.4rem;
}
.tracklist { list-style: none; margin: 0; padding: 0; counter-reset: track; }
.tracklist li {
  display: flex; gap: 0.9rem; align-items: baseline;
  padding: 0.55rem 0; border-bottom: 1px solid var(--line);
}
.tracklist .no {
  font-family: var(--mono); font-size: 0.8rem; color: var(--faint);
  min-width: 1.6rem; text-align: right;
}
.tracklist .t { font-family: var(--serif); font-size: 1.08rem; }
.tracklist a { color: var(--ink); text-decoration: none; }
.tracklist a:hover { color: var(--accent); text-decoration: underline; }
.tracklist .path {
  display: block; font-family: var(--mono); font-size: 0.78rem; color: var(--faint);
}
.tracklist .path a { color: var(--faint); }
.feed-line {
  margin: 2.2rem 0 0; padding: 1rem 0 0; border-top: 1px solid var(--line);
  font-family: var(--serif); font-size: 1.05rem;
}
.feed-line a { color: var(--accent); }
.prevnext {
  display: flex; justify-content: space-between; gap: 2rem;
  margin-top: 3rem; padding-top: 1.2rem; border-top: 1px solid var(--line);
  font-family: var(--serif); font-size: 1rem;
}
.prevnext a { color: var(--accent); text-decoration: none; }
.prevnext a:hover { text-decoration: underline; }
.prevnext .next { margin-left: auto; text-align: right; }
footer.colophon {
  margin-top: 2.6rem; padding-top: 1.1rem; border-top: 1px solid var(--line);
  color: var(--faint); font-size: 0.84rem; line-height: 1.6;
}
footer.colophon a { color: var(--soft); }
footer.colophon .addr { font-family: var(--mono); font-size: 0.78rem; }
.crumbs { font-size: 0.84rem; color: var(--faint); margin: 0 0 1.6rem; font-family: var(--serif); font-style: italic; }
.crumbs a { color: var(--accent); text-decoration: none; }
"#;

/// The quiet suggestion — it lives in exactly two places: the
/// editor's footer and the bundle index page's footer.
pub const QUIET_LINE: &str = "Flat files are a fine start. When you want \
addresses in eleven dimensions, try <a href=\"https://edit.phext.io\">edit.phext.io</a>.";

/// `Version 3 · (Your Version) · updated 15 November 2026`, plus the
/// release's change line when one was given.
pub fn version_line(bundle: &Bundle) -> String {
    let mut line = format!(
        "Version {} · (Your Version) · updated {}",
        bundle.version,
        long_date(bundle.published_at)
    );
    if let Some(changed) = &bundle.changed {
        line.push_str(&format!(" · {}", html_escape(changed)));
    }
    line
}

fn shell(title: &str, body: &str) -> String {
    format!(
        "<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>{}</title>\n<link rel=\"icon\" type=\"image/png\" href=\"/favicon.png\">\n\
         <script>{THEME_SNIPPET}</script>\n\
         <style>{SITE_CSS}</style>\n</head>\n<body>\n<div class=\"page\">\n{body}\n</div>\n</body>\n</html>\n",
        html_escape(title)
    )
}

/// The bundle index: `/b/<slug>/`. A tracklist under the bundle's own
/// self-description — the site is generated from the bundle alone.
pub fn index_page(bundle: &Bundle, base: &str) -> String {
    let slug = html_escape(&bundle.slug);
    let title = bundle_title(&bundle.parsed, &bundle.slug);

    // The bundle describes itself: its root index, rendered.
    let mut intro = String::new();
    if let Some(root) = bundle
        .parsed
        .files
        .iter()
        .find(|f| is_root_index(&f.path))
    {
        if let Some(text) = root.text() {
            let rendered = strip_leading_h1(&render_markdown(text));
            intro = format!("<article>{rendered}</article>");
        }
    }

    // The tracklist: bundle order, numbered, grouped by era (the
    // top-level folder a file lives in; root files are the opening
    // group and carry no era name).
    let mut list = String::new();
    let mut current_era: Option<String> = None;
    let mut n = 0usize;
    for f in ordered_files(&bundle.parsed) {
        if !f.is_markdown {
            continue;
        }
        let era = f.path.split('/').next().filter(|_| f.path.contains('/')).map(|s| s.to_string());
        if era != current_era {
            if let Some(e) = &era {
                list.push_str(&format!("<p class=\"era\">{}</p>", html_escape(e)));
            }
            current_era = era;
        }
        n += 1;
        let stem = markdown_stem(&f.path).unwrap_or_else(|| f.path.clone());
        let label = f
            .text()
            .and_then(incipit)
            .unwrap_or_else(|| f.path.clone());
        list.push_str(&format!(
            "<li><span class=\"no\">{n}</span><span class=\"t\"><a href=\"{}\">{}</a>\
             <span class=\"path\">{} · <a href=\"{}.md\">the pattern</a></span></span></li>",
            html_escape(&stem),
            html_escape(&label),
            html_escape(&f.path),
            html_escape(&stem),
        ));
    }

    let body = format!(
        "<div class=\"title-block keeper\"><img class=\"keeper-badge\" src=\"/mascot-badge.webp\" alt=\"\"><div><p class=\"eyebrow\">A bundle of markdown</p>\
         <h1 class=\"display\">{}</h1></div></div>\n\
         {intro}\n\
         <ol class=\"tracklist\">{list}</ol>\n\
         <p class=\"feed-line\">This bundle has a feed: <a href=\"feed.xml\">RSS</a> — \
         every release, in your reader, at <span style=\"font-family:var(--mono);font-size:0.85em\">{base}/b/{slug}/feed.xml</span>.</p>\n\
         <footer class=\"colophon\"><p>{}</p><p>{QUIET_LINE}</p>\
         <p>Published from a zip of markdown — the bundle is the document, and \
         <a href=\"/b/{slug}.zip\">the zip itself</a> is here to take away.</p></footer>",
        html_escape(&title),
        version_line(bundle),
    );
    shell(&title, &body)
}

/// One file's reading view: `/b/<slug>/<path>` (extension dropped).
/// A printed page: title, rule, body, neighbours, colophon.
pub fn reading_page(bundle: &Bundle, file: &BundleFile) -> String {
    let md = file.text().unwrap_or("");
    let rendered = strip_leading_h1(&render_markdown(md));
    let title = first_heading(md)
        .or_else(|| incipit(md))
        .unwrap_or_else(|| file.path.clone());
    let slug = html_escape(&bundle.slug);
    let stem = markdown_stem(&file.path).unwrap_or_else(|| file.path.clone());

    // Neighbours in bundle order, named by their first lines.
    let order: Vec<&BundleFile> = ordered_files(&bundle.parsed)
        .into_iter()
        .filter(|f| f.is_markdown)
        .collect();
    let pos = order.iter().position(|f| f.path == file.path);
    let mut nav = String::new();
    if let Some(pos) = pos {
        let prev = if pos > 0 {
            let p = order[pos - 1];
            let p_stem = markdown_stem(&p.path).unwrap_or_else(|| p.path.clone());
            let p_label = p.text().and_then(incipit).unwrap_or_else(|| p.path.clone());
            format!(
                "<span><a href=\"{}\">‹ Previous: {}</a></span>",
                html_escape(&p_stem),
                html_escape(&p_label)
            )
        } else {
            "<span></span>".to_string()
        };
        let next = if pos + 1 < order.len() {
            let nx = order[pos + 1];
            let n_stem = markdown_stem(&nx.path).unwrap_or_else(|| nx.path.clone());
            let n_label = nx.text().and_then(incipit).unwrap_or_else(|| nx.path.clone());
            format!(
                "<span class=\"next\"><a href=\"{}\">Next: {} ›</a></span>",
                html_escape(&n_stem),
                html_escape(&n_label)
            )
        } else {
            "<span></span>".to_string()
        };
        nav = format!("<nav class=\"prevnext\">{prev}{next}</nav>");
    }

    let body = format!(
        "<p class=\"crumbs\"><a href=\"/b/{slug}/\">‹ {slug}</a></p>\n\
         <div class=\"title-block\"><h1 class=\"display\">{}</h1></div>\n\
         <article>\n{rendered}</article>\n\
         {nav}\n\
         <footer class=\"colophon\">\
         <p><span class=\"addr\">/b/{slug}/{}</span> · from the bundle \
         <a href=\"/b/{slug}/\">{slug}</a></p>\
         <p>{}</p>\
         <p>This page is woven from plain markdown: \
         <a href=\"{}.md\">read the source</a> — the pattern, exactly as written.</p>\
         </footer>",
        html_escape(&title),
        html_escape(&file.path),
        version_line(bundle),
        html_escape(&stem),
    );
    shell(&title, &body)
}

/// The 404 page, in the same quiet dress.
pub fn not_found_page(what: &str) -> String {
    let body = format!(
        "<div class=\"title-block\"><p class=\"eyebrow\">404</p>\
         <h1 class=\"display\">Nothing at this address.</h1></div>\n\
         <p class=\"lede\">{} is not here. An address is only an address when something lives at it.</p>\n\
         <p><a href=\"/\">‹ Back to the editor</a></p>",
        html_escape(what)
    );
    shell("Not found — md.phext.io", &body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_basic_markdown() {
        let html = render_markdown("# Hi\n\nSome *text* and **more**.\n");
        assert!(html.contains("<h1>Hi</h1>"));
        assert!(html.contains("<em>text</em>"));
        assert!(html.contains("<strong>more</strong>"));
    }

    #[test]
    fn raw_html_is_text_not_markup() {
        let html = render_markdown("before\n\n<script>alert(1)</script>\n\nafter\n");
        assert!(!html.contains("<script>"));
        assert!(html.contains("&lt;script&gt;"));
    }

    #[test]
    fn internal_links_drop_the_extension() {
        let html = render_markdown("See [the guide](docs/guide.md) and [part](a.md#sec) and [out](https://x.test/a.md).\n");
        assert!(html.contains("href=\"docs/guide\""));
        assert!(html.contains("href=\"a#sec\""));
        assert!(html.contains("href=\"https://x.test/a.md\""));
    }

    #[test]
    fn headings_found() {
        assert_eq!(first_heading("text\n# Real Title\n").as_deref(), Some("Real Title"));
        assert_eq!(first_heading("## Not it\n"), None);
    }

    #[test]
    fn hook_is_not_rendered() {
        let html = render_markdown("# T\n\nBody.\n\n<!-- next: ask about the fence -->\n");
        assert!(html.contains("Body."));
        assert!(!html.contains("next:"));
        // A comment that is not a trailing hook stays visible as text.
        let html2 = render_markdown("<!-- a remark -->\n\nBody.\n");
        assert!(html2.contains("a remark"));
    }

    #[test]
    fn published_pages_resolve_the_theme_before_paint() {
        let page = not_found_page("x");
        // The reader's stored choice, under the one shared key, is
        // read by the pre-paint snippet on every published page…
        assert!(page.contains("mdwiz.theme"));
        assert!(page.contains("data-theme"));
        assert!(page.contains("prefers-color-scheme: dark"));
        // …and the stylesheet carries the dark palette plus the
        // no-JS media-query fallback.
        assert!(page.contains(":root[data-theme=\"dark\"]"));
        assert!(page.contains(":root:not([data-theme])"));
    }

    #[test]
    fn published_pages_carry_the_keeper_favicon() {
        let page = not_found_page("x");
        assert!(page.contains("<link rel=\"icon\" type=\"image/png\" href=\"/favicon.png\">"), "{page}");
    }

    #[test]
    fn leading_h1_stripped_only_at_the_top() {
        assert_eq!(strip_leading_h1("<h1>T</h1>\n<p>x</p>"), "<p>x</p>");
        assert_eq!(strip_leading_h1("<p>x</p><h1>T</h1>"), "<p>x</p><h1>T</h1>");
    }
}
