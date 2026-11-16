//! Working copies — the server-side drafts an MCP agent edits.
//!
//! The web editor is local-first: its drafts live in the browser and
//! meet the server only at the publish commit point. An MCP agent
//! has no browser, so its draft lives here — one working copy per
//! slug, held to the same discipline as everything else in this
//! service:
//!
//! * A working copy is **seeded from the published bundle** when the
//!   slug is published (opening it takes the bundle's edit key), or
//!   **minted fresh** for an unpublished slug, which issues the
//!   slug's edit key at open time. That key is the draft's only
//!   credential, and it is the same key that opens the bundle after
//!   the first publish — one key for the slug's whole life. (The
//!   store mints a key at first publish on the web path; here the
//!   key must exist before the first publish, because the draft
//!   itself needs a credential, so the store grew a
//!   publish-with-issued-key path for exactly this case.)
//! * Writes replace one file at a time and persist immediately,
//!   with the store's durability profile (stage, sync, rename,
//!   sync), so a draft survives a restart and is never half-written.
//! * A working copy remembers the published **version it was seeded
//!   from** (its base). Publishing refuses when the published bundle
//!   has moved past the base — a stale draft never silently
//!   overwrites a newer release. The commit itself is the store's
//!   own publish: the same gate, the same atomic swap. An agent's
//!   edits and a human's edits meet only at that commit point.
//! * A file may carry the **keep-private mark**: it stays in the
//!   working copy, fully editable, but it is not in the set a
//!   publish commits — the published bundle never contains it, so
//!   no published surface (pages, feed, downloads, outputs, syncs)
//!   can carry it. Un-marking and re-publishing restores it; marking
//!   a file that is already live and re-publishing removes it,
//!   because a publish replaces the whole published tree. The mark
//!   is distinct from the vault: a vault file is put away out of
//!   the bundle; a marked file stays in the bundle, out of the
//!   publication.
//!
//! On disk, per slug, beside the published slots (see store.rs):
//!   <slug>/work/bundle.zip   the draft's files, as an archive
//!   <slug>/work/meta.txt     key_sha256= / base_version=, plus one
//!                            `private <path>` line per marked file
//! A work directory whose archive fails to parse at load is skipped
//! as uncommitted, exactly like a torn publish.

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use crate::bundle::{
    build_zip, constant_time_eq, is_markdown_path, new_edit_key, parse_working, sha256_hex,
    BundleFile, MAX_FILES, MAX_FILE_BYTES, MAX_TOTAL_BYTES,
};
use crate::store::{sync_dir, write_synced, PublishError, Store};

/// One slug's draft: a mutable file set behind a lock, with the
/// version it was seeded from and the hash of its only credential.
pub struct WorkingCopy {
    pub files: Vec<BundleFile>,
    /// Published version this draft was seeded from; 0 means the
    /// slug has never been published.
    pub base_version: u64,
    /// SHA-256 hex of the edit key. The key itself is never stored.
    pub key_sha256: String,
    /// Paths carrying the keep-private mark (see the module docs):
    /// in the working copy, out of the publish set. A set, in path
    /// order, so the persisted form is stable.
    pub private: BTreeSet<String>,
}

impl WorkingCopy {
    pub fn file(&self, path: &str) -> Option<&BundleFile> {
        self.files.iter().find(|f| f.path == path)
    }

    pub fn is_private(&self, path: &str) -> bool {
        self.private.contains(path)
    }

    pub fn verify(&self, supplied_key: Option<&str>) -> bool {
        let supplied = supplied_key.unwrap_or("");
        let hash = sha256_hex(supplied.as_bytes());
        constant_time_eq(hash.as_bytes(), self.key_sha256.as_bytes())
    }

    pub fn total_bytes(&self) -> u64 {
        self.files.iter().map(|f| f.bytes.len() as u64).sum()
    }
}

pub enum WorkError {
    /// The supplied key is missing or does not open this slug.
    WrongKey,
    /// No working copy exists for the slug.
    NoWorkingCopy,
    /// Validation failed (message is user-facing).
    Invalid(String),
    /// The published bundle moved past the draft's base.
    Stale { base: u64, current: u64 },
    /// Filesystem failure (message is for the log).
    Io(String),
}

impl From<PublishError> for WorkError {
    fn from(e: PublishError) -> WorkError {
        match e {
            PublishError::WrongKey => WorkError::WrongKey,
            PublishError::Stale { base, current } => WorkError::Stale { base, current },
            PublishError::Invalid(m) => WorkError::Invalid(m),
            PublishError::NoPrevious => WorkError::Invalid("no previous version".to_string()),
            PublishError::Io(m) => WorkError::Io(m),
        }
    }
}

