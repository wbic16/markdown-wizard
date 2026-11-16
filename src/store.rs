//! The bundle store — holo-phext's layer-2 discipline, distilled to
//! the size a zip of markdown needs.
//!
//! What was taken from holo-phext, concretely:
//!
//! * **Atomicity by commit point.** A holo transaction is applied iff
//!   every entry verifies and its commit marker lands. Here a publish
//!   is applied iff the whole zip parses within caps (every entry
//!   verified in memory, before anything touches disk) and the staged
//!   `bundle.zip` rename lands. The rename is the commit marker: a
//!   crash before it leaves the old bundle exactly as it was; after
//!   it, the new bundle is the state. There is no half-published
//!   bundle, on disk or in memory.
//! * **Durability profile.** Holo checkpoints follow "temp file, sync,
//!   rename, directory sync". A publish does the same: stage in the
//!   bundle's own directory, `sync_all` the staged files, rename into
//!   place, `sync_all` the directory — then acknowledge.
//! * **Snapshot isolation.** A commit swaps one `Arc`; readers holding
//!   the old snapshot see all of the old bundle or none of it,
//!   forever. Here each bundle lives behind an `Arc<Bundle>` in the
//!   map; a publish builds the new `Bundle` completely, then swaps
//!   the `Arc` under a short write lock. In-flight readers are never
//!   disturbed and never see a mix.
//! * **Absolute operations.** Holo ops are absolute, so replay is
//!   idempotent. A publish is an absolute whole-bundle replacement,
//!   so re-publishing the same bytes is a no-op in effect, and the
//!   startup loader can simply re-read whatever committed state the
//!   renames left behind — a torn staging file is ignored, never
//!   applied.
//!
//! Reversibility replaces confirmation dialogs: before a commit, the
//! current bundle is *copied* (never renamed away — the current state
//! stays committed until the new commit lands) into `prev/`, so the
//! previous published version survives every release and restoring
//! it is the same commit operation run with the old bytes. The two
//! slots swap cleanly in both directions.
//!
//! On-disk layout, per slug, under the data directory:
//!   <slug>/bundle.zip        the archive exactly as published (commit)
//!   <slug>/meta.txt          key_sha256= / published_at= / version= / changed=
//!                            plus one `stamp` line per file (see below)
//!   <slug>/prev/bundle.zip   the previous published version
//!   <slug>/prev/meta.txt     its meta, its own version and date
//!   <slug>/work/…            the MCP working copy (see work.rs)
//!   <slug>/site/…            the derived outputs (see outputs.rs):
//!                            the pre-rendered static tree, its PDFs,
//!                            and deploy.zip — rebuilt at every commit
//! A directory whose bundle.zip fails to parse at load is skipped as
//! uncommitted.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::bundle::{constant_time_eq, new_edit_key, parse_bundle, sha256_hex, ParsedBundle};

/// When one file was first published, and when its content last
/// changed. A bundle's feed dates each item by its own file's
/// `last_changed`, so re-releasing a bundle for one file's sake does
/// not re-date the rest.
#[derive(Clone, Copy)]
pub struct FileStamps {
    pub first_published: u64,
    pub last_changed: u64,
}

/// One published bundle, immutable once built. Readers clone the
/// `Arc` and drop the lock immediately — the snapshot pattern.
pub struct Bundle {
    pub slug: String,
    pub parsed: ParsedBundle,
    /// Unix seconds of the publish that produced this state.
    pub published_at: u64,
    /// Per-file publish stamps, keyed by path. A file published
    /// before stamps existed has no entry; `stamps_of` falls back to
    /// the bundle's own stamp for it.
    pub file_stamps: HashMap<String, FileStamps>,
    /// Release number: 1 at first publish, +1 at every re-publish.
    /// A restored bundle carries its own number back with it.
    pub version: u64,
    /// The publisher's one-line "what changed?", when given.
    pub changed: Option<String>,
    /// SHA-256 hex of the edit key. The key itself is never stored.
    pub key_sha256: String,
}

impl Bundle {
    /// One file's (first published, last changed) stamps. Files
    /// with no stamps of their own — anything published before
    /// per-file stamps existed — fall back to the bundle's stamp:
    /// as far as the record knows, they were born and last changed
    /// when the bundle was.
    pub fn stamps_of(&self, path: &str) -> (u64, u64) {
        match self.file_stamps.get(path) {
            Some(s) => (s.first_published, s.last_changed),
            None => (self.published_at, self.published_at),
        }
    }
}

