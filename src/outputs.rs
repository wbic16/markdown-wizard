//! Derived outputs: the published bundle, pre-rendered.
//!
//! A publish produces one state (the bundle, in store.rs) and three
//! carriers of it, built here at commit time and cached beside the
//! bundle on disk:
//!
//! * **Pre-rendered HTML** — every markdown file's reading view,
//!   rendered once by the one renderer (render.rs) and stored as a
//!   static page, served at `/b/<slug>/<path>.html`. What is served
//!   is the export-shaped page below; its links are relative, and
//!   the `.html` address sits at the same depth as the reading view,
//!   so they resolve identically in both homes.
//! * **PDF** — the same rendered page, printed by a headless
//!   Chromium at publish time and cached with the snapshot; a
//!   reader's request never triggers a render. Chromium is the one
//!   external tool: it is found on PATH (or in the Playwright cache
//!   this repo's own e2e uses, or via the MARKDOWN_WIZARD_CHROMIUM
//!   environment variable), and when no Chromium exists the publish
//!   still lands — the bundle simply has no PDFs, and the `.pdf`
//!   address answers with a page that says so. See deploy/DEPLOY.md.
//! * **The static site** — the whole bundle as a deployable tree
//!   (the download at `/b/<slug>/site.zip`): the bundle index as
//!   `index.html`, one `.html` and one `.pdf` per file, the raw
//!   `.md` sources at their own paths, every carried file, the
//!   feed, the source zip as `bundle.zip`, and a README.txt that
//!   says what the owner is holding. Links inside the exported
//!   pages are rewritten to relative paths that resolve inside the
//!   tree, so the site works from any static host — an S3 website
//!   bucket is the named example — with no markdown-wizard server
//!   anywhere in the loop.
//!
//! The vault is excluded from every output, by the same rule that
//! keeps it unserved: it is private by structure, and a derived
//! carrier inherits the privacy of its source.
//!
//! On disk, per slug: `<slug>/site/…` holds the tree and
//! `site/deploy.zip` the zipped download. Generation builds into a
//! staging directory and swaps it into place after the bundle
//! commit has landed, so a crash mid-generation leaves the previous
//! outputs (or none) — never a half-built tree pretending to be the
//! snapshot's. Store::open regenerates any bundle whose outputs are
//! missing, which is also how bundles published before outputs
//! existed acquire theirs.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::bundle::{is_markdown_path, is_vault_path, markdown_stem, BundleFile};
use crate::render;
use crate::rss;
use crate::store::Bundle;

/// The site's canonical public origin. The exported feed is
/// generated against it, so the feed inside a downloaded site
/// stays a valid feed whose items point at the live bundle (a
/// feed reader needs absolute addresses; a downloaded tree has
/// none of its own to give).
pub const CANONICAL_BASE: &str = "https://md.phext.io";

const FAVICON_SVG: &str = include_str!("../web/favicon.svg");

/// The rest of the site's furniture the pages reference — the PNG
/// favicon the shell links and the keeper badge the bundle index
/// wears — carried into every tree in the same bytes the server
/// itself serves at those root addresses, so nothing on an
/// exported page points back at the living site.
const FAVICON_PNG: &[u8] = include_bytes!("../web/favicon.png");
const MASCOT_BADGE: &[u8] = include_bytes!("../web/mascot-badge.webp");

/// Whether a bundle's on-disk outputs are missing or visibly
/// incomplete: no site tree at all, or a Chromium is available now
/// but the tree holds no PDF for a bundle that has markdown — the
/// mark of a publish made while no Chromium existed. Store::open
/// regenerates the outputs of any bundle this names.
pub fn outputs_incomplete(data_dir: &Path, bundle: &Bundle) -> bool {
    let site = data_dir.join(&bundle.slug).join("site");
    if !site.is_dir() {
        return true;
    }
    if find_chromium().is_none() {
        return false;
    }
    let has_markdown = bundle
        .parsed
        .files
        .iter()
        .any(|f| f.is_markdown && !is_vault_path(&f.path));
    has_markdown && !tree_has_pdf(&site)
}

