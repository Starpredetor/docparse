#![cfg(all(feature = "image", not(feature = "ocr")))]

use std::path::PathBuf;

#[test]
fn image_without_ocr_feature_is_an_error_not_an_empty_page() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/hello_ocr.png");
    let err = docparse::image::ImageSource::open(&path).unwrap_err();
    assert!(
        matches!(err, docparse::Error::OcrUnavailable { .. }),
        "got {err:?}"
    );
}