fn io_err(e: std::io::Error) -> WorkError {
    WorkError::Io(e.to_string())
}

fn nonce() -> String {
    format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    )
}

/// The result of opening a working copy.
pub struct OpenedCopy {
    pub copy: Arc<Mutex<WorkingCopy>>,
    /// The edit key, when this call minted the copy for a
    /// never-published slug. Shown once, like a first publish.
    pub minted_key: Option<String>,
}

pub struct WriteInfo {
    pub files: usize,
    pub base_version: u64,
    pub diverged: bool,
    pub minted_key: Option<String>,
    /// The written file's keep-private mark after this write.
    pub keep_private: bool,
}

/// Does this draft differ from what's published? The three-state
/// word (Drafting / Current / Diverged) rests on this comparison.
/// The comparison is against the publishable set: a file marked
/// keep-private is not a difference from the published bundle —
/// it is deliberately not in it.
pub fn diverged(store: &Store, slug: &str, copy: &WorkingCopy) -> bool {
    match store.get(slug) {
        None => copy.files.iter().any(|f| !copy.is_private(&f.path)),
        Some(b) => {
            let draft: Vec<(&str, &[u8])> = copy
                .files
                .iter()
                .filter(|f| !copy.is_private(&f.path))
                .map(|f| (f.path.as_str(), f.bytes.as_slice()))
                .collect();
            let published: Vec<(&str, &[u8])> = b
                .parsed
                .files
                .iter()
                .map(|f| (f.path.as_str(), f.bytes.as_slice()))
                .collect();
            draft != published
        }
    }
}

pub struct PublishedInfo {
    pub version: u64,
    pub published_at: u64,
    pub files: Vec<String>,
    /// Marked files held back from this publish, in path order.
    /// They remain in the working copy, marks and all.
    pub kept_private: Vec<String>,
}

pub struct WorkStore {
    dir: PathBuf,
    copies: RwLock<HashMap<String, Arc<Mutex<WorkingCopy>>>>,
}

impl WorkStore {
    /// Open the working-copy store at `dir`, loading every persisted
    /// draft. Unparseable work directories are skipped with a note —
    /// an uncommitted draft is not a draft.
    pub fn open(dir: PathBuf) -> std::io::Result<WorkStore> {
        let mut map = HashMap::new();
        if dir.is_dir() {
            for entry in fs::read_dir(&dir)? {
                let entry = entry?;
                let slug_dir = entry.path();
                if !slug_dir.is_dir() {
                    continue;
                }
                let work_dir = slug_dir.join("work");
                if !work_dir.is_dir() {
                    continue;
                }
                let slug = entry.file_name().to_string_lossy().into_owned();
                match load_working(&work_dir) {
                    Ok(copy) => {
                        map.insert(slug, Arc::new(Mutex::new(copy)));
                    }
                    Err(e) => {
                        eprintln!("markdown-wizard: skipping draft {slug:?}: {e}");
                    }
                }
            }
        }
        Ok(WorkStore {
            dir,
            copies: RwLock::new(map),
        })
    }

    pub fn get(&self, slug: &str) -> Option<Arc<Mutex<WorkingCopy>>> {
        self.copies.read().unwrap().get(slug).cloned()
    }