fn tree_has_pdf(dir: &Path) -> bool {
    let Ok(rd) = fs::read_dir(dir) else {
        return false;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if tree_has_pdf(&path) {
                return true;
            }
        } else if path.extension().map(|e| e == "pdf").unwrap_or(false) {
            return true;
        }
    }
    false
}

fn nonce() -> String {
    format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    )
}

/// Build (or rebuild) the derived outputs for one committed bundle
/// and swap them into `<data>/<slug>/site/`. Filesystem failures
/// are the caller's to log; a missing Chromium is not a failure —
/// it is the degraded state (the bundle simply has no PDFs).
pub fn generate(data_dir: &Path, bundle: &Bundle) -> Result<(), String> {
    let slug_dir = data_dir.join(&bundle.slug);
    fs::create_dir_all(&slug_dir).map_err(|e| format!("cannot create {}: {e}", slug_dir.display()))?;
    let staging = slug_dir.join(format!(".site-{}", nonce()));
    fs::create_dir_all(&staging).map_err(|e| format!("cannot stage outputs: {e}"))?;
    let result = build_tree(&staging, bundle).and_then(|_| swap_into_place(&slug_dir, &staging));
    if result.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    result
}

fn swap_into_place(slug_dir: &Path, staging: &Path) -> Result<(), String> {
    let site = slug_dir.join("site");
    let trash = slug_dir.join(format!(".site-old-{}", nonce()));
    if site.is_dir() {
        fs::rename(&site, &trash)
            .map_err(|e| format!("cannot retire the previous outputs: {e}"))?;
    }
    if let Err(e) = fs::rename(staging, &site) {
        let _ = fs::rename(&trash, &site);
        return Err(format!("cannot install the new outputs: {e}"));
    }
    let _ = fs::remove_dir_all(&trash);
    Ok(())
}

