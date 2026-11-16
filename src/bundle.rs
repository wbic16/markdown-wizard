//! Bundles: a zip of markdown files is the document.
//!
//! First principles, phext stripped down: the texts are plain text;
//! their addresses are their paths inside the archive; the archive is
//! self-contained and opens anywhere, in any tool, forever. This
//! module owns the rules that keep a bundle a bundle — path hygiene,
//! size caps, and slug rules — and the one parse that both the publish
//! path and the startup loader run, so a bundle means the same thing
//! in both places.

use std::io::{Cursor, Read};

use sha2::Digest;

/// Upload cap: the whole zip, as sent. Matches the nginx
/// `client_max_body_size 16M` in deploy/.
pub const MAX_ZIP_BYTES: usize = 16 * 1024 * 1024;
/// Per-file extracted cap.
pub const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;
/// Whole-bundle extracted cap (zip-bomb guard).
pub const MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024;
/// Entry-count cap.
pub const MAX_FILES: usize = 2000;

/// One file inside a bundle, extracted and held in memory. Readers
/// hold the whole bundle behind an `Arc` (see store.rs), so a file is
/// just bytes plus its address.
#[derive(Clone)]
pub struct BundleFile {
    /// The address: the path inside the zip, `/`-separated.
    pub path: String,
    pub bytes: Vec<u8>,
    pub is_markdown: bool,
}

impl BundleFile {
    /// The file's text, when it is valid UTF-8 (markdown must be).
    pub fn text(&self) -> Option<&str> {
        std::str::from_utf8(&self.bytes).ok()
    }
}

/// A fully parsed bundle: the archive bytes as published, plus every
/// file extracted. Parsing is the publish gate — holo-phext's rule
/// that a transaction is applied iff every entry verifies, restated
/// for zips: a bundle is stored iff every entry parses within caps.
pub struct ParsedBundle {
    pub zip_bytes: Vec<u8>,
    pub files: Vec<BundleFile>,
}

impl ParsedBundle {
    pub fn file(&self, path: &str) -> Option<&BundleFile> {
        self.files.iter().find(|f| f.path == path)
    }
}

pub fn is_markdown_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.ends_with(".md") || lower.ends_with(".markdown")
}

/// Strip the markdown extension from a path (`docs/guide.md` →
/// `docs/guide`), or return None when the path is not markdown.
pub fn markdown_stem(path: &str) -> Option<String> {
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".markdown") {
        Some(path[..path.len() - ".markdown".len()].to_string())
    } else if lower.ends_with(".md") {
        Some(path[..path.len() - ".md".len()].to_string())
    } else {
        None
    }
}

/// Path hygiene for one zip entry name. A path is an address, and an
/// address must stay inside the bundle: no absolute paths, no `..`,
/// no backslashes, no empty or `.` segments, no control characters.
pub fn valid_bundle_path(path: &str) -> bool {
    if path.is_empty() || path.len() > 512 {
        return false;
    }
    if path.starts_with('/') || path.contains('\\') {
        return false;
    }
    if path.chars().any(|c| (c as u32) < 0x20 || c as u32 == 0x7f) {
        return false;
    }
    for seg in path.split('/') {
        if seg.is_empty() || seg == "." || seg == ".." {
            return false;
        }
    }
    true
}

/// Parse and validate a bundle zip. Errors are plain-language strings
/// the publish API returns verbatim.
pub fn parse_bundle(zip_bytes: Vec<u8>) -> Result<ParsedBundle, String> {
    parse_entries(zip_bytes, true)
}

/// Parse a working-copy archive (see work.rs): the same hygiene and
/// caps as a published bundle, but a draft may legitimately hold no
/// markdown yet — the publish gate, not the draft, enforces that.
pub fn parse_working(zip_bytes: Vec<u8>) -> Result<ParsedBundle, String> {
    parse_entries(zip_bytes, false)
}

