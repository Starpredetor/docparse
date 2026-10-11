//! Change detection, as pure functions over plain data.
//!
//! No filesystem, no database. The caller supplies what it found on disk and
//! what the index already knows; these functions decide what to do. That
//! keeps the part of the feature most able to corrupt an index testable at a
//! table, with nothing to mock.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    Indexed,
    Failed,
}

/// What the index already records for one path.
#[derive(Debug, Clone, PartialEq)]
pub struct KnownPath {
    pub file_id: i64,
    pub byte_size: u64,
    /// Nanoseconds since the Unix epoch. Signed: archives contain files
    /// dated before 1970.
    pub mtime_ns: i128,
    pub status: FileStatus,
}

/// What the walk found on disk.
#[derive(Debug, Clone, PartialEq)]
pub struct WalkedFile {
    pub byte_size: u64,
    pub mtime_ns: i128,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Size and mtime both match. Do not read the file.
    Unchanged,
    /// Content is identical but the timestamp moved. Refresh mtime only.
    TouchOnly { file_id: i64 },
    /// Content is new to this index. Parse it.
    Parse,
    /// This content is already indexed. Point the path at it; do not parse.
    LinkExisting { file_id: i64 },
    /// Recorded as failed and unchanged since. Skip unless asked to retry.
    SkipFailed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Settled(Action),
    /// Cannot decide without the content hash; the caller must hash the file.
    NeedsHash,
}

/// Decides what can be decided without reading the file.
///
/// Returning `Settled(Unchanged)` from size and mtime alone is what makes a
/// no-op run fast; everything else here exists to avoid trusting mtime.
pub fn decide_cheap(
    known: Option<&KnownPath>,
    walked: &WalkedFile,
    retry_failed: bool,
) -> Decision {
    let Some(known) = known else {
        return Decision::NeedsHash;
    };

    let same_stat = known.byte_size == walked.byte_size && known.mtime_ns == walked.mtime_ns;

    match known.status {
        FileStatus::Failed => {
            if same_stat && !retry_failed {
                Decision::Settled(Action::SkipFailed)
            } else {
                // Changed on disk, or the user asked for a retry.
                Decision::NeedsHash
            }
        }
        FileStatus::Indexed => {
            if same_stat {
                Decision::Settled(Action::Unchanged)
            } else {
                Decision::NeedsHash
            }
        }
    }
}

