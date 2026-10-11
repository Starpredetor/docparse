//! The directory walk behind `docparse index`.
//!
//! Shape: decide serially (cheap stat, hash only when needed), parse the
//! files that need it in parallel, write every result from the one thread
//! that owns the SQLite connection. `rusqlite::Connection` is not `Sync`, so
//! the database is why parsing and writing are split, not preference.
//!
//! MEMORY: indexing holds roughly `2 x jobs` documents' chunks in flight (one
//! per worker plus up to `jobs` buffered in the channel). Measured peak working
//! set re-indexing a 94 MB corpus: about 35 MB at `--jobs 1` and 76 MB at
//! `--jobs 4`. It does not grow with corpus size, but the 64 MB budget applies
//! to the streaming path and to `--jobs 1`, not to parallel indexing.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::UNIX_EPOCH;

use rayon::prelude::*;
use walkdir::WalkDir;

use super::config::{IndexConfig, Staleness};
use super::detect::{decide_after_hash, decide_cheap, Action, Decision, FileStatus, WalkedFile};
use super::Index;
use crate::chunk::Chunker;
use crate::error::{Error, Result};
use crate::model::Chunk;

#[derive(Debug, Clone, Copy)]
pub struct IndexOptions {
    /// Parse workers. Values below 1 are treated as 1.
    pub jobs: usize,
    /// Reattempt files recorded as failed even when they have not changed.
    pub retry_failed: bool,
    /// Ignore everything cached and parse every file.
    pub reindex: bool,
}

/// What one pass did. These counts are the observable behind the feature's
/// promises: `parsed == 0` on a re-run is "nothing to do costs nothing".
#[derive(Debug, Clone, Default, PartialEq)]
pub struct IndexReport {
    pub parsed: usize,
    /// Path pointed at content already in the index (rename, copy, duplicate).
    pub linked: usize,
    /// Content unchanged, only the timestamp moved.
    pub touched: usize,
    pub unchanged: usize,
    pub failed: usize,
    /// Known failures left alone because the file has not changed.
    pub skipped_failed: usize,
    pub paths_pruned: usize,
    pub files_pruned: usize,
    /// Files whose size or mtime moved while they were being processed.
    /// Nothing is stored for them; the next run picks them up.
    pub modified_during_run: usize,
    /// Directory entries that could not be read. Any of these means the walk
    /// was incomplete, so nothing is pruned and settings are not recorded.
    pub walk_errors: usize,
    /// Why every file was re-parsed, when that happened.
    pub reason: Option<String>,
}

/// BLAKE3 digest and byte size, read in blocks.
///
/// Streamed deliberately: hashing a 94 MB PDF must not load it into memory,
/// or the project's bounded-memory property dies here.
pub fn hash_file(path: &Path) -> Result<(String, u64)> {
    use std::io::Read;

    let io = |source| Error::Io {
        path: path.to_path_buf(),
        source,
    };
    let file = std::fs::File::open(path).map_err(io)?;
    let mut reader = std::io::BufReader::new(file);
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut total: u64 = 0;

    loop {
        let n = reader.read(&mut buf).map_err(io)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        total += n as u64;
    }

    Ok((hasher.finalize().to_hex().to_string(), total))
}

/// Key under which paths are compared. Windows treats `Paper.pdf` and
/// `paper.pdf` as one file; Unix treats them as two. Getting this backwards
/// either double-counts case-renamed files or merges distinct ones.
#[cfg(windows)]
fn path_key(rel: &str) -> String {
    rel.to_lowercase()
}

#[cfg(not(windows))]
fn path_key(rel: &str) -> String {
    rel.to_string()
}

fn stat(path: &Path) -> Result<WalkedFile> {
    let io = |source| Error::Io {
        path: path.to_path_buf(),
        source,
    };
    let md = std::fs::metadata(path).map_err(io)?;
    let modified = md.modified().map_err(io)?;
    let mtime_ns = match modified.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_nanos() as i128,
        Err(before) => -(before.duration().as_nanos() as i128),
    };
    Ok(WalkedFile {
        byte_size: md.len(),
        mtime_ns,
    })
}

/// `rel_path` relative to the root with forward slashes on every platform, so
/// an index is portable. `None` when a component is not valid UTF-8: lossy
/// conversion could make two distinct files collide.
fn rel_path(root: &Path, abs: &Path) -> Option<String> {
    let rel = abs.strip_prefix(root).ok()?;
    let parts: Option<Vec<&str>> = rel.components().map(|c| c.as_os_str().to_str()).collect();
    Some(parts?.join("/"))
}