/// The tree itself: carried files, furniture, rendered pages,
/// printed PDFs, and the zipped download of all of it.
fn build_tree(staging: &Path, bundle: &Bundle) -> Result<(), String> {
    let files: HashSet<&str> = bundle.parsed.files.iter().map(|f| f.path.as_str()).collect();

    // Furniture paths the site itself owns. A carried file at one
    // of these addresses is shadowed by the furniture, exactly as
    // the live routes shadow a carried feed.xml with the feed.
    let furniture: HashSet<&str> = ["index.html", "feed.xml", "favicon.svg", "favicon.png", "mascot-badge.webp", "bundle.zip", "README.txt", "deploy.zip"]
        .into_iter()
        .collect();

    // Carried files first — every non-vault file at its own path,
    // the assets the pages point at included.
    for f in &bundle.parsed.files {
        if is_vault_path(&f.path) || furniture.contains(f.path.as_str()) {
            continue;
        }
        write_file(&staging.join(&f.path), &f.bytes)?;
    }

    // The furniture.
    write_file(&staging.join("favicon.svg"), FAVICON_SVG.as_bytes())?;
    write_file(&staging.join("favicon.png"), FAVICON_PNG)?;
    write_file(&staging.join("mascot-badge.webp"), MASCOT_BADGE)?;
    write_file(&staging.join("bundle.zip"), &bundle.parsed.zip_bytes)?;
    write_file(
        &staging.join("feed.xml"),
        rss::feed_xml(bundle, CANONICAL_BASE).as_bytes(),
    )?;
    write_file(&staging.join("README.txt"), readme_text(bundle).as_bytes())?;

    // The bundle index, rendered once and rewritten for export.
    let index_html = render::index_page(bundle, CANONICAL_BASE);
    let index_html = rewrite_page(&index_html, bundle, "", "index.html");
    write_file(&staging.join("index.html"), index_html.as_bytes())?;

    // One rendered page (and, when a Chromium exists, one PDF) per
    // markdown file. A carried file already sitting at a page's
    // `.html` address wins that address — the live site serves it
    // first too — so no page is generated over it; the same holds
    // for a carried `.pdf`. The root index's page is the bundle
    // index itself (its content is the index's introduction), so
    // no separate `index.html` is rendered for it.
    let chromium = find_chromium();
    let pdf_work = staging.join(".pdf-work");
    for f in crate::bundle::ordered_files(&bundle.parsed) {
        if !f.is_markdown || is_vault_path(&f.path) {
            continue;
        }
        let stem = markdown_stem(&f.path).unwrap_or_else(|| f.path.clone());
        let html_rel = format!("{stem}.html");
        let pdf_rel = format!("{stem}.pdf");
        let page = render::reading_page(bundle, f, CANONICAL_BASE);
        let page = rewrite_page(&page, bundle, &parent_dir(&stem), &html_rel);
        if html_rel != "index.html" && !files.contains(html_rel.as_str()) {
            write_file(&staging.join(&html_rel), page.as_bytes())?;
        }
        if chromium.is_some() && !files.contains(pdf_rel.as_str()) {
            // Print from a copy with the theme forced to light:
            // the PDF is the printed page, and a fresh browser
            // profile must not inherit a night setting from its
            // environment. The forcing script runs after the
            // page's own resolver and before first paint.
            let print_html = page.replacen(
                "</head>",
                "<script>document.documentElement.setAttribute(\"data-theme\",\"light\")</script></head>",
                1,
            );
            fs::create_dir_all(&pdf_work).map_err(|e| format!("cannot stage pdf work: {e}"))?;
            // The print input sits beside the page's own tree
            // address (under a throwaway name, removed right after
            // printing), so the page's relative images resolve
            // against the tree exactly as a reader's would.
            let input = staging.join(format!("{stem}.pdfprint.html"));
            let out = staging.join(&pdf_rel);
            if let Some(parent) = input.parent() {
                fs::create_dir_all(parent).map_err(|e| format!("cannot stage pdf input: {e}"))?;
            }
            // A failed print is a skipped PDF, never a failed
            // publish — the result is deliberately unexamined.
            let _ = fs::write(&input, print_html.as_bytes()).is_ok()
                && print_pdf(chromium.as_ref().unwrap(), &input, &out, &pdf_work);
            let _ = fs::remove_file(&input);
        }
    }
    let _ = fs::remove_dir_all(&pdf_work);

    // The download: the whole tree as one zip, itself excluded.
    let mut tree_files: Vec<BundleFile> = Vec::new();
    collect_tree(staging, staging, &mut tree_files)?;
    let zip = crate::bundle::build_zip(&tree_files)
        .map_err(|e| format!("cannot build the site download: {e}"))?;
    write_file(&staging.join("deploy.zip"), &zip)?;
    Ok(())
}