    /// Open the working copy for `slug`, seeding or minting it when
    /// none exists yet. Seeding from a published bundle requires the
    /// bundle's edit key; minting for a fresh slug issues the key.
    /// An existing copy always requires its key.
    pub fn open_or_seed(
        &self,
        store: &Store,
        slug: &str,
        supplied_key: Option<&str>,
    ) -> Result<OpenedCopy, WorkError> {
        if let Some(copy) = self.get(slug) {
            if !copy.lock().unwrap().verify(supplied_key) {
                return Err(WorkError::WrongKey);
            }
            return Ok(OpenedCopy {
                copy,
                minted_key: None,
            });
        }
        // Build the candidate outside the map lock.
        let (candidate, minted_key) = match store.get(slug) {
            Some(bundle) => {
                let supplied = supplied_key.unwrap_or("");
                let hash = sha256_hex(supplied.as_bytes());
                if !constant_time_eq(hash.as_bytes(), bundle.key_sha256.as_bytes()) {
                    return Err(WorkError::WrongKey);
                }
                (
                    WorkingCopy {
                        files: bundle.parsed.files.clone(),
                        base_version: bundle.version,
                        key_sha256: bundle.key_sha256.clone(),
                        // A published bundle's files are, by the
                        // fact of their publication, unmarked.
                        private: BTreeSet::new(),
                    },
                    None,
                )
            }
            None => {
                let key = new_edit_key().map_err(|e| WorkError::Io(e.to_string()))?;
                (
                    WorkingCopy {
                        files: Vec::new(),
                        base_version: 0,
                        key_sha256: sha256_hex(key.as_bytes()),
                        private: BTreeSet::new(),
                    },
                    Some(key),
                )
            }
        };
        // Register it — unless a concurrent first-open beat us, in
        // which case theirs wins and the key must open theirs.
        let mut map = self.copies.write().unwrap();
        if let Some(existing) = map.get(slug) {
            let existing = existing.clone();
            drop(map);
            if !existing.lock().unwrap().verify(supplied_key) {
                return Err(WorkError::WrongKey);
            }
            return Ok(OpenedCopy {
                copy: existing,
                minted_key: None,
            });
        }
        let copy = Arc::new(Mutex::new(candidate));
        map.insert(slug.to_string(), copy.clone());
        Ok(OpenedCopy { copy, minted_key })
    }

    /// Create or replace one file in the slug's working copy,
    /// opening the copy first when needed, and persist the draft
    /// before returning — a write that answered is a write that
    /// survives a restart. `keep_private`, when carried, sets or
    /// clears the file's keep-private mark with the same write;
    /// when absent, the mark stands as it was.
    pub fn write_file(
        &self,
        store: &Store,
        slug: &str,
        path: &str,
        content: &[u8],
        supplied_key: Option<&str>,
        keep_private: Option<bool>,
    ) -> Result<WriteInfo, WorkError> {
        if content.len() as u64 > MAX_FILE_BYTES {
            return Err(WorkError::Invalid(format!(
                "file {path} is {} bytes; the limit is {}",
                content.len(),
                MAX_FILE_BYTES
            )));
        }
        let opened = self.open_or_seed(store, slug, supplied_key)?;
        let mut copy = opened.copy.lock().unwrap();
        let old_len = copy.file(path).map(|f| f.bytes.len() as u64).unwrap_or(0);
        if copy.total_bytes() - old_len + content.len() as u64 > MAX_TOTAL_BYTES {
            return Err(WorkError::Invalid(format!(
                "bundle extracts to more than {} bytes; that is the limit",
                MAX_TOTAL_BYTES
            )));
        }
        match copy.files.iter_mut().find(|f| f.path == path) {
            Some(f) => {
                f.bytes = content.to_vec();
                f.is_markdown = is_markdown_path(path);
            }
            None => {
                if copy.files.len() >= MAX_FILES {
                    return Err(WorkError::Invalid(format!(
                        "bundle holds {} entries; the limit is {}",
                        copy.files.len() + 1,
                        MAX_FILES
                    )));
                }
                copy.files.push(BundleFile {
                    path: path.to_string(),
                    bytes: content.to_vec(),
                    is_markdown: is_markdown_path(path),
                });
                copy.files.sort_by(|a, b| a.path.cmp(&b.path));
            }
        }
        // The mark, when the write carries one: a property of the
        // file in the working copy, applied with the write that
        // names it and persisted with the draft.
        if let Some(mark) = keep_private {
            if mark {
                copy.private.insert(path.to_string());
            } else {
                copy.private.remove(path);
            }
        }
        let files = copy.files.len();
        let base_version = copy.base_version;
        let diverged = diverged(store, slug, &copy);
        let keep_private = copy.is_private(path);
        self.persist(slug, &copy)?;
        Ok(WriteInfo {
            files,
            base_version,
            diverged,
            minted_key: opened.minted_key,
            keep_private,
        })
    }

