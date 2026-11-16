//! Feeds: the per-bundle RSS 2.0 feed at `/b/<slug>/feed.xml`, and
//! the site-wide feed at `/rss.xml` — every published bundle on the
//! instance, newest first. (Dave Winer's prototype has a site-wide
//! feed; under the Dave-governs rule his demonstrated want outranks
//! the market survey's refusal, so the site-wide feed ships.)
//!
//! Generated only from published snapshots — drafts are private by
//! structure, so there is nothing else a feed could be made from —
//! and vault files are excluded by the same rule that keeps them
//! unserved: the vault never appears in any feed, in any bundle.
//!
//! Each item is a whole document, not a pointer (the feed
//! enrichment of 2026-11-15, from the blogger survey's RSS
//! section):
//!
//! * `content:encoded` — the file rendered whole by the one
//!   renderer, its links and images absolutized against the item's
//!   own address: a feed reader has no base to resolve a relative
//!   address against, and relative addresses are the most commonly
//!   missed step in feed production.
//! * `source:markdown` — the file's complete markdown source, in
//!   Winer's extension element (namespace
//!   `http://source.scripting.com/` — the declaration Micro.blog
//!   ships by default and NetNewsWire 6.2 reads), so a post can be
//!   re-edited as markdown downstream. That is the pattern of
//!   Winer's own wpInbound bridge: the feed carries the source,
//!   not just the rendering.
//! * `description` — a short plain-text excerpt alongside, the
//!   reader's skim line; the release's "What changed?" line rides
//!   in the channel description, where release-level news belongs.
//!
//! The pitfalls discipline (the survey's list, treated as spec):
//! GUIDs are the items' own addresses — bundle slug + file path,
//! never the title, which derives from an editable first line and
//! changes under a reader. Dates are RFC 822 in GMT, converted
//! from Unix time by hand (the civil-date algorithm below) so the
//! service needs no date crate; each item is dated by its own
//! file's last-changed stamp (see store.rs), so re-releasing a
//! bundle for one file's sake does not re-date the rest. Every
//! payload is XML-escaped rather than CDATA-wrapped: one unescaped
//! `&` kills a feed silently for every subscriber, and a CDATA
//! section is one `]]>` in the source away from the same death —
//! escaped character data round-trips byte-exact through any real
//! XML parser. Items are newest first and capped — 25 in a bundle
//! feed, 50 in the site-wide feed: an item here carries full text
//! *and* full source, roughly triple an excerpt item's weight, and
//! readers re-poll the whole file, so the bundle feed sits at the
//! low end of the customary 20–50 range and the aggregate feed —
//! whose per-bundle feeds remain the complete record — at the top.
//! The routes answer conditional GETs with the stack's strong
//! ETags, so a poll that changed nothing costs a 304.
//!
//! WebSub: when a hub is configured (`MDWIZ_WEBSUB_HUB`), feeds
//! carry the hub and self `atom:link`s, and every commit pings the
//! hub for both changed feed addresses. The ping rides a detached
//! thread with the publish hooks' failure posture (WordPress,
//! cloud sync): it can never fail, block, or meaningfully delay a
//! publish — the commit has already landed before it is sent.

use std::sync::OnceLock;
use std::time::Duration;

use crate::bundle::{incipit, is_vault_path, markdown_stem, ordered_files};
use crate::http::xml_escape;
use crate::render::render_markdown;
use crate::store::Bundle;

/// Items in one bundle's feed.
pub const MAX_FEED_ITEMS: usize = 25;
/// Items in the site-wide feed.
pub const MAX_SITE_FEED_ITEMS: usize = 50;

const CONTENT_NS: &str = "http://purl.org/rss/1.0/modules/content/";
const ATOM_NS: &str = "http://www.w3.org/2005/Atom";
const SOURCE_NS: &str = "http://source.scripting.com/";