struct Follower {
    rel: String,
    abs: PathBuf,
    walked: WalkedFile,
}

/// A file to parse. `followers` are other new paths with identical content,
/// linked to the leader's result instead of being parsed again.
struct ParseItem {
    rel: String,
    abs: PathBuf,
    hash: String,
    walked: WalkedFile,
    followers: Vec<Follower>,
}

fn parse_one(abs: &Path, config: &IndexConfig) -> std::result::Result<Vec<Chunk>, Error> {
    let source = crate::open_source(abs, config.ocr, config.ocr_dpi)?;
    let name = abs
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| abs.to_string_lossy().into_owned());
    Chunker::new(source, name, config.chunk).collect()
}

fn unchanged_since(abs: &Path, before: &WalkedFile) -> bool {
    matches!(stat(abs), Ok(now) if now == *before)
}

impl Index {
    /// Brings the index up to date with the directory.
    pub fn index_dir(&mut self, config: &IndexConfig, opts: &IndexOptions) -> Result<IndexReport> {
        self.index_dir_with_hook(config, opts, &|_| {})
    }

    /// As `index_dir`, calling `after_parse(path)` just before each parsed
    /// file is re-checked. Exists so a test can modify a file at the one
    /// moment that matters; not part of the supported API.
    #[doc(hidden)]
    pub fn index_dir_with_hook(
        &mut self,
        config: &IndexConfig,
        opts: &IndexOptions,
        after_parse: &dyn Fn(&Path),
    ) -> Result<IndexReport> {
        let mut report = IndexReport::default();
        let jobs = opts.jobs.max(1);

        let invalidate_all = if opts.reindex {
            report.reason = Some("--reindex given".to_string());
            true
        } else {
            report.reason = self.full_reindex_reason(config)?;
            report.reason.is_some()
        };

        let root = self.root.clone();
        let stored = self.stored_spellings()?;
        let mut seen: HashSet<String> = HashSet::new();
        let mut items: Vec<ParseItem> = Vec::new();
        let mut leader_of: HashMap<String, usize> = HashMap::new();

        // symlinks are not followed: a link to a directory invites cycles and
        // double-indexing, and `is_file()` is false for a link itself.
        for entry in WalkDir::new(&root).follow_links(false).sort_by_file_name() {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    eprintln!("warning: cannot read directory entry: {e}");
                    report.walk_errors += 1;
                    continue;
                }
            };
            if !entry.file_type().is_file() || !crate::is_supported(entry.path()) {
                continue;
            }
            let abs = entry.path().to_path_buf();
            let Some(found) = rel_path(&root, &abs) else {
                eprintln!(
                    "warning: skipping {}: path is not valid UTF-8",
                    abs.display()
                );
                report.walk_errors += 1;
                continue;
            };

            // Same file under a different casing (Windows only): move the
            // stored row to the spelling on disk rather than adding a second.
            if let Some(old) = stored.get(&path_key(&found)) {
                if *old != found && self.known_path(&found)?.is_none() {
                    self.rename_path(old, &found)?;
                }
            }
            seen.insert(found.clone());

            let walked = match stat(&abs) {
                Ok(w) => w,
                Err(e) => {
                    eprintln!("warning: {e}");
                    report.walk_errors += 1;
                    continue;
                }
            };

            let known = if invalidate_all {
                None
            } else {
                self.known_path(&found)?
            };

            let (action, hashed) = if invalidate_all {
                match hash_file(&abs) {
                    Ok(h) => (Action::Parse, Some(h)),
                    Err(e) => {
                        eprintln!("warning: {e}");
                        report.failed += 1;
                        continue;
                    }
                }
            } else {
                match decide_cheap(known.as_ref(), &walked, opts.retry_failed) {
                    Decision::Settled(a) => (a, None),
                    Decision::NeedsHash => match hash_file(&abs) {
                        Ok(h) => {
                            let owner = self.hash_owner(&h.0)?;
                            (
                                decide_after_hash(known.as_ref().map(|k| k.file_id), owner),
                                Some(h),
                            )
                        }
                        Err(e) => {
                            eprintln!("warning: {e}");
                            report.failed += 1;
                            continue;
                        }
                    },
                }
            };

            // `--retry-failed` must reach a failed file even though its
            // content hash matches the stored (failed) row.
            let action = match action {
                Action::TouchOnly { file_id } | Action::LinkExisting { file_id }
                    if opts.retry_failed && self.file_status(file_id)? == FileStatus::Failed =>
                {
                    Action::Parse
                }
                other => other,
            };