    /// Commit the slug's working copy through the store's own
    /// publish path — the same gate and the same atomic swap as a
    /// web release. Refuses when the published bundle has moved
    /// past the draft's base version. The commit set is the
    /// publishable set: files marked keep-private are held back
    /// (they stay in the working copy), so the published bundle
    /// never contains them — and because a publish replaces the
    /// whole published tree, a file marked after it was live
    /// leaves the published surface with this commit.
    pub fn publish(
        &self,
        store: &Store,
        slug: &str,
        supplied_key: Option<&str>,
        changed: Option<String>,
    ) -> Result<PublishedInfo, WorkError> {
        let copy = self.get(slug).ok_or(WorkError::NoWorkingCopy)?;
        let mut guard = copy.lock().unwrap();
        if !guard.verify(supplied_key) {
            return Err(WorkError::WrongKey);
        }
        // The publishable set: everything not marked keep-private.
        // The publish stays one atomic act — over this set.
        let publishable: Vec<BundleFile> = guard
            .files
            .iter()
            .filter(|f| !guard.is_private(&f.path))
            .cloned()
            .collect();
        if !publishable.iter().any(|f| f.is_markdown) {
            return Err(WorkError::Invalid(
                "nothing to publish: every markdown file in the working copy is marked keep-private. Un-mark at least one file first.".to_string(),
            ));
        }
        let kept_private: Vec<String> = guard
            .files
            .iter()
            .filter(|f| guard.is_private(&f.path))
            .map(|f| f.path.clone())
            .collect();
        let zip = build_zip(&publishable).map_err(WorkError::Invalid)?;
        // The staleness gate lives inside the store's commit lock:
        // checked here, it would race a web publish landing between
        // the check and the commit. publish_from_draft refuses with
        // Stale when the bundle has moved past the draft's base.
        let bundle = store.publish_from_draft(
            slug,
            zip,
            supplied_key,
            guard.key_sha256.clone(),
            guard.base_version,
            changed,
        )?;
        guard.base_version = bundle.version;
        self.persist(slug, &guard)?;
        Ok(PublishedInfo {
            version: bundle.version,
            published_at: bundle.published_at,
            files: bundle.parsed.files.iter().map(|f| f.path.clone()).collect(),
            kept_private,
        })
    }

    /// Persist one draft with the store's durability profile: stage
    /// beside its home, sync, rename into place, sync the directory.
    fn persist(&self, slug: &str, copy: &WorkingCopy) -> Result<(), WorkError> {
        let slug_dir = self.dir.join(slug);
        let work_dir = slug_dir.join("work");
        fs::create_dir_all(&work_dir).map_err(io_err)?;
        let zip = build_zip(&copy.files).map_err(WorkError::Invalid)?;
        let mut meta = format!(
            "key_sha256={}\nbase_version={}\n",
            copy.key_sha256, copy.base_version
        );
        // One `private <path>` line per marked file. The set is a
        // BTreeSet, so the lines land in path order and the file is
        // stable: the mark is part of the draft's committed state
        // and is restored with it. The path is the rest of the
        // line — paths may hold spaces (never control characters;
        // the bundle rules refuse those).
        for path in &copy.private {
            meta.push_str(&format!("private {path}\n"));
        }
        let n = nonce();
        let staged_zip = work_dir.join(format!(".staging-{n}.zip"));
        let staged_meta = work_dir.join(format!(".staging-{n}.meta"));
        write_synced(&staged_meta, meta.as_bytes()).map_err(io_err)?;
        write_synced(&staged_zip, &zip).map_err(io_err)?;
        fs::rename(&staged_meta, work_dir.join("meta.txt")).map_err(io_err)?;
        fs::rename(&staged_zip, work_dir.join("bundle.zip")).map_err(io_err)?;
        sync_dir(&work_dir).map_err(io_err)?;
        sync_dir(&slug_dir).map_err(io_err)?;
        Ok(())
    }
}

/// Load one persisted draft. Anything missing or unparseable is an
/// error the caller treats as "not committed".
fn load_working(work_dir: &Path) -> Result<WorkingCopy, String> {
    let meta = fs::read_to_string(work_dir.join("meta.txt"))
        .map_err(|e| format!("draft meta unreadable: {e}"))?;
    let mut key_sha256 = None;
    let mut base_version = 0u64;
    let mut private = BTreeSet::new();
    for line in meta.lines() {
        if let Some(v) = line.strip_prefix("key_sha256=") {
            key_sha256 = Some(v.trim().to_string());
        } else if let Some(v) = line.strip_prefix("base_version=") {
            base_version = v.trim().parse().unwrap_or(0);
        } else if let Some(v) = line.strip_prefix("private ") {
            // The path is the rest of the line, untrimmed: paths
            // may hold spaces.
            private.insert(v.to_string());
        }
    }
    let key_sha256 = key_sha256.ok_or("draft meta holds no key hash")?;
    let zip_bytes =
        fs::read(work_dir.join("bundle.zip")).map_err(|e| format!("draft unreadable: {e}"))?;
    let parsed = parse_working(zip_bytes)?;
    Ok(WorkingCopy {
        files: parsed.files,
        base_version,
        key_sha256,
        private,
    })
}