/// Format a Unix timestamp as an RFC 822 date in GMT:
/// `Sat, 15 Nov 2026 22:20:00 GMT`.
pub fn rfc822(unix: u64) -> String {
    let days = (unix / 86_400) as i64;
    let secs = unix % 86_400;
    let (y, m, d) = civil_from_days(days);
    // 1970-01-01 was a Thursday.
    let weekday = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"]
        [((days % 7 + 7) % 7) as usize];
    let month = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ][(m - 1) as usize];
    format!(
        "{weekday}, {d:02} {month} {y} {:02}:{:02}:{:02} GMT",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

/// Format a Unix timestamp as a long human date: `15 November 2026`.
pub fn long_date(unix: u64) -> String {
    let (y, m, d) = civil_from_days((unix / 86_400) as i64);
    let month = [
        "January", "February", "March", "April", "May", "June", "July", "August",
        "September", "October", "November", "December",
    ][(m - 1) as usize];
    format!("{d} {month} {y}")
}

/// Days since 1970-01-01 → (year, month, day). Howard Hinnant's
/// `civil_from_days` algorithm, public domain by design.
pub(crate) fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// The configured WebSub hub, if any: `MDWIZ_WEBSUB_HUB`, read
/// once at first use. Unset or empty means no hub — feeds carry
/// no hub link and commits ping nothing.
pub fn websub_hub() -> Option<&'static str> {
    static HUB: OnceLock<Option<String>> = OnceLock::new();
    HUB.get_or_init(|| {
        std::env::var("MDWIZ_WEBSUB_HUB")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    })
    .as_deref()
}

/// One feed item, built once from a published file and rendered
/// into either feed, so the two feeds can never disagree about
/// what an item is.
struct Item {
    title: String,
    /// The item's address — also its GUID: slug + path, stable
    /// across everything except the file actually moving.
    link: String,
    /// The file's last-changed stamp.
    published: u64,
    description: String,
    /// The file rendered whole, addresses absolutized.
    content_html: String,
    /// The file's complete markdown source, byte for byte.
    source_md: String,
}

fn items_of(bundle: &Bundle, base: &str) -> Vec<Item> {
    let slug = &bundle.slug;
    let mut items = Vec::new();
    for f in ordered_files(&bundle.parsed) {
        if !f.is_markdown || is_vault_path(&f.path) {
            continue;
        }
        let Some(text) = f.text() else { continue };
        let stem = markdown_stem(&f.path).unwrap_or_else(|| f.path.clone());
        let link = format!("{base}/b/{slug}/{stem}");
        let description = match excerpt(text) {
            d if !d.is_empty() => d,
            _ => format!("{} — from the bundle {slug}.", f.path),
        };
        items.push(Item {
            title: incipit(text).unwrap_or_else(|| f.path.clone()),
            published: bundle.stamps_of(&f.path).1,
            description,
            content_html: absolutize(&render_markdown(text), &link),
            source_md: text.to_string(),
            link,
        });
    }
    sort_items(&mut items);
    items
}

/// Newest first; the address breaks ties, so the order of a given
/// state is deterministic across rebuilds.
fn sort_items(items: &mut [Item]) {
    items.sort_by(|a, b| {
        b.published
            .cmp(&a.published)
            .then_with(|| a.link.cmp(&b.link))
    });
}

fn render_item(item: &Item) -> String {
    format!(
        "<item><title>{}</title><link>{}</link><guid isPermaLink=\"true\">{}</guid>\
         <pubDate>{}</pubDate><description>{}</description>\
         <content:encoded>{}</content:encoded><source:markdown>{}</source:markdown></item>\n",
        xml_escape(&item.title),
        xml_escape(&item.link),
        xml_escape(&item.link),
        rfc822(item.published),
        xml_escape(&item.description),
        xml_escape(&item.content_html),
        xml_escape(&item.source_md),
    )
}