            match action {
                Action::Unchanged => report.unchanged += 1,
                Action::SkipFailed => report.skipped_failed += 1,
                Action::TouchOnly { .. } => {
                    self.touch_path(&found, walked.mtime_ns, walked.byte_size)?;
                    report.touched += 1;
                }
                Action::LinkExisting { file_id } => {
                    self.link_path(file_id, &found, walked.mtime_ns, walked.byte_size)?;
                    if self.file_status(file_id)? == FileStatus::Failed {
                        report.skipped_failed += 1;
                    } else {
                        report.linked += 1;
                    }
                }
                Action::Parse => {
                    let (hash, hashed_size) = hashed.expect("Parse is always preceded by a hash");
                    // Belt and braces: the post-parse re-stat is the real guard;
                    // this only catches a size change between stat and hash early.
                    if hashed_size != walked.byte_size {
                        // Grew or shrank between the stat and the read.
                        eprintln!(
                            "warning: {} changed while being read; not recorded this run",
                            abs.display()
                        );
                        report.modified_during_run += 1;
                        continue;
                    }
                    if let Some(&leader) = leader_of.get(&hash) {
                        items[leader].followers.push(Follower {
                            rel: found,
                            abs,
                            walked,
                        });
                    } else {
                        leader_of.insert(hash.clone(), items.len());
                        items.push(ParseItem {
                            rel: found,
                            abs,
                            hash,
                            walked,
                            followers: Vec::new(),
                        });
                    }
                }
            }
        }

        self.parse_and_store(&items, config, jobs, after_parse, &mut report)?;

        // A path missing from an incomplete walk may simply be unreadable;
        // pruning would delete good rows for files that still exist.
        if report.walk_errors == 0 {
            let (paths, files) = self.prune_unseen(&seen)?;
            report.paths_pruned = paths;
            report.files_pruned = files;
        }

        // Last, and only if every file was actually brought up to date: an
        // interrupted or partial run must leave the old fingerprint so the
        // next run re-invalidates instead of trusting stale chunks.
        if report.walk_errors == 0 && report.modified_during_run == 0 {
            self.record_config(config)?;
        }

        Ok(report)
    }

    /// Parses `items` on a pool and stores each result as it arrives.
    ///
    /// Results travel over a bounded channel, so at most about `jobs`
    /// documents' chunks exist at once and each is dropped after its write.
    fn parse_and_store(
        &mut self,
        items: &[ParseItem],
        config: &IndexConfig,
        jobs: usize,
        after_parse: &dyn Fn(&Path),
        report: &mut IndexReport,
    ) -> Result<()> {
        if items.is_empty() {
            return Ok(());
        }

        // No silent fallback to one thread if the pool cannot start.
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(jobs)
            .build()
            .map_err(|e| Error::Index {
                detail: format!("could not start {jobs} workers: {e}"),
            })?;

        type Parsed = (usize, std::result::Result<Vec<Chunk>, String>);
        let (tx, rx) = mpsc::sync_channel::<Parsed>(jobs);
        let config = *config;

        std::thread::scope(|scope| -> Result<()> {
            scope.spawn(move || {
                pool.install(|| {
                    // `try_for_each_with` stops handing out work once the
                    // receiver is gone (a database error on the main thread).
                    let _ = items
                        .par_iter()
                        .enumerate()
                        .try_for_each_with(tx, |tx, (i, item)| {
                            let outcome = parse_one(&item.abs, &config).map_err(|e| e.to_string());
                            tx.send((i, outcome)).map_err(|_| ())
                        });
                });
            });

            for (i, outcome) in rx {
                let item = &items[i];
                after_parse(&item.abs);

                // Re-stat AFTER the parse. If the file moved under us, the
                // chunks may not belong to the hash we computed, and storing
                // them as current would make the file look fresh forever.
                if !unchanged_since(&item.abs, &item.walked) {
                    eprintln!(
                        "warning: {} changed during the run; not recorded, will be re-read next run",
                        item.abs.display()
                    );
                    report.modified_during_run += 1 + item.followers.len();
                    continue;
                }

                let (file_id, ok) = match outcome {
                    Ok(chunks) => {
                        let id = self.store_parsed(
                            &item.hash,
                            item.walked.byte_size,
                            None,
                            &chunks,
                            &item.rel,
                            item.walked.mtime_ns,
                        )?;
                        report.parsed += 1;
                        (id, true)
                    }
                    Err(message) => {
                        eprintln!("error: {message}");
                        let id = self.store_failure(
                            &item.hash,
                            item.walked.byte_size,
                            &message,
                            &item.rel,
                            item.walked.mtime_ns,
                        )?;
                        report.failed += 1;
                        (id, false)
                    }
                };

                for f in &item.followers {
                    if !unchanged_since(&f.abs, &f.walked) {
                        eprintln!(
                            "warning: {} changed during the run; not recorded",
                            f.abs.display()
                        );
                        report.modified_during_run += 1;
                        continue;
                    }
                    self.link_path(file_id, &f.rel, f.walked.mtime_ns, f.walked.byte_size)?;
                    if ok {
                        report.linked += 1;
                    } else {
                        report.failed += 1;
                    }
                }
            }
            Ok(())
        })
    }

    /// Stored spelling of each path, keyed by comparison key. Only Windows
    /// needs it; elsewhere the exact spelling is the key.
    #[cfg(windows)]
    fn stored_spellings(&self) -> Result<HashMap<String, String>> {
        let db = |e: rusqlite::Error| Error::Index {
            detail: format!("cannot list paths: {e}"),
        };
        let all: Vec<String> = self
            .conn
            .prepare("SELECT rel_path FROM paths")
            .map_err(db)?
            .query_map([], |r| r.get(0))
            .map_err(db)?
            .collect::<std::result::Result<_, _>>()
            .map_err(db)?;
        Ok(all.into_iter().map(|p| (path_key(&p), p)).collect())
    }

    #[cfg(not(windows))]
    fn stored_spellings(&self) -> Result<HashMap<String, String>> {
        Ok(HashMap::new())
    }

    fn rename_path(&mut self, old: &str, new: &str) -> Result<()> {
        self.conn
            .execute(
                "UPDATE paths SET rel_path = ?1 WHERE rel_path = ?2",
                rusqlite::params![new, old],
            )
            .map_err(|e| Error::Index {
                detail: format!("cannot rename {old} to {new}: {e}"),
            })?;
        Ok(())
    }

    fn file_status(&self, file_id: i64) -> Result<FileStatus> {
        let status: String = self
            .conn
            .query_row(
                "SELECT status FROM files WHERE id = ?1",
                rusqlite::params![file_id],
                |r| r.get(0),
            )
            .map_err(|e| Error::Index {
                detail: format!("cannot read status of file {file_id}: {e}"),
            })?;
        match status.as_str() {
            "indexed" => Ok(FileStatus::Indexed),
            "failed" => Ok(FileStatus::Failed),
            other => Err(Error::Index {
                detail: format!("unknown file status `{other}` in index"),
            }),
        }
    }
}

