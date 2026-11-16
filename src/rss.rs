//! The per-bundle RSS 2.0 feed: `/b/<slug>/feed.xml`.
//!
//! Generated only from the published snapshot — drafts are private
//! by structure, so there is nothing else a feed could be made from.
//! One item per markdown file, in bundle order; the release's
//! "What changed?" line rides in each item's description, and the
//! version stamps the channel. Each item is dated by its own
//! file's last-changed stamp (see store.rs), so re-releasing a
//! bundle for one file's sake does not re-date the rest; the
//! channel's build date is the bundle's latest publish. Dates are
//! RFC 822 in GMT, converted
//! from Unix time by hand (the civil-date algorithm below) so the
//! service needs no date crate.

use crate::bundle::{incipit, is_vault_path};
use crate::http::xml_escape;
use crate::store::Bundle;

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
fn civil_from_days(z: i64) -> (i64, u32, u32) {
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

/// The bundle's feed. `base` is the site's public origin with no
/// trailing slash (e.g. `https://md.phext.io`); item links are built
/// from it so feed readers land on real addresses.
pub fn feed_xml(bundle: &Bundle, base: &str) -> String {
    let slug = &bundle.slug;
    let title = crate::bundle::bundle_title(&bundle.parsed, &bundle.slug);
    let mut items = String::new();
    for f in crate::bundle::ordered_files(&bundle.parsed) {
        if !f.is_markdown || is_vault_path(&f.path) {
            continue;
        }
        let stem = crate::bundle::markdown_stem(&f.path).unwrap_or_else(|| f.path.clone());
        let link = format!("{base}/b/{slug}/{stem}");
        let item_title = f
            .text()
            .and_then(incipit)
            .unwrap_or_else(|| f.path.clone());
        let mut desc = format!("{} — from the bundle {slug}.", f.path);
        if let Some(changed) = &bundle.changed {
            desc = format!("{changed} ({desc})");
        }
        items.push_str(&format!(
            "<item><title>{}</title><link>{}</link><guid isPermaLink=\"true\">{}</guid>\
             <pubDate>{}</pubDate><description>{}</description></item>\n",
            xml_escape(&item_title),
            xml_escape(&link),
            xml_escape(&link),
            rfc822(bundle.stamps_of(&f.path).1),
            xml_escape(&desc),
        ));
    }
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <rss version=\"2.0\">\n<channel>\n\
         <title>{}</title>\n<link>{base}/b/{slug}/</link>\n\
         <description>{} — a bundle of markdown, published whole. Version {}.</description>\n\
         <lastBuildDate>{}</lastBuildDate>\n{items}</channel>\n</rss>\n",
        xml_escape(&title),
        xml_escape(&title),
        bundle.version,
        rfc822(bundle.published_at),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_convert() {
        // 2026-11-15 22:20:00 UTC = 1794781200 (a Sunday).
        assert_eq!(rfc822(1794781200), "Sun, 15 Nov 2026 22:20:00 GMT");
        assert_eq!(long_date(1794781200), "15 November 2026");
        assert_eq!(rfc822(0), "Thu, 01 Jan 1970 00:00:00 GMT");
    }
}
