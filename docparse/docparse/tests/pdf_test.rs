#![cfg(feature = "pdf")]

use std::path::PathBuf;

use docparse::pdf::PdfSource;
use docparse::{PageOrigin, Source};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

#[test]
fn pdfium_is_available() {
    // Goes through the same singleton the library uses, rather than binding
    // independently: two bind paths in one process fail with
    // PdfiumLibraryBindingsAlreadyInitialized under --test-threads=1.
    let result = PdfSource::open(&fixture("two_pages.pdf"));
    assert!(
        result.is_ok(),
        "PDFium unavailable. pdfium.dll must be beside the executable or on          PATH, and its build must match or postdate the pdfium-render feature          compiled in; see the comment in Cargo.toml. Error: {:?}",
        result.err()
    );
}

fn joined(page: &docparse::Page) -> String {
    page.blocks
        .iter()
        .map(|b| b.text.as_str())
        .collect::<Vec<_>>()
        .join(" ")
}

#[test]
fn extracts_text_from_each_page_with_real_page_numbers() {
    let mut source = PdfSource::open(&fixture("two_pages.pdf")).unwrap();

    let first = source.next_page().unwrap().unwrap();
    assert_eq!(first.number, 1);
    assert_eq!(first.origin, PageOrigin::Native);
    assert!(
        first.width > 0.0 && first.height > 0.0,
        "page geometry missing"
    );

    let text = joined(&first);
    assert!(text.contains("First page first line."), "got {text:?}");
    assert!(text.contains("First page second line."), "got {text:?}");

    let second = source.next_page().unwrap().unwrap();
    assert_eq!(second.number, 2);
    let text = joined(&second);
    assert!(text.contains("Second page content here."), "got {text:?}");

    assert!(source.next_page().is_none(), "expected exactly two pages");
}

#[test]
fn blocks_carry_bounding_boxes_unlike_docx() {
    let mut source = PdfSource::open(&fixture("two_pages.pdf")).unwrap();
    let page = source.next_page().unwrap().unwrap();
    assert!(!page.blocks.is_empty());

    for block in &page.blocks {
        let bbox = block.bbox.expect("PDF blocks must have geometry");
        assert!(bbox.x1 > bbox.x0, "degenerate bbox: {bbox:?}");
        assert!(bbox.y1 > bbox.y0, "degenerate bbox: {bbox:?}");
    }
}

#[test]
fn a_page_with_no_text_yields_no_blocks_rather_than_a_placeholder() {
    let mut source = PdfSource::open(&fixture("blank.pdf")).unwrap();
    let page = source.next_page().unwrap().unwrap();

    // Honest emptiness. OCR is opt-in and lives elsewhere.
    assert!(page.blocks.is_empty());
    assert_eq!(page.origin, PageOrigin::Native);
}

#[test]
fn a_nonexistent_file_is_an_io_error() {
    let err = PdfSource::open(&fixture("does_not_exist.pdf")).unwrap_err();
    assert!(matches!(err, docparse::Error::Io { .. }), "got {err:?}");
}

#[test]
fn a_file_that_is_not_a_pdf_is_malformed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fake.pdf");
    std::fs::write(&path, b"definitely not a pdf").unwrap();

    let err = PdfSource::open(&path).unwrap_err();
    assert!(
        matches!(err, docparse::Error::Malformed { .. }),
        "got {err:?}"
    );
}

#[test]
fn block_text_is_normalized() {
    let mut source = PdfSource::open(&fixture("messy_text.pdf")).unwrap();
    let page = source.next_page().unwrap().unwrap();
    let text = joined(&page);
    assert_eq!(text, "He said \"hello\" - it's fine here.");
}

#[test]
fn pdf_blocks_are_lines_not_paragraphs() {
    let mut source = PdfSource::open(&fixture("two_pages.pdf")).unwrap();
    let page = source.next_page().unwrap().unwrap();
    assert!(page
        .blocks
        .iter()
        .all(|b| b.kind == docparse::BlockKind::Line));
}