/// Decides once the content hash is known.
///
/// `known_file_id` is the file this path currently points at, if any.
/// `hash_owner` is the file that already holds this content hash, if any.
pub fn decide_after_hash(known_file_id: Option<i64>, hash_owner: Option<i64>) -> Action {
    match (known_file_id, hash_owner) {
        // Same path, same content: only the timestamp moved.
        (Some(a), Some(b)) if a == b => Action::TouchOnly { file_id: a },
        // This content exists elsewhere in the index: rename, copy, or an
        // overwrite with bytes we have already parsed.
        (_, Some(owner)) => Action::LinkExisting { file_id: owner },
        // Content new to the index.
        (_, None) => Action::Parse,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn known(file_id: i64, size: u64, mtime: i128, status: FileStatus) -> KnownPath {
        KnownPath {
            file_id,
            byte_size: size,
            mtime_ns: mtime,
            status,
        }
    }

    fn walked(size: u64, mtime: i128) -> WalkedFile {
        WalkedFile {
            byte_size: size,
            mtime_ns: mtime,
        }
    }

    // --- decide_cheap ---------------------------------------------------

    #[test]
    fn an_unchanged_indexed_file_needs_no_read() {
        let k = known(7, 100, 1_000, FileStatus::Indexed);
        assert_eq!(
            decide_cheap(Some(&k), &walked(100, 1_000), false),
            Decision::Settled(Action::Unchanged)
        );
    }

    #[test]
    fn a_changed_size_demands_a_hash() {
        let k = known(7, 100, 1_000, FileStatus::Indexed);
        assert_eq!(
            decide_cheap(Some(&k), &walked(101, 1_000), false),
            Decision::NeedsHash
        );
    }

    #[test]
    fn a_changed_mtime_demands_a_hash() {
        // mtime alone is never trusted: sync clients and checkouts move it
        // without changing content, and coarse filesystems lie about it.
        let k = known(7, 100, 1_000, FileStatus::Indexed);
        assert_eq!(
            decide_cheap(Some(&k), &walked(100, 2_000), false),
            Decision::NeedsHash
        );
    }

    #[test]
    fn an_unknown_path_demands_a_hash() {
        assert_eq!(
            decide_cheap(None, &walked(100, 1_000), false),
            Decision::NeedsHash
        );
    }

    #[test]
    fn a_previously_failed_unchanged_file_is_skipped() {
        // A corrupt PDF must not cost parse time on every single run.
        let k = known(7, 100, 1_000, FileStatus::Failed);
        assert_eq!(
            decide_cheap(Some(&k), &walked(100, 1_000), false),
            Decision::Settled(Action::SkipFailed)
        );
    }

    #[test]
    fn retry_failed_forces_a_reattempt_of_an_unchanged_failure() {
        let k = known(7, 100, 1_000, FileStatus::Failed);
        assert_eq!(
            decide_cheap(Some(&k), &walked(100, 1_000), true),
            Decision::NeedsHash
        );
    }

    #[test]
    fn a_failed_file_that_changed_is_reattempted_without_the_flag() {
        // The user fixed it; they should not need to remember a flag.
        let k = known(7, 100, 1_000, FileStatus::Failed);
        assert_eq!(
            decide_cheap(Some(&k), &walked(250, 9_000), false),
            Decision::NeedsHash
        );
    }

    #[test]
    fn a_negative_mtime_is_handled() {
        // Archives really do contain files dated before 1970.
        let k = known(7, 100, -86_400_000_000_000, FileStatus::Indexed);
        assert_eq!(
            decide_cheap(Some(&k), &walked(100, -86_400_000_000_000), false),
            Decision::Settled(Action::Unchanged)
        );
    }

    #[test]
    fn retry_failed_does_not_force_a_read_of_an_unchanged_indexed_file() {
        // `--retry-failed` targets recorded failures. If it also re-hashed
        // healthy files, every run with the flag would become a full read of
        // the corpus, which is the opposite of this feature's purpose.
        let k = known(7, 100, 1_000, FileStatus::Indexed);
        assert_eq!(
            decide_cheap(Some(&k), &walked(100, 1_000), true),
            Decision::Settled(Action::Unchanged)
        );
    }

    #[test]
    fn an_unknown_path_still_demands_a_hash_when_retrying() {
        assert_eq!(
            decide_cheap(None, &walked(100, 1_000), true),
            Decision::NeedsHash
        );
    }

    #[test]
    fn a_changed_failed_file_demands_a_hash_when_retrying() {
        let k = known(7, 100, 1_000, FileStatus::Failed);
        assert_eq!(
            decide_cheap(Some(&k), &walked(250, 9_000), true),
            Decision::NeedsHash
        );
    }

    #[test]
    fn a_failed_file_with_only_a_size_change_demands_a_hash() {
        let k = known(7, 100, 1_000, FileStatus::Failed);
        assert_eq!(
            decide_cheap(Some(&k), &walked(101, 1_000), false),
            Decision::NeedsHash
        );
    }

    #[test]
    fn a_failed_file_with_only_an_mtime_change_demands_a_hash() {
        let k = known(7, 100, 1_000, FileStatus::Failed);
        assert_eq!(
            decide_cheap(Some(&k), &walked(100, 2_000), false),
            Decision::NeedsHash
        );
    }

    #[test]
    fn indexed_file_that_changed_still_demands_a_hash_when_retrying() {
        // `--retry-failed` must not make a genuinely modified file look
        // unchanged. The Indexed arm ignores the flag; this pins that.
        let k = known(7, 100, 1_000, FileStatus::Indexed);
        assert_eq!(
            decide_cheap(Some(&k), &walked(250, 9_000), true),
            Decision::NeedsHash
        );
    }

    // --- decide_after_hash ---------------------------------------------

    #[test]
    fn content_unchanged_despite_a_moved_mtime_only_touches() {
        assert_eq!(
            decide_after_hash(Some(7), Some(7)),
            Action::TouchOnly { file_id: 7 }
        );
    }

    #[test]
    fn brand_new_content_is_parsed() {
        assert_eq!(decide_after_hash(None, None), Action::Parse);
    }

    #[test]
    fn a_renamed_or_copied_file_links_instead_of_parsing() {
        // This is what makes a rename free, and what stores a duplicated
        // corpus once instead of twice.
        assert_eq!(
            decide_after_hash(None, Some(3)),
            Action::LinkExisting { file_id: 3 }
        );
    }

    #[test]
    fn a_path_overwritten_with_already_indexed_content_relinks() {
        // a.pdf was file 1; someone overwrote it with b.pdf's bytes, which
        // are already file 2. Relink, do not re-parse.
        assert_eq!(
            decide_after_hash(Some(1), Some(2)),
            Action::LinkExisting { file_id: 2 }
        );
    }

    #[test]
    fn a_known_path_with_new_content_is_parsed() {
        assert_eq!(decide_after_hash(Some(1), None), Action::Parse);
    }
}
