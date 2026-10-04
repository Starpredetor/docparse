//! OCR via the `tesseract` executable.
//!
//! Shelling out rather than linking `libtesseract`: linking needs Leptonica
//! and Tesseract development headers (a build burden on Windows) and the
//! process-spawn cost is negligible next to OCR itself.
//!
//! The binary is resolved in this order:
//! 1. the `DOCPARSE_TESSERACT` environment variable (full path to the executable)
//! 2. `tesseract` on `PATH`
//! 3. known install locations (Windows: `Program Files` and `Program Files (x86)`
//!    under `Tesseract-OCR`; Unix: `/usr/bin` and `/usr/local/bin`)

//!
//! # Memory
//!
//! The 64 MB budget does NOT hold under OCR. Measured (release, A4 scans at
//! 150 DPI, peak working set): `--ocr --jobs 1` is about 52 MB in-process plus
//! a separate tesseract process of about 62 MB, flat from 8 to 24 pages;
//! `--ocr --jobs 4` is about 152 MB in-process and about 213 MB including
//! tesseract. Budget roughly 50 MB plus a comparable tesseract per worker.
//! The bitmap grows with the square of `--ocr-dpi`.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::{Error, Result};
use crate::model::{Block, BlockKind};
use crate::normalize::normalize_block_text;

/// Environment variable naming the tesseract executable.
pub const TESSERACT_ENV: &str = "DOCPARSE_TESSERACT";

#[cfg(windows)]
const KNOWN_LOCATIONS: &[&str] = &[
    r"C:\Program Files\Tesseract-OCR\tesseract.exe",
    r"C:\Program Files (x86)\Tesseract-OCR\tesseract.exe",
];
#[cfg(not(windows))]
const KNOWN_LOCATIONS: &[&str] = &["/usr/bin/tesseract", "/usr/local/bin/tesseract"];

fn backend_err(detail: String) -> Error {
    Error::Backend {
        backend: "tesseract",
        detail,
    }
}

/// Finds a usable tesseract executable using the real environment.
fn resolve_tesseract() -> Result<PathBuf> {
    resolve_from(
        std::env::var_os(TESSERACT_ENV),
        Path::new("tesseract"),
        KNOWN_LOCATIONS,
    )
}

/// The resolution order with its inputs injected, so the failure path can be
/// tested without depending on what is installed on the machine.
fn resolve_from(
    env_override: Option<std::ffi::OsString>,
    path_name: &Path,
    known: &[&str],
) -> Result<PathBuf> {
    let mut tried: Vec<String> = Vec::new();

    match env_override.filter(|v| !v.is_empty()) {
        Some(value) => {
            let path = PathBuf::from(&value);
            if runs(&path) {
                return Ok(path);
            }
            tried.push(format!("{TESSERACT_ENV}={} (not runnable)", path.display()));
        }
        None => tried.push(format!("{TESSERACT_ENV} (not set)")),
    }

    if runs(path_name) {
        return Ok(path_name.to_path_buf());
    }
    tried.push(format!("`{}` on PATH (not found)", path_name.display()));

    for location in known {
        let path = PathBuf::from(location);
        if path.is_file() && runs(&path) {
            return Ok(path);
        }
        tried.push(format!("{location} (not found)"));
    }

    Err(backend_err(format!(
        "tesseract executable not found. Install Tesseract, or set {TESSERACT_ENV} to its full path. Search order: {}",
        tried.join("; ")
    )))
}