fn render_feed(
    title: &str,
    link: &str,
    description: &str,
    last_build: u64,
    feed_url: &str,
    hub: Option<&str>,
    items: &[Item],
    cap: usize,
) -> String {
    let mut out = String::new();
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str(&format!(
        "<rss version=\"2.0\" xmlns:content=\"{CONTENT_NS}\" xmlns:atom=\"{ATOM_NS}\" xmlns:source=\"{SOURCE_NS}\">\n<channel>\n"
    ));
    out.push_str(&format!("<title>{}</title>\n", xml_escape(title)));
    out.push_str(&format!("<link>{}</link>\n", xml_escape(link)));
    out.push_str(&format!(
        "<description>{}</description>\n",
        xml_escape(description)
    ));
    if last_build > 0 {
        out.push_str(&format!(
            "<lastBuildDate>{}</lastBuildDate>\n",
            rfc822(last_build)
        ));
    }
    out.push_str(&format!(
        "<atom:link href=\"{}\" rel=\"self\" type=\"application/rss+xml\"/>\n",
        xml_escape(feed_url)
    ));
    if let Some(hub) = hub {
        out.push_str(&format!(
            "<atom:link href=\"{}\" rel=\"hub\"/>\n",
            xml_escape(hub)
        ));
    }
    for item in items.iter().take(cap) {
        out.push_str(&render_item(item));
    }
    out.push_str("</channel>\n</rss>\n");
    out
}

/// The bundle's feed. `base` is the site's public origin with no
/// trailing slash (e.g. `https://md.phext.io`); item links are
/// built from it so feed readers land on real addresses.
pub fn feed_xml(bundle: &Bundle, base: &str) -> String {
    feed_xml_with_hub(bundle, base, websub_hub())
}

fn feed_xml_with_hub(bundle: &Bundle, base: &str, hub: Option<&str>) -> String {
    let slug = &bundle.slug;
    let title = crate::bundle::bundle_title(&bundle.parsed, &bundle.slug);
    let mut channel_desc = format!(
        "{title} — a bundle of markdown, published whole. Version {}.",
        bundle.version
    );
    if let Some(changed) = &bundle.changed {
        channel_desc.push_str(&format!(" {changed}"));
    }
    render_feed(
        &title,
        &format!("{base}/b/{slug}/"),
        &channel_desc,
        bundle.published_at,
        &format!("{base}/b/{slug}/feed.xml"),
        hub,
        &items_of(bundle, base),
        MAX_FEED_ITEMS,
    )
}

/// The site-wide feed: the items of every published bundle on the
/// instance, newest first. It lives only on the live instance —
/// the static-site download is one bundle's artifact and carries
/// that bundle's own feed, never this one.
pub fn site_feed_xml(bundles: &[&Bundle], base: &str) -> String {
    site_feed_xml_with_hub(bundles, base, websub_hub())
}

fn site_feed_xml_with_hub(bundles: &[&Bundle], base: &str, hub: Option<&str>) -> String {
    let mut items: Vec<Item> = bundles.iter().flat_map(|b| items_of(b, base)).collect();
    sort_items(&mut items);
    let latest = bundles.iter().map(|b| b.published_at).max().unwrap_or(0);
    render_feed(
        "md.phext.io — every bundle",
        &format!("{base}/"),
        "Every bundle published here, newest first — each item whole: the rendering, and the markdown it was woven from.",
        latest,
        &format!("{base}/rss.xml"),
        hub,
        &items,
        MAX_SITE_FEED_ITEMS,
    )
}

/// Announce a commit to the WebSub hub, if one is configured: the
/// bundle's feed and the site-wide feed both changed. Called from
/// the store's commit point (publish, MCP publish, and restore
/// alike), after the commit has landed and the outputs are built.
/// A detached thread and a swallowed error: the hub's health is
/// never the publisher's problem.
pub fn ping_hub(bundle: &Bundle) {
    let Some(hub) = websub_hub() else { return };
    let hub = hub.to_string();
    let base = crate::outputs::CANONICAL_BASE;
    let feeds = [
        format!("{base}/b/{}/feed.xml", bundle.slug),
        format!("{base}/rss.xml"),
    ];
    std::thread::spawn(move || {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(5))
            .timeout_read(Duration::from_secs(10))
            .timeout_write(Duration::from_secs(10))
            .build();
        for feed in &feeds {
            if let Err(e) = agent
                .post(&hub)
                .send_form(&[("hub.mode", "publish"), ("hub.url", feed.as_str())])
            {
                eprintln!("markdown-wizard: websub ping for {feed} failed: {e}");
            }
        }
    });
}