fn parse_entries(zip_bytes: Vec<u8>, require_markdown: bool) -> Result<ParsedBundle, String> {
    if zip_bytes.len() > MAX_ZIP_BYTES {
        return Err(format!(
            "bundle is {} bytes; the limit is {}",
            zip_bytes.len(),
            MAX_ZIP_BYTES
        ));
    }
    let cursor = Cursor::new(&zip_bytes);
    let mut archive =
        zip::ZipArchive::new(cursor).map_err(|e| format!("not a readable zip: {e}"))?;
    if archive.len() > MAX_FILES {
        return Err(format!(
            "bundle holds {} entries; the limit is {}",
            archive.len(),
            MAX_FILES
        ));
    }
    let mut files: Vec<BundleFile> = Vec::new();
    let mut total: u64 = 0;
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| format!("unreadable zip entry: {e}"))?;
        let name = entry.name().to_string();
        if name.ends_with('/') {
            continue; // a directory marker, not a file
        }
        if !valid_bundle_path(&name) {
            return Err(format!("unsafe path in bundle: {name:?}"));
        }
        if files.iter().any(|f| f.path == name) {
            return Err(format!("duplicate path in bundle: {name}"));
        }
        if entry.size() > MAX_FILE_BYTES {
            return Err(format!(
                "file {name} is {} bytes extracted; the limit is {}",
                entry.size(),
                MAX_FILE_BYTES
            ));
        }
        let mut bytes = Vec::with_capacity(entry.size() as usize);
        entry
            .read_to_end(&mut bytes)
            .map_err(|e| format!("cannot extract {name}: {e}"))?;
        total += bytes.len() as u64;
        if total > MAX_TOTAL_BYTES {
            return Err(format!(
                "bundle extracts to more than {} bytes; that is the limit",
                MAX_TOTAL_BYTES
            ));
        }
        let is_markdown = is_markdown_path(&name);
        if is_markdown && std::str::from_utf8(&bytes).is_err() {
            return Err(format!("markdown file {name} is not valid UTF-8 text"));
        }
        files.push(BundleFile {
            path: name,
            bytes,
            is_markdown,
        });
    }
    if require_markdown && files.iter().all(|f| !f.is_markdown) {
        return Err("bundle holds no markdown (.md) files".to_string());
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(ParsedBundle { zip_bytes, files })
}

/// Serialize files back into a bundle archive — the inverse of
/// parse_bundle, used for working copies (work.rs). Entries are
/// stored, never deflated, in path order, so the bytes are
/// deterministic for a given file set: the same discipline the
/// editor's hand-rolled writer keeps.
pub fn build_zip(files: &[BundleFile]) -> Result<Vec<u8>, String> {
    use std::io::Write as _;
    let mut sorted: Vec<&BundleFile> = files.iter().collect();
    sorted.sort_by(|a, b| a.path.cmp(&b.path));
    let cursor = Cursor::new(Vec::new());
    let mut writer = zip::ZipWriter::new(cursor);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Stored)
        .unix_permissions(0o644);
    for f in sorted {
        writer
            .start_file(&f.path, options)
            .map_err(|e| format!("cannot stage {}: {e}", f.path))?;
        writer
            .write_all(&f.bytes)
            .map_err(|e| format!("cannot write {}: {e}", f.path))?;
    }
    let cursor = writer
        .finish()
        .map_err(|e| format!("cannot finish the archive: {e}"))?;
    Ok(cursor.into_inner())
}

/// A file's incipit: its first heading if it opens with one, else its
/// first non-empty line, lightly stripped of markdown furniture. The
/// sidebar, the tracklist, prev/next links, and feeds all name a file
/// by its incipit — a file's display title derives from its own first
/// line, never from a name it was required to have.
pub fn incipit(text: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let stripped = if line.starts_with('#') {
            line.trim_start_matches('#').trim().trim_end_matches('#').trim()
        } else if let Some(rest) = line.strip_prefix("> ") {
            rest.trim()
        } else {
            line
        };
        if !stripped.is_empty() {
            return Some(stripped.to_string());
        }
    }
    None
}

/// The vault is the bundle's memory: `vault/` holds what was put
/// away. Vault paths are never served, listed, or fed — the vault is
/// private by structure, like a draft.
pub fn is_vault_path(path: &str) -> bool {
    path == "vault" || path.starts_with("vault/")
}

/// Is this the bundle's root self-description file?
pub fn is_root_index(path: &str) -> bool {
    !path.contains('/') && (path.eq_ignore_ascii_case("index.md") || path.eq_ignore_ascii_case("readme.md"))
}