fn runs(exe: &Path) -> bool {
    Command::new(exe)
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn run_tesseract(exe: &Path, image: &Path, tsv: bool) -> Result<String> {
    let mut cmd = Command::new(exe);
    cmd.arg(image).arg("stdout");
    if tsv {
        cmd.arg("tsv");
    }
    let output = cmd
        .output()
        .map_err(|e| backend_err(format!("could not run {}: {e}", exe.display())))?;
    if !output.status.success() {
        return Err(backend_err(format!(
            "{} exited with {} on {}: {}",
            exe.display(),
            output.status,
            image.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Mean word confidence from tesseract TSV output, in `0.0..=1.0`.
///
/// Rows with empty text or a negative confidence (layout rows) are skipped.
/// With no qualifying word the result is `0.0`, not a flattering default.
fn mean_confidence(tsv: &str) -> f32 {
    let (mut sum, mut n) = (0.0f32, 0u32);
    for line in tsv.lines().skip(1) {
        let cols: Vec<&str> = line.split('\t').collect();
        // 12 columns: ..., conf (index 10), text (index 11).
        let (Some(conf), Some(text)) = (cols.get(10), cols.get(11)) else {
            continue;
        };
        if text.trim().is_empty() {
            continue;
        }
        if let Ok(c) = conf.trim().parse::<f32>() {
            if c >= 0.0 {
                sum += c;
                n += 1;
            }
        }
    }
    if n == 0 {
        0.0
    } else {
        (sum / n as f32 / 100.0).clamp(0.0, 1.0)
    }
}

/// Returns the recognized text and a mean confidence in `0.0..=1.0`.
///
/// Runs `tesseract <image> stdout` for the text and
/// `tesseract <image> stdout tsv` for the confidences.
pub fn ocr_image_file(path: &Path) -> Result<(String, f32)> {
    let exe = resolve_tesseract()?;
    let text = run_tesseract(&exe, path, false)?;
    let tsv = run_tesseract(&exe, path, true)?;
    Ok((text, mean_confidence(&tsv)))
}

/// Turns raw OCR text into blocks. The one place OCR output is normalized:
/// whole-page OCR gives a single `Other` block with no geometry, and
/// whitespace-only output gives none rather than an empty block.
pub fn blocks_from_ocr_text(text: &str) -> Vec<Block> {
    let cleaned = normalize_block_text(text);
    if cleaned.is_empty() {
        return Vec::new();
    }
    vec![Block {
        id: 0,
        bbox: None,
        kind: BlockKind::Other,
        text: cleaned,
    }]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_tesseract_is_a_backend_error_naming_the_search_order() {
        let err = resolve_from(
            Some("Z:/nope/tesseract.exe".into()),
            Path::new("definitely-not-a-real-tesseract"),
            &["Z:/also/nope"],
        )
        .unwrap_err();
        let Error::Backend { backend, detail } = &err else {
            panic!("expected Backend, got {err:?}");
        };
        assert_eq!(*backend, "tesseract");
        for needle in [
            "DOCPARSE_TESSERACT",
            "Z:/nope/tesseract.exe",
            "PATH",
            "Z:/also/nope",
        ] {
            assert!(detail.contains(needle), "{needle} missing from {detail}");
        }
    }

    #[test]
    fn ocr_text_is_normalized_into_one_block() {
        let raw = "It\u{a0}said  \u{201c}hi\u{201d}   there\n";
        let blocks = blocks_from_ocr_text(raw);
        assert_eq!(blocks.len(), 1);
        assert!(!blocks[0].text.contains('\u{a0}'), "{:?}", blocks[0].text);
        assert!(!blocks[0].text.contains("  "), "{:?}", blocks[0].text);
        assert_eq!(blocks[0].text, normalize_block_text(raw));
    }

    #[test]
    fn whitespace_only_ocr_output_yields_no_blocks() {
        assert!(blocks_from_ocr_text(" \n\u{a0}\t ").is_empty());
    }

    #[test]
    fn confidence_averages_words_and_ignores_layout_rows() {
        let tsv = "level\tpage_num\tblock_num\tpar_num\tline_num\tword_num\tleft\ttop\twidth\theight\tconf\ttext\n\
                   1\t1\t0\t0\t0\t0\t0\t0\t10\t10\t-1\t\n\
                   5\t1\t1\t1\t1\t1\t0\t0\t10\t10\t90\tfoo\n\
                   5\t1\t1\t1\t1\t2\t0\t0\t10\t10\t70\tbar\n";
        assert!((mean_confidence(tsv) - 0.8).abs() < 1e-6);
    }

    #[test]
    fn no_words_means_zero_confidence() {
        assert_eq!(mean_confidence("header\n"), 0.0);
    }
}
