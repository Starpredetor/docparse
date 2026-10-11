use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn binary() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    path.join(format!("docparse{}", std::env::consts::EXE_SUFFIX))
}

fn write_docx(dir: &Path, name: &str, paragraphs: &[&str]) {
    let file = std::fs::File::create(dir.join(name)).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    let opts: zip::write::FileOptions<'_, ()> =
        zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    let body: String = paragraphs
        .iter()
        .map(|p| format!("<w:p><w:r><w:t>{p}</w:t></w:r></w:p>"))
        .collect();
    zip.start_file("word/document.xml", opts).unwrap();
    write!(
        zip,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
<w:body>{body}</w:body></w:document>"#
    )
    .unwrap();
    zip.finish().unwrap();
}

fn run(args: &[&std::ffi::OsStr]) -> Output {
    Command::new(binary()).args(args).output().unwrap()
}

fn corpus(dir: &Path) {
    // Enough text that one document yields several chunks.
    let long: Vec<String> = (0..60)
        .map(|i| format!("Sentence number {i} of the long document."))
        .collect();
    let long_refs: Vec<&str> = long.iter().map(String::as_str).collect();
    write_docx(dir, "b.docx", &long_refs);
    write_docx(dir, "a.docx", &["Alpha text."]);
    write_docx(dir, "c.docx", &["Charlie one.", "Charlie two."]);
}

#[test]
fn export_reproduces_the_streaming_output_byte_for_byte() {
    let dir = tempfile::tempdir().unwrap();
    corpus(dir.path());

    // --jobs 1 is deliberate and must stay: parallel streaming emits files in
    // completion order (documented), so only --jobs 1 has a defined order to
    // compare against. Do not raise it to "improve" this test.
    let streamed = run(&["--jobs".as_ref(), "1".as_ref(), dir.path().as_os_str()]);
    assert!(streamed.status.success());
    assert!(!streamed.stdout.is_empty());

    assert!(run(&["index".as_ref(), dir.path().as_os_str()])
        .status
        .success());
    let exported = run(&["export".as_ref(), dir.path().as_os_str()]);
    assert!(
        exported.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&exported.stderr)
    );

    assert_eq!(
        String::from_utf8(streamed.stdout).unwrap(),
        String::from_utf8(exported.stdout).unwrap()
    );
}

#[test]
fn export_emits_one_json_object_per_line() {
    let dir = tempfile::tempdir().unwrap();
    corpus(dir.path());
    run(&["index".as_ref(), dir.path().as_os_str()]);
    let out = run(&["export".as_ref(), dir.path().as_os_str()]);
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.ends_with('\n'));
    let n = text.lines().count();
    assert!(n >= 3);
    for line in text.lines() {
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        assert!(v.is_object());
    }
}

#[test]
fn export_of_an_empty_index_writes_nothing_and_exits_zero() {
    let dir = tempfile::tempdir().unwrap();
    assert!(run(&["index".as_ref(), dir.path().as_os_str()])
        .status
        .success());
    let out = run(&["export".as_ref(), dir.path().as_os_str()]);
    assert_eq!(out.status.code(), Some(0));
    assert!(out.stdout.is_empty());
}

#[test]
fn export_to_a_file_leaves_stdout_empty() {
    let dir = tempfile::tempdir().unwrap();
    corpus(dir.path());
    run(&["index".as_ref(), dir.path().as_os_str()]);
    let target = tempfile::tempdir().unwrap().keep().join("out.jsonl");
    let out = run(&[
        "export".as_ref(),
        dir.path().as_os_str(),
        "-o".as_ref(),
        target.as_os_str(),
    ]);
    assert!(out.status.success());
    assert!(out.stdout.is_empty());
    let written = std::fs::read_to_string(&target).unwrap();
    assert!(written.lines().count() >= 3);
}

#[test]
fn export_matches_streaming_order_for_nested_directories() {
    let dir = tempfile::tempdir().unwrap();
    // The walk descends into directory `a` before file `a.docx` ("a" < "a.docx"),
    // but a flat string sort puts "a.docx" before "a/x.docx" ('.' < '/').
    std::fs::create_dir(dir.path().join("a")).unwrap();
    std::fs::create_dir(dir.path().join("a").join("deep")).unwrap();
    write_docx(dir.path(), "a.docx", &["Root a."]);
    write_docx(&dir.path().join("a"), "x.docx", &["Nested x."]);
    write_docx(&dir.path().join("a").join("deep"), "y.docx", &["Deep y."]);
    write_docx(dir.path(), "b.docx", &["Root b."]);

    // --jobs 1: see the note in the byte-for-byte test.
    let streamed = run(&["--jobs".as_ref(), "1".as_ref(), dir.path().as_os_str()]);
    assert!(streamed.status.success());
    assert!(run(&["index".as_ref(), dir.path().as_os_str()])
        .status
        .success());
    let exported = run(&["export".as_ref(), dir.path().as_os_str()]);
    assert!(exported.status.success());

    assert_eq!(
        String::from_utf8(streamed.stdout).unwrap(),
        String::from_utf8(exported.stdout).unwrap()
    );
}