/// What a directory looks like relative to its index. Produced by
/// `Index::status`, which reads and hashes but never parses or writes.
#[derive(Debug, Clone, PartialEq)]
pub struct StatusReport {
    /// Content is current (includes files only touched, not changed).
    pub unchanged: Vec<String>,
    /// Known paths whose content changed.
    pub modified: Vec<String>,
    /// Paths the index has not seen, including new paths for known content.
    pub new_files: Vec<String>,
    /// Indexed paths that are no longer on disk.
    pub deleted: Vec<String>,
    /// Paths recorded as failed, with the recorded reason.
    pub failed: Vec<(String, String)>,
    pub staleness: Staleness,
    /// Set when `index` would re-parse everything, with the same wording
    /// `index` prints.
    pub stale_reason: Option<String>,
    /// Entries that could not be read. When non-zero the walk was incomplete
    /// and `deleted` is left empty, since an unreadable file is not a deleted one.
    pub unreadable: usize,
}

impl Index {
    /// Why every stored result must be discarded, if it must. The single
    /// source of this rule: `index_dir` acts on it, `status` reports it.
    fn full_reindex_reason(&self, config: &IndexConfig) -> Result<Option<String>> {
        Ok(match self.staleness(config)? {
            Staleness::Current => None,
            // An empty index with no settings is simply new. Rows with no
            // settings come from a run that never finished; what they were
            // parsed with is unknown, so they cannot be trusted.
            Staleness::FirstRun => {
                if self.path_count()? > 0 {
                    Some("earlier run did not finish; its settings were never recorded".to_string())
                } else {
                    None
                }
            }
            Staleness::ConfigChanged { from, to } => {
                Some(format!("settings changed ({from} -> {to})"))
            }
            Staleness::ExtractionChanged { from, to } => {
                Some(format!("extraction version changed ({from} -> {to})"))
            }
        })
    }