pub struct Store {
    dir: PathBuf,
    bundles: RwLock<HashMap<String, Arc<Bundle>>>,
}

pub enum PublishOutcome {
    /// First publish of this slug; carries the one-time edit key.
    Created { bundle: Arc<Bundle>, edit_key: String },
    /// Re-publish (or restore) of an existing slug, key verified.
    Replaced { bundle: Arc<Bundle> },
}

#[derive(Debug)]
pub enum PublishError {
    /// The bundle bytes failed validation (message is user-facing).
    Invalid(String),
    /// Slug exists and the supplied key is missing or wrong.
    WrongKey,
    /// Restore was asked for but no previous version exists.
    NoPrevious,
    /// Filesystem failure (message is for the log and the user).
    Io(String),
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
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

/// Sanitize a "what changed?" line: one line, printable, capped.
pub fn clean_changed_line(s: &str) -> Option<String> {
    let cleaned: String = s
        .chars()
        .filter(|c| !c.is_control())
        .take(200)
        .collect::<String>()
        .trim()
        .to_string();
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned)
    }
}

impl Store {
    /// Open the store at `dir`, loading every committed bundle.
    /// Unparseable or torn directories are skipped with a note on
    /// stderr — an uncommitted state is not a state.
    pub fn open(dir: PathBuf) -> std::io::Result<Store> {
        fs::create_dir_all(&dir)?;
        let mut map = HashMap::new();
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let slug_dir = entry.path();
            if !slug_dir.is_dir() {
                continue;
            }
            let slug = entry.file_name().to_string_lossy().into_owned();
            match load_bundle(&slug_dir, &slug) {
                Ok(bundle) => {
                    map.insert(slug, Arc::new(bundle));
                }
                Err(e) => {
                    eprintln!("markdown-wizard: skipping bundle {slug:?}: {e}");
                }
            }
        }
        let store = Store {
            dir,
            bundles: RwLock::new(map),
        };
        // Derived outputs are a cache of the committed state: any
        // bundle whose outputs are missing or visibly incomplete
        // (published before they existed, lost to a crash
        // mid-generation, or published while no Chromium existed
        // to print its PDFs) acquires them now, from its own
        // committed bytes.
        for bundle in store.list() {
            if crate::outputs::outputs_incomplete(&store.dir, &bundle) {
                if let Err(e) = crate::outputs::generate(&store.dir, &bundle) {
                    eprintln!("markdown-wizard: outputs for {} failed: {e}", bundle.slug);
                }
            }
        }
        Ok(store)
    }

    pub fn get(&self, slug: &str) -> Option<Arc<Bundle>> {
        self.bundles.read().unwrap().get(slug).cloned()
    }

    /// Every published bundle, ordered by slug — the MCP
    /// `list_bundles` surface.
    pub fn list(&self) -> Vec<Arc<Bundle>> {
        let mut out: Vec<Arc<Bundle>> =
            self.bundles.read().unwrap().values().cloned().collect();
        out.sort_by(|a, b| a.slug.cmp(&b.slug));
        out
    }

    pub fn count(&self) -> usize {
        self.bundles.read().unwrap().len()
    }

    /// Whether a previous published version survives for this slug.
    pub fn has_previous(&self, slug: &str) -> bool {
        self.dir.join(slug).join("prev").join("bundle.zip").is_file()
    }

    /// Publish a bundle. `supplied_key` is required iff the slug
    /// already exists. The whole operation is: verify in memory,
    /// check the key, preserve the current as previous, stage + sync
    /// + rename + sync, swap the Arc.
    pub fn publish(
        &self,
        slug: &str,
        zip_bytes: Vec<u8>,
        supplied_key: Option<&str>,
        changed: Option<String>,
    ) -> Result<PublishOutcome, PublishError> {
        // Gate 1: every entry verifies, in memory, before disk.
        let parsed = parse_bundle(zip_bytes).map_err(PublishError::Invalid)?;

        let existing = self.get(slug);
        let (key_sha256, fresh_key, version) = match &existing {
            Some(old) => {
                let supplied = supplied_key.unwrap_or("");
                let supplied_hash = sha256_hex(supplied.as_bytes());
                if !constant_time_eq(supplied_hash.as_bytes(), old.key_sha256.as_bytes()) {
                    return Err(PublishError::WrongKey);
                }
                (old.key_sha256.clone(), None, old.version + 1)
            }
            None => {
                let key = new_edit_key().map_err(|e| PublishError::Io(e.to_string()))?;
                (sha256_hex(key.as_bytes()), Some(key), 1)
            }
        };

        let now = now_unix();
        // Per-file stamps, from content hashes (the publish path
        // already hashes what it must): a file whose hash holds
        // keeps both stamps; a changed file keeps its first
        // published and advances only its last changed; a new file
        // is born stamped now.
        let mut file_stamps: HashMap<String, FileStamps> = HashMap::new();
        for f in &parsed.files {
            let stamp = match &existing {
                Some(old) => match old.parsed.file(&f.path) {
                    Some(prev) if sha256_hex(&prev.bytes) == sha256_hex(&f.bytes) => {
                        let (first, last) = old.stamps_of(&f.path);
                        FileStamps { first_published: first, last_changed: last }
                    }
                    Some(_) => {
                        let (first, _) = old.stamps_of(&f.path);
                        FileStamps { first_published: first, last_changed: now }
                    }
                    None => FileStamps { first_published: now, last_changed: now },
                },
                None => FileStamps { first_published: now, last_changed: now },
            };
            file_stamps.insert(f.path.clone(), stamp);
        }

        let bundle = Arc::new(Bundle {
            slug: slug.to_string(),
            parsed,
            published_at: now,
            file_stamps,
            version,
            changed,
            key_sha256,
        });
        self.commit(slug, &bundle)?;
        match fresh_key {
            Some(edit_key) => Ok(PublishOutcome::Created { bundle, edit_key }),
            None => Ok(PublishOutcome::Replaced { bundle }),
        }
    }

    /// First publish with a caller-issued edit key. The MCP working
    /// copy mints the slug's key when the draft is opened — the
    /// draft needs a credential before any publish exists — and
    /// that same key must open the bundle it becomes, so this path
    /// takes the key's hash from the caller instead of minting one.
    /// Only valid while the slug is unpublished; the web path
    /// (`publish`) mints its own key and is unchanged. Everything
    /// else — the in-memory gate, the commit sequence — is shared.
    pub fn publish_with_issued_key(
        &self,
        slug: &str,
        zip_bytes: Vec<u8>,
        key_sha256: String,
        changed: Option<String>,
    ) -> Result<Arc<Bundle>, PublishError> {
        let parsed = parse_bundle(zip_bytes).map_err(PublishError::Invalid)?;
        if self.get(slug).is_some() {
            return Err(PublishError::Invalid(format!(
                "slug {slug} is already published; a re-publish goes through publish"
            )));
        }
        // A first publish: every file is born stamped, first and
        // last both at this publish - the same stamps the web
        // publish path gives a first release.
        let now = now_unix();
        let file_stamps: HashMap<String, FileStamps> = parsed
            .files
            .iter()
            .map(|f| {
                (
                    f.path.clone(),
                    FileStamps { first_published: now, last_changed: now },
                )
            })
            .collect();
        let bundle = Arc::new(Bundle {
            slug: slug.to_string(),
            parsed,
            published_at: now,
            version: 1,
            changed,
            key_sha256,
            file_stamps,
        });
        self.commit(slug, &bundle)?;
        Ok(bundle)
    }

    /// Restore the previous published version — the reversibility
    /// that replaces confirmation. Runs the same commit path with the
    /// previous bytes and the previous meta (its own version, date,
    /// and change line come back with it), after preserving the
    /// current state into `prev/`, so a restore is itself reversible.
    pub fn restore(&self, slug: &str, supplied_key: Option<&str>) -> Result<Arc<Bundle>, PublishError> {
        let current = self.get(slug).ok_or(PublishError::NoPrevious)?;
        let supplied = supplied_key.unwrap_or("");
        let supplied_hash = sha256_hex(supplied.as_bytes());
        if !constant_time_eq(supplied_hash.as_bytes(), current.key_sha256.as_bytes()) {
            return Err(PublishError::WrongKey);
        }
        let prev_dir = self.dir.join(slug).join("prev");
        let prev = load_bundle(&prev_dir, slug).map_err(|_| PublishError::NoPrevious)?;
        let bundle = Arc::new(prev);
        self.commit(slug, &bundle)?;
        Ok(bundle)
    }

    /// The commit sequence shared by publish and restore: preserve
    /// the on-disk current into `prev/` by copy, stage the new state,
    /// sync, rename (bundle last — the commit point), sync the
    /// directories, swap the Arc.
    fn commit(&self, slug: &str, bundle: &Arc<Bundle>) -> Result<(), PublishError> {
        let slug_dir = self.dir.join(slug);
        fs::create_dir_all(&slug_dir).map_err(io_err)?;

        // Preserve the current committed state as the previous one.
        let cur_zip = slug_dir.join("bundle.zip");
        if cur_zip.is_file() {
            let prev_dir = slug_dir.join("prev");
            fs::create_dir_all(&prev_dir).map_err(io_err)?;
            let n = nonce();
            let tmp_zip = prev_dir.join(format!(".prev-{n}.zip"));
            let tmp_meta = prev_dir.join(format!(".prev-{n}.meta"));
            fs::copy(&cur_zip, &tmp_zip).map_err(io_err)?;
            if slug_dir.join("meta.txt").is_file() {
                fs::copy(slug_dir.join("meta.txt"), &tmp_meta).map_err(io_err)?;
                sync_file(&tmp_meta).map_err(io_err)?;
                fs::rename(&tmp_meta, prev_dir.join("meta.txt")).map_err(io_err)?;
            }
            sync_file(&tmp_zip).map_err(io_err)?;
            fs::rename(&tmp_zip, prev_dir.join("bundle.zip")).map_err(io_err)?;
            sync_dir(&prev_dir).map_err(io_err)?;
        }

        // Stage the new state beside its home and commit it.
        let n = nonce();
        let staged_zip = slug_dir.join(format!(".staging-{n}.zip"));
        let staged_meta = slug_dir.join(format!(".staging-{n}.meta"));
        let mut meta_text = format!(
            "key_sha256={}\npublished_at={}\nversion={}\n",
            bundle.key_sha256, bundle.published_at, bundle.version
        );
        if let Some(changed) = &bundle.changed {
            meta_text.push_str(&format!("changed={changed}\n"));
        }
        // One stamp line per file, in path order so the file is
        // stable: `stamp <first-published> <last-changed> <path>`.
        // The path is the rest of the line — paths may hold spaces
        // (never control characters; the bundle rules refuse those).
        let mut stamp_paths: Vec<&String> = bundle.file_stamps.keys().collect();
        stamp_paths.sort();
        for path in stamp_paths {
            let s = &bundle.file_stamps[path];
            meta_text.push_str(&format!(
                "stamp {} {} {}\n",
                s.first_published, s.last_changed, path
            ));
        }
        write_synced(&staged_meta, meta_text.as_bytes()).map_err(io_err)?;
        write_synced(&staged_zip, &bundle.parsed.zip_bytes).map_err(io_err)?;
        fs::rename(&staged_meta, slug_dir.join("meta.txt")).map_err(io_err)?;
        fs::rename(&staged_zip, slug_dir.join("bundle.zip")).map_err(io_err)?;
        sync_dir(&slug_dir).map_err(io_err)?;
        sync_dir(&self.dir).map_err(io_err)?;

        // The swap: one Arc, under a short write lock.
        self.bundles
            .write()
            .unwrap()
            .insert(slug.to_string(), bundle.clone());

        // The derived outputs follow the commit: the pre-rendered
        // pages, the PDFs, and the static-site download are rebuilt
        // from the bundle just committed and swapped in beside it.
        // They are a cache of the committed state, so a failure here
        // is logged, never fatal — the commit has already landed,
        // and Store::open rebuilds whatever is missing.
        if let Err(e) = crate::outputs::generate(&self.dir, bundle) {
            eprintln!("markdown-wizard: outputs for {slug} failed: {e}");
        }
        Ok(())
    }

    /// One file of a bundle's derived outputs (see outputs.rs), read
    /// from `<slug>/site/`. `rel` must be a path the caller has
    /// already validated as bundle-shaped (no `..`, no vault) — the
    /// outputs routes only ever pass the request path itself, after
    /// the same checks the bundle routes apply.
    pub fn site_file(&self, slug: &str, rel: &str) -> Option<Vec<u8>> {
        fs::read(self.dir.join(slug).join("site").join(rel)).ok()
    }
}

