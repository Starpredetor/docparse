#![cfg(feature = "ocr")]

use std::path::PathBuf;

use docparse::image::ImageSource;
use docparse::{PageOrigin, Source};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

#[test]
fn ocr_binary_resolves_and_reports_nonzero_confidence() {
    // Exercises env / PATH / known-location resolution through a real run.
    let (_, conf) = docparse::ocr::ocr_image_file(&fixture("hello_ocr.png")).unwrap();
    assert!(conf > 0.0, "a legible fixture must have nonzero confidence");
}

#[test]
fn reads_text_from_an_image_and_marks_the_origin_as_ocr() {
    let mut source = ImageSource::open(&fixture("hello_ocr.png")).unwrap();
    let page = source.next_page().unwrap().unwrap();

    assert_eq!(page.number, 1);
    let PageOrigin::Ocr { confidence } = page.origin else {
        panic!("origin must record an OCR guess, got {:?}", page.origin);
    };
    assert!((0.0..=1.0).contains(&confidence), "confidence {confidence}");
    eprintln!("measured confidence: {confidence}");

    let text: String = page
        .blocks
        .iter()
        .map(|b| b.text.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(text.to_lowercase().contains("hello"), "got {text:?}");
    assert!(source.next_page().is_none(), "an image is a single page");
    assert_eq!(page.blocks[0].kind, docparse::BlockKind::Other);
}

#[test]
fn an_unreadable_image_is_an_error_not_empty_text() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("broken.png");
    std::fs::write(&path, b"not a png").unwrap();
    let err = ImageSource::open(&path).unwrap_err();
    assert!(
        matches!(err, docparse::Error::Malformed { .. }),
        "got {err:?}"
    );
}