/// Bundle order — the order the sidebar, the tracklist, and prev/next
/// all walk: the root index first (index.md ahead of README.md when a
/// bundle somehow holds both), then every other file by path. Vault
/// files are excluded; they are not part of the published order.
pub fn ordered_files(parsed: &ParsedBundle) -> Vec<&BundleFile> {
    let mut out: Vec<&BundleFile> = Vec::with_capacity(parsed.files.len());
    let root = parsed
        .files
        .iter()
        .filter(|f| is_root_index(&f.path))
        .min_by_key(|f| if f.path.eq_ignore_ascii_case("index.md") { 0 } else { 1 });
    if let Some(root) = root {
        out.push(root);
    }
    let root_path = root.map(|f| f.path.as_str());
    for f in &parsed.files {
        if is_vault_path(&f.path) || Some(f.path.as_str()) == root_path {
            continue;
        }
        out.push(f);
    }
    out
}

/// The bundle's title, from the bundle alone: the root index's first
/// line (its incipit), else the slug. No app-side document structure
/// exists to ask instead.
pub fn bundle_title(parsed: &ParsedBundle, slug: &str) -> String {
    for name in ["index.md", "README.md"] {
        if let Some(f) = parsed.files.iter().find(|f| f.path.eq_ignore_ascii_case(name) && !f.path.contains('/')) {
            if let Some(title) = f.text().and_then(incipit) {
                return title;
            }
        }
    }
    slug.to_string()
}

/// Slugs are the bundle's public name: `md.phext.io/b/<slug>/`.
/// Lowercase letters, digits, and hyphens; 1–64 chars; must start and
/// end with a letter or digit.
pub fn valid_slug(slug: &str) -> bool {
    if slug.is_empty() || slug.len() > 64 {
        return false;
    }
    if !slug
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return false;
    }
    let first = slug.chars().next().unwrap();
    let last = slug.chars().last().unwrap();
    if !first.is_ascii_alphanumeric() || !last.is_ascii_alphanumeric() {
        return false;
    }
    !RESERVED_SLUGS.contains(&slug)
}

/// Names that would collide with this service's own routes or files,
/// now or plausibly later.
pub const RESERVED_SLUGS: &[&str] = &[
    "api", "b", "about", "new", "edit", "admin", "feed", "feed.xml", "rss", "rss.xml",
    "sitemap.xml", "robots.txt", "favicon.ico", "favicon.svg", "healthz", "static",
    "assets", "index", "index.html",
];

/// SHA-256 hex of a byte string — edit keys are stored only as this.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = sha2::Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0xf) as usize] as char);
    }
    out
}

/// Constant-time equality for key hashes, so a wrong-key probe learns
/// nothing from timing.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

/// A fresh edit key: 32 bytes from the kernel's random source, hex.
/// (phext-lattice draws on `getrandom` for the same job; reading
/// /dev/urandom is the std-only equivalent on the hosts this runs on.)
pub fn new_edit_key() -> std::io::Result<String> {
    let mut buf = [0u8; 32];
    let mut f = std::fs::File::open("/dev/urandom")?;
    f.read_exact(&mut buf)?;
    Ok(sha256_hex(&buf)[..64].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_rules() {
        assert!(valid_slug("hello"));
        assert!(valid_slug("my-notes-2"));
        assert!(!valid_slug(""));
        assert!(!valid_slug("-lead"));
        assert!(!valid_slug("trail-"));
        assert!(!valid_slug("Upper"));
        assert!(!valid_slug("under_score"));
        assert!(!valid_slug("api"));
        assert!(!valid_slug(&"x".repeat(65)));
    }

    #[test]
    fn path_rules() {
        assert!(valid_bundle_path("index.md"));
        assert!(valid_bundle_path("docs/deep/guide.md"));
        assert!(!valid_bundle_path("/abs.md"));
        assert!(!valid_bundle_path("../up.md"));
        assert!(!valid_bundle_path("a/../b.md"));
        assert!(!valid_bundle_path("a//b.md"));
        assert!(!valid_bundle_path("a\\b.md"));
        assert!(!valid_bundle_path("./x.md"));
        assert!(!valid_bundle_path(""));
    }

    #[test]
    fn stems() {
        assert_eq!(markdown_stem("a/b.md").as_deref(), Some("a/b"));
        assert_eq!(markdown_stem("a/b.markdown").as_deref(), Some("a/b"));
        assert_eq!(markdown_stem("a/b.txt"), None);
    }
}
