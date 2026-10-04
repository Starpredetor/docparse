#![cfg(feature = "image")]

use std::path::PathBuf;

#[test]
fn missing_image_is_an_io_error_not_a_malformed_document() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/does_not_exist.png");
    let err = docparse::image::ImageSource::open(&path).unwrap_err();
    assert!(matches!(err, docparse::Error::Io { .. }), "got {err:?}");
}
