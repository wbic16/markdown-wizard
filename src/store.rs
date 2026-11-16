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
//!   <slug>/prev/bundle.zip   the previous published version
//!   <slug>/prev/meta.txt     its meta, its own version and date
//! A directory whose bundle.zip fails to parse at load is skipped as
//! uncommitted.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::bundle::{constant_time_eq, new_edit_key, parse_bundle, sha256_hex, ParsedBundle};

/// One published bundle, immutable once built. Readers clone the
/// `Arc` and drop the lock immediately — the snapshot pattern.
pub struct Bundle {
    pub slug: String,
    pub parsed: ParsedBundle,
    /// Unix seconds of the publish that produced this state.
    pub published_at: u64,
    /// Release number: 1 at first publish, +1 at every re-publish.
    /// A restored bundle carries its own number back with it.
    pub version: u64,
    /// The publisher's one-line "what changed?", when given.
    pub changed: Option<String>,
    /// SHA-256 hex of the edit key. The key itself is never stored.
    pub key_sha256: String,
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
        Ok(Store {
            dir,
            bundles: RwLock::new(map),
        })
    }

    pub fn get(&self, slug: &str) -> Option<Arc<Bundle>> {
        self.bundles.read().unwrap().get(slug).cloned()
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

        let bundle = Arc::new(Bundle {
            slug: slug.to_string(),
            parsed,
            published_at: now_unix(),
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
        Ok(())
    }
}

fn io_err(e: std::io::Error) -> PublishError {
    PublishError::Io(e.to_string())
}

fn write_synced(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut f = fs::File::create(path)?;
    f.write_all(bytes)?;
    f.sync_all()
}

fn sync_file(path: &Path) -> std::io::Result<()> {
    fs::File::open(path)?.sync_all()
}

fn sync_dir(path: &Path) -> std::io::Result<()> {
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
    for line in meta.lines() {
        if let Some(v) = line.strip_prefix("key_sha256=") {
            key_sha256 = Some(v.trim().to_string());
        } else if let Some(v) = line.strip_prefix("published_at=") {
            published_at = v.trim().parse().unwrap_or(0);
        } else if let Some(v) = line.strip_prefix("version=") {
            version = v.trim().parse().unwrap_or(1);
        } else if let Some(v) = line.strip_prefix("changed=") {
            changed = clean_changed_line(v);
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
        version,
        changed,
        key_sha256,
    })
}