    /// Compares the directory with the index. Hashes where `index` would, so
    /// the two agree; opens no document; writes nothing.
    pub fn status(&self, config: &IndexConfig) -> Result<StatusReport> {
        let staleness = self.staleness(config)?;
        let stale_reason = self.full_reindex_reason(config)?;
        let stored = self.all_stored_paths()?;
        let stored_by_key: HashMap<String, String> =
            stored.iter().map(|p| (path_key(p), p.clone())).collect();

        let mut report = StatusReport {
            unchanged: Vec::new(),
            modified: Vec::new(),
            new_files: Vec::new(),
            deleted: Vec::new(),
            failed: Vec::new(),
            staleness,
            stale_reason,
            unreadable: 0,
        };
        let mut seen: HashSet<String> = HashSet::new();

        for entry in WalkDir::new(&self.root)
            .follow_links(false)
            .sort_by_file_name()
        {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    eprintln!("warning: cannot read directory entry: {e}");
                    report.unreadable += 1;
                    continue;
                }
            };
            if !entry.file_type().is_file() || !crate::is_supported(entry.path()) {
                continue;
            }
            let abs = entry.path();
            let Some(found) = rel_path(&self.root, abs) else {
                eprintln!(
                    "warning: skipping {}: path is not valid UTF-8",
                    abs.display()
                );
                report.unreadable += 1;
                continue;
            };
            // Look the path up under the spelling the index stored (matters
            // on case-insensitive Windows), but report the spelling on disk.
            let lookup = stored_by_key
                .get(&path_key(&found))
                .cloned()
                .unwrap_or_else(|| found.clone());
            seen.insert(path_key(&found));

            let walked = match stat(abs) {
                Ok(w) => w,
                Err(e) => {
                    eprintln!("warning: {e}");
                    report.unreadable += 1;
                    continue;
                }
            };
            let known = self.known_path(&lookup)?;

            let action = match decide_cheap(known.as_ref(), &walked, false) {
                Decision::Settled(a) => a,
                Decision::NeedsHash => match hash_file(abs) {
                    Ok((hash, _)) => {
                        let owner = self.hash_owner(&hash)?;
                        decide_after_hash(known.as_ref().map(|k| k.file_id), owner)
                    }
                    Err(e) => {
                        eprintln!("warning: {e}");
                        report.unreadable += 1;
                        continue;
                    }
                },
            };

            match action {
                Action::Unchanged => report.unchanged.push(found),
                Action::SkipFailed => {
                    let id = known.as_ref().map(|k| k.file_id);
                    report.failed.push((found, self.failure_reason(id)?));
                }
                // The two functions are each right alone but a failed file
                // whose content is unchanged lands here, where "content is
                // current" would hide that it never parsed.
                Action::TouchOnly { file_id } | Action::LinkExisting { file_id }
                    if self.file_status(file_id)? == FileStatus::Failed =>
                {
                    report
                        .failed
                        .push((found, self.failure_reason(Some(file_id))?));
                }
                Action::TouchOnly { .. } => report.unchanged.push(found),
                Action::LinkExisting { .. } => report.new_files.push(found),
                Action::Parse => {
                    if known.is_some() {
                        report.modified.push(found);
                    } else {
                        report.new_files.push(found);
                    }
                }
            }
        }

        if report.unreadable == 0 {
            report.deleted = stored
                .into_iter()
                .filter(|p| !seen.contains(&path_key(p)))
                .collect();
            report.deleted.sort();
        }
        Ok(report)
    }

    fn all_stored_paths(&self) -> Result<Vec<String>> {
        let db = |e: rusqlite::Error| Error::Index {
            detail: format!("cannot list paths: {e}"),
        };
        let rows = self
            .conn
            .prepare("SELECT rel_path FROM paths ORDER BY rel_path")
            .map_err(db)?
            .query_map([], |r| r.get(0))
            .map_err(db)?
            .collect::<std::result::Result<Vec<String>, _>>()
            .map_err(db)?;
        Ok(rows)
    }

    fn failure_reason(&self, file_id: Option<i64>) -> Result<String> {
        let Some(id) = file_id else {
            return Ok("unknown".to_string());
        };
        let reason: Option<String> = self
            .conn
            .query_row("SELECT error FROM files WHERE id = ?1", [id], |r| r.get(0))
            .map_err(|e| Error::Index {
                detail: format!("cannot read failure reason of file {id}: {e}"),
            })?;
        Ok(reason.unwrap_or_else(|| "unknown".to_string()))
    }
}