fn io_err(e: std::io::Error) -> PublishError {
    PublishError::Io(e.to_string())
}

pub(crate) fn write_synced(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut f = fs::File::create(path)?;
    f.write_all(bytes)?;
    f.sync_all()
}

fn sync_file(path: &Path) -> std::io::Result<()> {
    fs::File::open(path)?.sync_all()
}

pub(crate) fn sync_dir(path: &Path) -> std::io::Result<()> {
    fs::File::open(path)?.sync_all()
}

/// Load one committed bundle directory (the current slot, or `prev/`
/// — same shape). Anything missing, torn, or unparseable is an error
/// the caller treats as "not committed".
fn load_bundle(dir: &Path, slug: &str) -> Result<Bundle, String> {
    let meta = fs::read_to_string(dir.join("meta.txt"))
        .map_err(|e| format!("meta unreadable: {e}"))?;
    let mut key_sha256 = None;
    let mut published_at = 0u64;
    let mut version = 1u64;
    let mut changed = None;
    let mut file_stamps: HashMap<String, FileStamps> = HashMap::new();
    for line in meta.lines() {
        if let Some(v) = line.strip_prefix("key_sha256=") {
            key_sha256 = Some(v.trim().to_string());
        } else if let Some(v) = line.strip_prefix("published_at=") {
            published_at = v.trim().parse().unwrap_or(0);
        } else if let Some(v) = line.strip_prefix("version=") {
            version = v.trim().parse().unwrap_or(1);
        } else if let Some(v) = line.strip_prefix("changed=") {
            changed = clean_changed_line(v);
        } else if let Some(v) = line.strip_prefix("stamp ") {
            let mut parts = v.splitn(3, ' ');
            if let (Some(first), Some(last), Some(path)) =
                (parts.next(), parts.next(), parts.next())
            {
                if let (Ok(first), Ok(last)) =
                    (first.trim().parse::<u64>(), last.trim().parse::<u64>())
                {
                    file_stamps.insert(
                        path.to_string(),
                        FileStamps { first_published: first, last_changed: last },
                    );
                }
            }
        }
    }
    let key_sha256 = key_sha256.ok_or("meta holds no key hash")?;
    let zip_bytes =
        fs::read(dir.join("bundle.zip")).map_err(|e| format!("bundle unreadable: {e}"))?;
    let parsed = parse_bundle(zip_bytes)?;
    Ok(Bundle {
        slug: slug.to_string(),
        parsed,
        published_at,
        file_stamps,
        version,
        changed,
        key_sha256,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// A stored zip, built by hand the way the HTTP tests build
    /// theirs — the store's tests need real bundles, not fixtures
    /// of the store's own making.
    fn zip_of(files: &[(&str, &str)]) -> Vec<u8> {
        fn crc32(bytes: &[u8]) -> u32 {
            let mut table = [0u32; 256];
            for (n, slot) in table.iter_mut().enumerate() {
                let mut c = n as u32;
                for _ in 0..8 {
                    c = if c & 1 != 0 { 0xedb88320 ^ (c >> 1) } else { c >> 1 };
                }
                *slot = c;
            }
            let mut c = 0xffffffffu32;
            for &b in bytes {
                c = table[((c ^ b as u32) & 0xff) as usize] ^ (c >> 8);
            }
            c ^ 0xffffffff
        }
        let mut out = Vec::new();
        let mut central = Vec::new();
        for (name, text) in files {
            let offset = out.len() as u32;
            let data = text.as_bytes();
            let crc = crc32(data);
            let mut local = Vec::new();
            local.extend_from_slice(&0x04034b50u32.to_le_bytes());
            local.extend_from_slice(&20u16.to_le_bytes());
            local.extend_from_slice(&0x0800u16.to_le_bytes());
            local.extend_from_slice(&0u16.to_le_bytes());
            local.extend_from_slice(&0u16.to_le_bytes());
            local.extend_from_slice(&0x21u16.to_le_bytes());
            local.extend_from_slice(&crc.to_le_bytes());
            local.extend_from_slice(&(data.len() as u32).to_le_bytes());
            local.extend_from_slice(&(data.len() as u32).to_le_bytes());
            local.extend_from_slice(&(name.len() as u16).to_le_bytes());
            local.extend_from_slice(&0u16.to_le_bytes());
            local.extend_from_slice(name.as_bytes());
            out.extend_from_slice(&local);
            out.extend_from_slice(data);
            let mut cen = Vec::new();
            cen.extend_from_slice(&0x02014b50u32.to_le_bytes());
            cen.extend_from_slice(&20u16.to_le_bytes());
            cen.extend_from_slice(&20u16.to_le_bytes());
            cen.extend_from_slice(&0x0800u16.to_le_bytes());
            cen.extend_from_slice(&0u16.to_le_bytes());
            cen.extend_from_slice(&0u16.to_le_bytes());
            cen.extend_from_slice(&0x21u16.to_le_bytes());
            cen.extend_from_slice(&crc.to_le_bytes());
            cen.extend_from_slice(&(data.len() as u32).to_le_bytes());
            cen.extend_from_slice(&(data.len() as u32).to_le_bytes());
            cen.extend_from_slice(&(name.len() as u16).to_le_bytes());
            cen.extend_from_slice(&[0u8; 12]);
            cen.extend_from_slice(&offset.to_le_bytes());
            cen.extend_from_slice(name.as_bytes());
            central.extend_from_slice(&cen);
        }
        let cd_offset = out.len() as u32;
        out.extend_from_slice(&central);
        let mut end = Vec::new();
        end.extend_from_slice(&0x06054b50u32.to_le_bytes());
        end.extend_from_slice(&0u16.to_le_bytes());
        end.extend_from_slice(&0u16.to_le_bytes());
        end.extend_from_slice(&(files.len() as u16).to_le_bytes());
        end.extend_from_slice(&(files.len() as u16).to_le_bytes());
        end.extend_from_slice(&(central.len() as u32).to_le_bytes());
        end.extend_from_slice(&cd_offset.to_le_bytes());
        end.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&end);
        out
    }

    fn created_key(outcome: PublishOutcome) -> String {
        match outcome {
            PublishOutcome::Created { edit_key, .. } => edit_key,
            PublishOutcome::Replaced { .. } => panic!("expected a first publish"),
        }
    }

    #[test]
    fn stamps_advance_only_for_the_changed_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().to_path_buf()).unwrap();
        let v1 = zip_of(&[("index.md", "# Home\n"), ("about.md", "one\n")]);
        let key = created_key(store.publish("b", v1, None, None).unwrap());
        let b1 = store.get("b").unwrap();
        let index1 = b1.stamps_of("index.md");
        let about1 = b1.stamps_of("about.md");
        assert!(about1.0 > 0 && about1.0 == about1.1, "a new file is born stamped once");

        // Feed dates are seconds; let the clock move so an advance
        // is visible at all.
        std::thread::sleep(Duration::from_millis(1100));
        let v2 = zip_of(&[
            ("index.md", "# Home\n"),
            ("about.md", "two\n"),
            ("new.md", "# New\n"),
        ]);
        store.publish("b", v2, Some(&key), None).unwrap();
        let b2 = store.get("b").unwrap();
        assert_eq!(b2.stamps_of("index.md"), index1, "an untouched file keeps both stamps");
        let about2 = b2.stamps_of("about.md");
        assert_eq!(about2.0, about1.0, "a changed file keeps its first published");
        assert!(about2.1 > about1.1, "a changed file's last changed advances");
        let new2 = b2.stamps_of("new.md");
        assert_eq!(new2.0, new2.1, "a new file is born stamped once");
        assert_eq!(new2.1, about2.1, "a new file is stamped at this publish");

        // The stamps are the record: they survive a reload.
        let store2 = Store::open(dir.path().to_path_buf()).unwrap();
        let rb = store2.get("b").unwrap();
        assert_eq!(rb.stamps_of("index.md"), index1);
        assert_eq!(rb.stamps_of("about.md"), about2);
        assert_eq!(rb.stamps_of("new.md"), new2);
    }

    #[test]
    fn a_bundle_without_stamp_lines_falls_back_to_its_own_stamp() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().to_path_buf()).unwrap();
        let v1 = zip_of(&[("index.md", "# Home\n")]);
        store.publish("b", v1, None, None).unwrap();
        // Rewrite the meta the way a pre-stamps build wrote it.
        let meta_path = dir.path().join("b").join("meta.txt");
        let meta = fs::read_to_string(&meta_path).unwrap();
        let legacy: String = meta
            .lines()
            .filter(|l| !l.starts_with("stamp "))
            .map(|l| format!("{l}\n"))
            .collect();
        fs::write(&meta_path, legacy).unwrap();
        let store2 = Store::open(dir.path().to_path_buf()).unwrap();
        let b = store2.get("b").unwrap();
        assert!(b.published_at > 0);
        assert_eq!(b.stamps_of("index.md"), (b.published_at, b.published_at));
    }
}