fn collect_tree(root: &Path, dir: &Path, out: &mut Vec<BundleFile>) -> Result<(), String> {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .map_err(|e| format!("cannot read {}: {e}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            collect_tree(root, &path, out)?;
        } else {
            let rel = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            if rel == "deploy.zip" {
                continue;
            }
            let bytes = fs::read(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            let is_markdown = is_markdown_path(&rel);
            out.push(BundleFile { path: rel, bytes, is_markdown });
        }
    }
    Ok(())
}

fn write_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    fs::write(path, bytes).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

fn readme_text(bundle: &Bundle) -> String {
    let title = crate::bundle::bundle_title(&bundle.parsed, &bundle.slug);
    format!(
        "{title} — a static site, ready to host\n\
         ====================================\n\n\
         This is the bundle \"{slug}\" (version {version}, published {date}) as a\n\
         ready-made static site, generated by md.phext.io at publish time.\n\
         Everything a reader needs is here, and nothing here needs a server\n\
         smarter than one that hands back files:\n\n\
         \x20 index.html    the bundle's front page\n\
         \x20 <name>.html   every markdown file, pre-rendered\n\
         \x20 <name>.pdf    the same page, printed to PDF\n\
         \x20 <name>.md     the markdown source itself, at its own path\n\
         \x20 feed.xml      the bundle's RSS feed (its items point at the live\n\
         \x20               bundle on md.phext.io, where the feed is canonical)\n\
         \x20 bundle.zip    the bundle exactly as published — the source archive\n\
         \x20 mascot-badge.webp, favicon.png, favicon.svg\n\
         \x20               the furniture the pages reference, carried along so\n\
         \x20               nothing on a page points back at the living site\n\n\
         To host it: put the whole contents of this folder anywhere that\n\
         serves static files, keeping the paths as they are — an S3 bucket\n\
         with website hosting enabled, a folder under any web server, a\n\
         static host of any kind. The links between pages are relative, so\n\
         the site works from a domain root or from a subfolder alike.\n\n\
         The bundle's private vault, if it had one, is not here: the vault\n\
         is never published, in any form.\n",
        slug = bundle.slug,
        version = bundle.version,
        date = rss::long_date(bundle.published_at),
    )
}
/// The deploy-download anchor, unwrapped in the export: inside a
/// downloaded site the reader already holds the download, so the
/// link becomes its plain text.
fn unwrap_deploy_anchor(html: &str, slug: &str) -> String {
    let href = format!("/b/{slug}/site.zip");
    let mut out = html.to_string();
    loop {
        let Some(a) = out.find("<a ") else { break };
        let Some(close) = out[a..].find('>') else { break };
        let tag_end = a + close;
        if !out[a..=tag_end].contains(&format!("href=\"{href}\"")) {
            // Not the deploy anchor; move past it and keep looking.
            let rest = out[tag_end + 1..].to_string();
            let head = out[..=tag_end].to_string();
            let tail = unwrap_deploy_anchor(&rest, slug);
            return head + &tail;
        }
        let Some(end_tag) = out[tag_end..].find("</a>") else { break };
        let end_tag = tag_end + end_tag;
        let inner = out[tag_end + 1..end_tag].to_string();
        out.replace_range(a..end_tag + 4, &inner);
    }
    out
}

/// Rewrite one rendered page's links for its life inside the
/// exported tree. `live_dir` is the directory of the page's live
/// address inside the bundle ("" for the index, the stem's parent
/// for a reading page); `export_path` is where the page lives in
/// the tree.
fn rewrite_page(html: &str, bundle: &Bundle, live_dir: &str, export_path: &str) -> String {
    let mut html = unwrap_deploy_anchor(html, &bundle.slug);
    // The index's feed line names the live feed's address as text;
    // inside the export it is simply the feed next door.
    let feed_addr = format!("{CANONICAL_BASE}/b/{}/feed.xml", bundle.slug);
    html = html.replace(&feed_addr, "feed.xml");
    // The ways-to-use-it page lives on the service, not in the
    // bundle: in the export it names the service's own address.
    html = html.replace("href=\"/integrate\"", "href=\"https://md.phext.io/integrate\"");

    let files: HashSet<&str> = bundle.parsed.files.iter().map(|f| f.path.as_str()).collect();
    let export_dir = parent_dir(export_path);
    let mut out = String::with_capacity(html.len());
    let mut rest = html.as_str();
    loop {
        // The next href= or src=, whichever comes first.
        let next_href = rest.find("href=\"");
        let next_src = rest.find("src=\"");
        let (pos, len) = match (next_href, next_src) {
            (Some(h), Some(s)) => {
                if h < s { (h, 6) } else { (s, 5) }
            }
            (Some(h), None) => (h, 6),
            (None, Some(s)) => (s, 5),
            (None, None) => break,
        };
        let val_start = pos + len;
        let Some(val_end_rel) = rest[val_start..].find('"') else {
            break;
        };
        let val_end = val_start + val_end_rel;
        let value = &rest[val_start..val_end];
        let rewritten = rewrite_link(value, bundle, &files, live_dir, &export_dir);
        out.push_str(&rest[..val_start]);
        out.push_str(&rewritten);
        rest = &rest[val_end..];
    }
    out.push_str(rest);
    out
}

/// Rewrite one attribute value, or return it unchanged when it does
/// not name something inside the bundle (external links, fragments,
/// and anything the bundle does not hold stay exactly as written).
fn rewrite_link(value: &str, bundle: &Bundle, files: &HashSet<&str>, live_dir: &str, export_dir: &str) -> String {
    let (core, suffix) = split_suffix(value);
    if core.is_empty() || core.contains("://") || core.starts_with("mailto:") || core.starts_with("data:") {
        return value.to_string();
    }
    let Some(target) = resolve_target(core, bundle, files, live_dir) else {
        return value.to_string();
    };
    let rel = relative_from(export_dir, &target);
    format!("{rel}{suffix}")
}

/// The fragment or query of an attribute value, if any — the part a
/// rewrite must carry along untouched.
fn split_suffix(value: &str) -> (&str, &str) {
    let hash = value.find('#');
    let query = value.find('?');
    let cut = match (hash, query) {
        (Some(h), Some(q)) => Some(h.min(q)),
        (Some(h), None) => Some(h),
        (None, Some(q)) => Some(q),
        (None, None) => None,
    };
    match cut {
        Some(i) => (&value[..i], &value[i..]),
        None => (value, ""),
    }
}

/// Resolve one link core to a path inside the exported tree, or
/// None when it names nothing there. Two candidates, in order: the
/// link as a bundle-root-relative address (the way this server's
/// own generated links are written), and the link resolved against
/// the page's own directory (the way a browser reads a relative
/// link — and the way an author's in-article links mean it).
fn resolve_target(core: &str, bundle: &Bundle, files: &HashSet<&str>, live_dir: &str) -> Option<String> {
    if let Some(p) = core.strip_prefix('/') {
        let slug = &bundle.slug;
        if p == "favicon.svg" || p == "favicon.png" || p == "mascot-badge.webp" {
            return Some(p.to_string());
        }
        if p == format!("b/{slug}.zip") {
            return Some("bundle.zip".to_string());
        }
        if p == format!("b/{slug}") || p == format!("b/{slug}/") {
            return Some("index.html".to_string());
        }
        if let Some(inner) = p.strip_prefix(&format!("b/{slug}/")) {
            return map_live_path(inner, files);
        }
        return None;
    }
    if let Some(t) = map_live_path(core, files) {
        return Some(t);
    }
    let joined = join_normalized(live_dir, core);
    map_live_path(&joined, files)
}

/// One bundle-internal path ("" for the bundle root) → its path in
/// the exported tree, or None when nothing in the bundle answers
/// to it.
fn map_live_path(p: &str, files: &HashSet<&str>) -> Option<String> {
    if p.is_empty() {
        return Some("index.html".to_string());
    }
    if p == "feed.xml" || p == "favicon.svg" || p == "favicon.png" || p == "mascot-badge.webp" || p == "bundle.zip" {
        return Some(p.to_string());
    }
    if files.contains(p) {
        return Some(p.to_string());
    }
    if files.contains(format!("{p}.md").as_str()) || files.contains(format!("{p}.markdown").as_str()) {
        return Some(format!("{p}.html"));
    }
    None
}

/// Join a relative link onto a directory, normalizing `.` and `..`
/// the way a browser would.
fn join_normalized(dir: &str, rel: &str) -> String {
    let mut parts: Vec<&str> = dir.split('/').filter(|s| !s.is_empty()).collect();
    for seg in rel.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    parts.join("/")
}

fn parent_dir(path: &str) -> String {
    match path.rfind('/') {
        Some(i) => path[..i].to_string(),
        None => String::new(),
    }
}

/// A path inside the exported tree, expressed relative to the
/// directory `from_dir` — the `../` count a static site needs.
fn relative_from(from_dir: &str, target: &str) -> String {
    let from: Vec<&str> = from_dir.split('/').filter(|s| !s.is_empty()).collect();
    let to: Vec<&str> = target.split('/').filter(|s| !s.is_empty()).collect();
    let mut common = 0usize;
    while common < from.len() && common < to.len() && from[common] == to[common] {
        common += 1;
    }
    let mut out = String::new();
    for _ in common..from.len() {
        out.push_str("../");
    }
    out.push_str(&to[common..].join("/"));
    out
}

// ——— Chromium ——————————————————————————————————————————————

/// Find a Chromium to print with: an explicit environment variable
/// first, then the PATH, then the Playwright cache this repo's own
/// e2e uses. Remembered for the process's life — the binary does
/// not move under a running server.
fn find_chromium() -> Option<PathBuf> {
    static FOUND: OnceLock<Option<PathBuf>> = OnceLock::new();
    FOUND.get_or_init(|| {
        if let Ok(p) = std::env::var("MARKDOWN_WIZARD_CHROMIUM") {
            let pb = PathBuf::from(&p);
            if is_executable(&pb) {
                return Some(pb);
            }
        }
        for name in [
            "chromium",
            "chromium-browser",
            "google-chrome",
            "google-chrome-stable",
            "chrome-headless-shell",
            "chrome",
        ] {
            if let Some(p) = which(name) {
                return Some(p);
            }
        }
        if let Some(home) = std::env::var_os("HOME") {
            let cache = PathBuf::from(home).join(".cache/ms-playwright");
            if let Ok(rd) = fs::read_dir(&cache) {
                let mut candidates: Vec<PathBuf> = Vec::new();
                for entry in rd.flatten() {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if name.starts_with("chromium_headless_shell-") {
                        candidates.push(
                            entry
                                .path()
                                .join("chrome-headless-shell-linux64/chrome-headless-shell"),
                        );
                    } else if name.starts_with("chromium-") {
                        candidates.push(entry.path().join("chrome-linux/chrome"));
                    }
                }
                candidates.sort();
                for p in candidates {
                    if is_executable(&p) {
                        return Some(p);
                    }
                }
            }
        }
        None
    })
    .clone()
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in path.to_string_lossy().split(':') {
        let p = PathBuf::from(dir).join(name);
        if is_executable(&p) {
            return Some(p);
        }
    }
    None
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(p)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(p: &Path) -> bool {
    fs::metadata(p).map(|m| m.is_file()).unwrap_or(false)
}

/// Print one rendered page to PDF. The browser reads the staged
/// page from disk; its relative images resolve against the staged
/// tree, which holds the carried files beside it. A failure —
/// missing binary, a crash, a timeout — is a skipped PDF, never a
/// failed publish: the caller records nothing and moves on.
fn print_pdf(chromium: &Path, html: &Path, out: &Path, work_dir: &Path) -> bool {
    let profile = work_dir.join(".pdf-profile");
    let mut cmd = Command::new(chromium);
    cmd.arg("--no-sandbox")
        .arg("--disable-gpu")
        .arg("--disable-dev-shm-usage");
    // The headless shell is headless-only; a full browser needs
    // the flag. The file's magic, checked below, is the real gate
    // either way.
    let name = chromium.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    if !name.contains("headless") {
        cmd.arg("--headless");
    }
    cmd.arg(format!("--user-data-dir={}", profile.display()))
        .arg(format!("--print-to-pdf={}", out.display()))
        .arg("--no-pdf-header-footer")
        .arg(format!("file://{}", html.display()))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(_) => return false,
    };
    let status = wait_timeout(&mut child, Duration::from_secs(30));
    let _ = fs::remove_dir_all(&profile);
    match status {
        Some(s) if s.success() => match fs::read(out) {
            Ok(bytes) => bytes.len() > 512 && bytes.starts_with(b"%PDF-"),
            Err(_) => false,
        },
        _ => false,
    }
}

/// Wait for a child, or kill it when the budget runs out.
fn wait_timeout(child: &mut Child, timeout: Duration) -> Option<std::process::ExitStatus> {
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(_) => return None,
        }
    }
}