/// A short plain-text description of a file: its opening prose
/// with the markdown furniture struck out, whitespace collapsed,
/// cut at a word boundary near 200 characters. The skim line that
/// rides alongside the full content — never instead of it.
fn excerpt(text: &str) -> String {
    let mut prose = String::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with("<!--") {
            continue; // blank, or furniture (the next-line hook)
        }
        let line = line.trim_start_matches('#').trim_start();
        let line = line.strip_prefix('>').map(str::trim_start).unwrap_or(line);
        prose.push_str(line);
        prose.push(' ');
        if prose.len() > 400 {
            break;
        }
    }
    let plain = plain_text(&prose);
    let collapsed = plain.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= 200 {
        return collapsed;
    }
    let cut: String = collapsed.chars().take(200).collect();
    match cut.rfind(' ') {
        Some(i) => format!("{}…", &cut[..i]),
        None => format!("{cut}…"),
    }
}

/// Strike the inline furniture from a line of prose: a link keeps
/// its words, an image keeps its alt text, emphasis marks and
/// code ticks go. Deliberately small — the description is a
/// summary, not a second renderer.
fn plain_text(md: &str) -> String {
    let chars: Vec<char> = md.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '[' || (c == '!' && i + 1 < chars.len() && chars[i + 1] == '[') {
            let open = if c == '!' { i + 1 } else { i };
            if let Some(close) = (open + 1..chars.len()).find(|&j| chars[j] == ']') {
                let label: String = chars[open + 1..close].iter().collect();
                let mut next = close + 1;
                if next < chars.len() && chars[next] == '(' {
                    if let Some(end) = (next + 1..chars.len()).find(|&j| chars[j] == ')') {
                        next = end + 1;
                    }
                }
                out.push_str(&label);
                i = next;
                continue;
            }
        }
        if !matches!(c, '*' | '`') {
            out.push(c);
        }
        i += 1;
    }
    out
}

/// Resolve every relative `href`/`src` in a rendered fragment
/// against the address of the page it belongs to, so the fragment
/// stands alone in a reader. External and fragment addresses pass
/// through; site-absolute paths gain the origin; the rest resolve
/// against the page's own directory, the way a browser reads them.
fn absolutize(html: &str, page_url: &str) -> String {
    let mut out = String::with_capacity(html.len() + 64);
    let mut rest = html;
    loop {
        let next_href = rest.find("href=\"");
        let next_src = rest.find("src=\"");
        let (pos, len) = match (next_href, next_src) {
            (Some(h), Some(s)) => {
                if h < s {
                    (h, 6)
                } else {
                    (s, 5)
                }
            }
            (Some(h), None) => (h, 6),
            (None, Some(s)) => (s, 5),
            (None, None) => break,
        };
        let val_start = pos + len;
        let Some(rel_end) = rest[val_start..].find('"') else {
            break;
        };
        let val_end = val_start + rel_end;
        out.push_str(&rest[..val_start]);
        out.push_str(&resolve_url(&rest[val_start..val_end], page_url));
        rest = &rest[val_end..];
    }
    out.push_str(rest);
    out
}

fn resolve_url(value: &str, page_url: &str) -> String {
    let cut = value.find(['#', '?']).unwrap_or(value.len());
    let (core, suffix) = value.split_at(cut);
    if core.is_empty()
        || core.contains("://")
        || core.starts_with("mailto:")
        || core.starts_with("data:")
    {
        return value.to_string();
    }
    if core.starts_with('/') {
        return format!("{}{core}{suffix}", origin_of(page_url));
    }
    let dir = match page_url.rfind('/') {
        Some(i) => &page_url[..i + 1],
        None => page_url,
    };
    format!("{}{suffix}", join_url(dir, core))
}

