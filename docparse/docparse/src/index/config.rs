//! The settings that affect extraction output, and how staleness is decided.

use super::{Index, EXTRACTION_VERSION};
use crate::chunk::ChunkConfig;
use crate::error::{Error, Result};

/// Everything that changes what a parse produces. A change to any of these
/// invalidates every stored chunk.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IndexConfig {
    pub chunk: ChunkConfig,
    pub ocr: bool,
    pub ocr_dpi: u32,
}

impl IndexConfig {
    /// A canonical, human-readable rendering of the settings. Stored next to
    /// the hash so `status` can say which setting changed.
    pub fn describe(&self) -> String {
        format!(
            "chunk={} max={} overlap={} cross_page={} ocr={} ocr_dpi={}",
            self.chunk.target_chars,
            self.chunk.max_chars,
            self.chunk.overlap_sentences,
            self.chunk.cross_page,
            self.ocr,
            self.ocr_dpi,
        )
    }

    /// Hash of `describe()`. Formatting the values first keeps the digest
    /// stable across runs and machines.
    pub fn fingerprint(&self) -> String {
        blake3::hash(self.describe().as_bytes())
            .to_hex()
            .to_string()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Staleness {
    /// Nothing recorded yet.
    FirstRun,
    /// Settings and extraction version both match.
    Current,
    /// A setting that affects output changed; everything must be re-parsed.
    ConfigChanged { from: String, to: String },
    /// Extraction itself changed; everything must be re-parsed.
    ExtractionChanged { from: u32, to: u32 },
}

impl Index {
    /// Decides, before the walk begins, whether every file is already stale.
    ///
    /// `ExtractionChanged` is checked before `ConfigChanged`: a changed
    /// extraction invalidates everything regardless of settings, and is the
    /// more informative report. Do not reorder.
    ///
    /// A fingerprint with no recorded extraction version means `record_config`
    /// was interrupted; that is treated as version 0 (stale), never as current.
    pub fn staleness(&self, config: &IndexConfig) -> Result<Staleness> {
        let stored_fp = self.meta("config_fingerprint")?;
        let Some(stored_fp) = stored_fp else {
            return Ok(Staleness::FirstRun);
        };

        let stored_ev: u32 = match self.meta("extraction_version")? {
            Some(v) => v.parse().map_err(|_| Error::Index {
                detail: format!(
                    "unparseable extraction_version `{v}`. Use --reindex to rebuild it."
                ),
            })?,
            // Assuming the current version here would serve chunks built by an
            // unknown extraction build while reporting them as current. Zero
            // forces a full re-index, which is the safe direction.
            None => 0,
        };
        if stored_ev != EXTRACTION_VERSION {
            return Ok(Staleness::ExtractionChanged {
                from: stored_ev,
                to: EXTRACTION_VERSION,
            });
        }

        if stored_fp != config.fingerprint() {
            return Ok(Staleness::ConfigChanged {
                from: self.meta("config_describe")?.unwrap_or(stored_fp),
                to: config.describe(),
            });
        }

        Ok(Staleness::Current)
    }

    /// Records the settings. All four writes happen in ONE transaction so the
    /// fingerprint and extraction version land together: a partial write
    /// (fingerprint without version) would make `staleness` misjudge the index.
    /// Callers should invoke this only after a walk completes.
    pub fn record_config(&mut self, config: &IndexConfig) -> Result<()> {
        let db = |e: rusqlite::Error| Error::Index {
            detail: format!("cannot record config: {e}"),
        };
        let tx = self.conn.transaction().map_err(db)?;
        let rows = [
            ("config_fingerprint", config.fingerprint()),
            ("config_describe", config.describe()),
            ("extraction_version", EXTRACTION_VERSION.to_string()),
            ("updated_at", super::now_iso8601()),
        ];
        for (k, v) in rows {
            tx.execute(
                "INSERT INTO meta(key, value) VALUES(?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                rusqlite::params![k, v],
            )
            .map_err(db)?;
        }
        tx.commit().map_err(db)?;
        Ok(())
    }
}