/// `scheme://authority` of an absolute URL.
fn origin_of(url: &str) -> &str {
    match url.find("://") {
        Some(i) => match url[i + 3..].find('/') {
            Some(j) => &url[..i + 3 + j],
            None => url,
        },
        None => "",
    }
}

/// Join a relative address onto a directory URL, normalizing `.`
/// and `..` segments the way a browser would — never above the
/// origin's root.
fn join_url(dir: &str, rel: &str) -> String {
    let origin = origin_of(dir);
    let path = &dir[origin.len()..];
    let mut segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    for seg in rel.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                segs.pop();
            }
            s => segs.push(s),
        }
    }
    if segs.is_empty() {
        format!("{origin}/")
    } else {
        format!("{origin}/{}", segs.join("/"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundle::{BundleFile, ParsedBundle};
    use std::collections::HashMap;

    #[test]
    fn dates_convert() {
        // 2026-11-15 22:20:00 UTC = 1794781200 (a Sunday).
        assert_eq!(rfc822(1794781200), "Sun, 15 Nov 2026 22:20:00 GMT");
        assert_eq!(long_date(1794781200), "15 November 2026");
        assert_eq!(rfc822(0), "Thu, 01 Jan 1970 00:00:00 GMT");
    }

    /// A published bundle in memory, the way the store builds one.
    fn bundle(slug: &str, files: &[(&str, &str)], stamps: &[(&str, u64)]) -> Bundle {
        Bundle {
            slug: slug.to_string(),
            parsed: ParsedBundle {
                zip_bytes: Vec::new(),
                files: files
                    .iter()
                    .map(|(path, text)| BundleFile {
                        path: path.to_string(),
                        bytes: text.as_bytes().to_vec(),
                        is_markdown: crate::bundle::is_markdown_path(path),
                    })
                    .collect(),
            },
            published_at: stamps.iter().map(|(_, t)| *t).max().unwrap_or(0),
            file_stamps: stamps
                .iter()
                .map(|(path, t)| {
                    (
                        path.to_string(),
                        crate::store::FileStamps {
                            first_published: *t,
                            last_changed: *t,
                        },
                    )
                })
                .collect::<HashMap<_, _>>(),
            version: 1,
            changed: None,
            key_sha256: String::new(),
        }
    }

    /// The text between two markers, XML-unescaped — what a feed
    /// reader's parser hands back.
    fn element(feed: &str, open: &str, close: &str) -> String {
        let start = feed.find(open).expect("element opens") + open.len();
        let end = feed[start..].find(close).expect("element closes") + start;
        unescape(&feed[start..end])
    }

    fn unescape(s: &str) -> String {
        s.replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&apos;", "'")
            .replace("&amp;", "&")
    }

    fn assert_no_bare_ampersands(feed: &str) {
        let bytes = feed.as_bytes();
        for (i, b) in bytes.iter().enumerate() {
            if *b == b'&' {
                let rest = &feed[i..];
                assert!(
                    ["&amp;", "&lt;", "&gt;", "&quot;", "&apos;"]
                        .iter()
                        .any(|e| rest.starts_with(e)),
                    "bare & at byte {i} in: {feed}"
                );
            }
        }
    }

    const POST: &str = "# One\n\nFirst paragraph, with AT&T and <angle> brackets.\n\nSee [two](two.md) and ![pic](pic.png).\n\nA middle paragraph that pads the post out to a length where a short description must stop long before the end of the text.\n\nAnother middle paragraph, because one was not quite enough padding to pass two hundred characters of prose.\n\nThe final paragraph, which an excerpt would drop.\n";

    #[test]
    fn items_carry_full_content_and_byte_exact_source() {
        let b = bundle(
            "notes",
            &[
                ("index.md", "# Home\n"),
                ("essays/one.md", POST),
                ("essays/two.md", "# Two\n"),
                ("essays/pic.png", "fakepng"),
                ("vault/secret.md", "# Hidden\n\nNever fed.\n"),
            ],
            &[("index.md", 100), ("essays/one.md", 300), ("essays/two.md", 200)],
        );
        let feed = feed_xml_with_hub(&b, "https://md.phext.io", None);
        assert_no_bare_ampersands(&feed);
        // The source rides whole and round-trips byte for byte.
        let source = element(&feed, "<source:markdown>", "</source:markdown>");
        // (The first item is the newest — essays/one itself.)
        assert_eq!(source, POST);
        // The content is the whole rendering, not an excerpt: the
        // final paragraph is present, and addresses are absolute.
        let content = element(&feed, "<content:encoded>", "</content:encoded>");
        assert!(content.contains("The final paragraph"), "{content}");
        // (In the HTML a reader receives, the text's own specials
        // ride as HTML entities — that layer is the renderer's.)
        assert!(content.contains("AT&amp;T and &lt;angle&gt; brackets."), "{content}");
        assert!(
            content.contains("href=\"https://md.phext.io/b/notes/essays/two\""),
            "{content}"
        );
        assert!(
            content.contains("src=\"https://md.phext.io/b/notes/essays/pic.png\""),
            "{content}"
        );
        assert!(!content.contains("href=\"two\""), "{content}");
        // The description is a short plain skim line: this item's
        // own, not the channel's.
        let item_start = feed.find("<item>").unwrap();
        let item_end = feed[item_start..].find("</item>").unwrap() + item_start;
        let desc = element(&feed[item_start..item_end], "<description>", "</description>");
        assert!(desc.chars().count() <= 201, "{desc}");
        assert!(!desc.contains("final paragraph"), "{desc}");
        assert!(!desc.contains("[two]"), "{desc}");
        assert!(desc.contains("See two and pic."), "{desc}");
        // The vault is nowhere: no item, no mention.
        assert!(!feed.contains("secret"), "{feed}");
        assert!(!feed.contains("Never fed."), "{feed}");
        // Newest first.
        let one = feed.find("essays/one").unwrap();
        let two = feed.find("essays/two").unwrap();
        assert!(one < two, "{feed}");
    }

    #[test]
    fn guid_is_the_path_never_the_title() {
        let v1 = bundle(
            "notes",
            &[("essays/one.md", "# One\n\nBody.\n")],
            &[("essays/one.md", 100)],
        );
        let v2 = bundle(
            "notes",
            &[("essays/one.md", "# One, Renamed\n\nBody.\n")],
            &[("essays/one.md", 100)],
        );
        let guid = |feed: &str| element(feed, "<guid isPermaLink=\"true\">", "</guid>");
        let f1 = feed_xml_with_hub(&v1, "https://md.phext.io", None);
        let f2 = feed_xml_with_hub(&v2, "https://md.phext.io", None);
        assert_eq!(guid(&f1), "https://md.phext.io/b/notes/essays/one");
        assert_eq!(guid(&f1), guid(&f2), "a retitle must not move the GUID");
        assert!(f2.contains("<title>One, Renamed</title>"), "{f2}");
    }

    #[test]
    fn bundle_feed_caps_at_25_newest() {
        let files: Vec<(String, String)> = (0..30)
            .map(|i| (format!("post-{i:02}.md"), format!("# Post {i}\n")))
            .collect();
        let stamps: Vec<(String, u64)> = (0..30)
            .map(|i| (format!("post-{i:02}.md"), 100 + i as u64))
            .collect();
        let b = bundle(
            "many",
            &files.iter().map(|(p, t)| (p.as_str(), t.as_str())).collect::<Vec<_>>(),
            &stamps.iter().map(|(p, t)| (p.as_str(), *t)).collect::<Vec<_>>(),
        );
        let feed = feed_xml_with_hub(&b, "https://md.phext.io", None);
        assert_eq!(feed.matches("<item>").count(), MAX_FEED_ITEMS);
        // The newest file leads; the sixth-oldest is past the cap.
        assert!(feed.contains("post-29"), "{feed}");
        assert!(!feed.contains("post-04"), "{feed}");
    }

    #[test]
    fn site_feed_merges_bundles_newest_first_and_caps() {
        let mk = |slug: &str, base_t: u64| {
            let files: Vec<(String, String)> = (0..30)
                .map(|i| (format!("p{i:02}.md"), format!("# {slug} {i}\n")))
                .collect();
            let stamps: Vec<(String, u64)> = (0..30)
                .map(|i| (format!("p{i:02}.md"), base_t + i as u64))
                .collect();
            bundle(
                slug,
                &files.iter().map(|(p, t)| (p.as_str(), t.as_str())).collect::<Vec<_>>(),
                &stamps.iter().map(|(p, t)| (p.as_str(), *t)).collect::<Vec<_>>(),
            )
        };
        let a = mk("aaa", 100);
        let b = mk("bbb", 200);
        let feed = site_feed_xml_with_hub(&[&a, &b], "https://md.phext.io", None);
        assert_eq!(feed.matches("<item>").count(), MAX_SITE_FEED_ITEMS);
        // bbb's newest is the site-wide newest and leads the feed.
        let first_item = feed.find("<item>").unwrap();
        let lead = &feed[first_item..];
        assert!(lead.contains("/b/bbb/p29"), "{lead}");
        // Bundle-qualified addresses: no two bundles' items collide.
        assert!(feed.contains("/b/aaa/p29"), "{feed}");
        assert!(!feed.contains("vault"), "{feed}");
    }

    #[test]
    fn hub_links_appear_only_with_a_hub() {
        let b = bundle("notes", &[("index.md", "# Home\n")], &[("index.md", 100)]);
        let plain = feed_xml_with_hub(&b, "https://md.phext.io", None);
        assert!(!plain.contains("rel=\"hub\""), "{plain}");
        assert!(plain.contains("rel=\"self\""), "{plain}");
        let hubbed = feed_xml_with_hub(&b, "https://md.phext.io", Some("https://hub.example/"));
        assert!(hubbed.contains("<atom:link href=\"https://hub.example/\" rel=\"hub\"/>"), "{hubbed}");
        assert!(
            hubbed.contains("href=\"https://md.phext.io/b/notes/feed.xml\" rel=\"self\""),
            "{hubbed}"
        );
    }

    #[test]
    fn absolutize_resolves_like_a_browser() {
        let page = "https://md.phext.io/b/notes/essays/one";
        assert_eq!(
            absolutize("<p><a href=\"two\">t</a></p>", page),
            "<p><a href=\"https://md.phext.io/b/notes/essays/two\">t</a></p>"
        );
        assert_eq!(
            absolutize("<img src=\"../img/p.png\">", page),
            "<img src=\"https://md.phext.io/b/notes/img/p.png\">"
        );
        assert_eq!(
            absolutize("<a href=\"/b/notes/x\">x</a>", page),
            "<a href=\"https://md.phext.io/b/notes/x\">x</a>"
        );
        assert_eq!(
            absolutize("<a href=\"#frag\">f</a>", page),
            "<a href=\"#frag\">f</a>"
        );
        assert_eq!(
            absolutize("<a href=\"https://elsewhere.test/a\">e</a>", page),
            "<a href=\"https://elsewhere.test/a\">e</a>"
        );
        assert_eq!(
            absolutize("<a href=\"two#part\">t</a>", page),
            "<a href=\"https://md.phext.io/b/notes/essays/two#part\">t</a>"
        );
    }

    #[test]
    fn excerpts_are_plain_and_short() {
        let e = excerpt("# Title\n\nSome *bold* words and [a link](x.md) here.\n");
        assert_eq!(e, "Title Some bold words and a link here.");
        let long = format!("# T\n\n{}\n", "word ".repeat(100));
        let e = excerpt(&long);
        assert!(e.chars().count() <= 201, "{e}");
        assert!(e.ends_with('…'), "{e}");
        assert_eq!(excerpt("<!-- next: later -->\n"), "");
    }
}
