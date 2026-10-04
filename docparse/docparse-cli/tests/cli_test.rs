use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

fn binary() -> PathBuf {
    // Cargo builds integration-test dependencies next to the test binary.
    let mut path = std::env::current_exe().unwrap();
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    path.join(format!("docparse{}", std::env::consts::EXE_SUFFIX))
}

fn write_docx(dir: &Path, name: &str, paragraphs: &[&str]) -> PathBuf {
    let path = dir.join(name);
    let file = std::fs::File::create(&path).unwrap();
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
    path
}

#[test]
fn emits_one_json_object_per_line_to_stdout() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_docx(dir.path(), "a.docx", &["Hello there.", "Second paragraph."]);

    let out = Command::new(binary()).arg(&path).output().unwrap();

    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.status.code(), Some(0));

    let stdout = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(lines.len(), 1, "both paragraphs fit one chunk: {stdout}");

    let v: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(v["page_start"], 1);
    assert_eq!(v["source"], "a.docx");
    assert!(v["text"].as_str().unwrap().contains("Hello there."));
    assert!(v["text"].as_str().unwrap().contains("Second paragraph."));
}

#[test]
fn a_corrupt_file_does_not_stop_the_batch_and_exits_2() {
    let dir = tempfile::tempdir().unwrap();
    let good = write_docx(dir.path(), "good.docx", &["Valid content here."]);
    let bad = dir.path().join("bad.docx");
    std::fs::write(&bad, b"this is not a zip archive at all").unwrap();

    let out = Command::new(binary())
        .arg(&bad)
        .arg(&good)
        .output()
        .unwrap();

    // Partial failure, not total.
    assert_eq!(out.status.code(), Some(2));

    let stdout = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(lines.len(), 1, "the good file must still be processed");
    assert!(lines[0].contains("Valid content here."));

    // Diagnostics never pollute the JSONL stream.
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("bad.docx"), "stderr: {stderr}");
    assert!(
        !stdout.contains("bad.docx"),
        "errors must not appear on stdout"
    );
}

#[test]
fn all_files_failing_exits_1() {
    let dir = tempfile::tempdir().unwrap();
    let bad = dir.path().join("bad.docx");
    std::fs::write(&bad, b"garbage").unwrap();

    let out = Command::new(binary()).arg(&bad).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
}

#[test]
fn walks_a_directory_and_skips_unsupported_extensions() {
    let dir = tempfile::tempdir().unwrap();
    write_docx(dir.path(), "one.docx", &["First document here."]);
    write_docx(dir.path(), "two.docx", &["Second document here."]);
    std::fs::write(dir.path().join("notes.txt"), b"ignored").unwrap();
    std::fs::create_dir(dir.path().join("nested")).unwrap();
    write_docx(
        &dir.path().join("nested"),
        "three.docx",
        &["Nested document here."],
    );

    let out = Command::new(binary()).arg(dir.path()).output().unwrap();

    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(lines.len(), 3, "expected all three docx files: {stdout}");
}

#[test]
fn output_flag_writes_to_a_file_and_leaves_stdout_empty() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_docx(dir.path(), "a.docx", &["Content for the file."]);
    let dest = dir.path().join("out.jsonl");

    let out = Command::new(binary())
        .arg(&path)
        .arg("--output")
        .arg(&dest)
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(0));
    assert!(
        out.stdout.is_empty(),
        "stdout should be empty with --output"
    );

    let written = std::fs::read_to_string(&dest).unwrap();
    assert!(written.contains("Content for the file."));
}

#[test]
fn emit_blocks_outputs_blocks_instead_of_chunks() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_docx(dir.path(), "a.docx", &["First one.", "Second one."]);

    let out = Command::new(binary())
        .arg(&path)
        .arg("--emit")
        .arg("blocks")
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(lines.len(), 2, "one line per block: {stdout}");

    let v: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(v["kind"], "paragraph");
    assert_eq!(v["page"], 1);
    assert_eq!(v["id"], 0);
}

#[test]
fn repeated_runs_produce_byte_identical_output() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_docx(
        dir.path(),
        "a.docx",
        &["Determinism matters here.", "So does this sentence."],
    );

    let first = Command::new(binary()).arg(&path).output().unwrap().stdout;
    let second = Command::new(binary()).arg(&path).output().unwrap().stdout;
    // Non-empty first: without this the test passes when BOTH runs emit
    // nothing, which makes it blind to a binary that produces no output at all.
    assert!(!first.is_empty(), "no output to compare");
    assert_eq!(first, second);
}

#[test]
fn no_inputs_is_an_error_not_a_silent_success() {
    let dir = tempfile::tempdir().unwrap();
    let out = Command::new(binary()).arg(dir.path()).output().unwrap();

    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("no supported"), "stderr: {stderr}");
}

#[test]
fn jobs_flag_processes_every_file_exactly_once() {
    let dir = tempfile::tempdir().unwrap();
    for i in 0..12 {
        write_docx(
            dir.path(),
            &format!("doc{i:02}.docx"),
            &[&format!("Document number {i} content.")],
        );
    }

    let out = Command::new(binary())
        .arg(dir.path())
        .arg("--jobs")
        .arg("4")
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(lines.len(), 12, "every file exactly once: {stdout}");

    // Every line is still valid JSON: no interleaved partial writes.
    for line in &lines {
        serde_json::from_str::<serde_json::Value>(line)
            .unwrap_or_else(|e| panic!("corrupt line {line:?}: {e}"));
    }

    for i in 0..12 {
        assert!(
            stdout.contains(&format!("Document number {i} content.")),
            "missing document {i}"
        );
    }
}

#[test]
fn a_files_lines_stay_contiguous_under_parallelism() {
    let dir = tempfile::tempdir().unwrap();
    // Each file needs several chunks so interleaving would be detectable.
    for i in 0..6 {
        let body: Vec<String> = (0..30)
            .map(|j| format!("File {i} sentence {j} of filler prose here."))
            .collect();
        let refs: Vec<&str> = body.iter().map(|s| s.as_str()).collect();
        write_docx(dir.path(), &format!("f{i}.docx"), &refs);
    }

    let out = Command::new(binary())
        .arg(dir.path())
        .arg("--jobs")
        .arg("4")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));

    let stdout = String::from_utf8(out.stdout).unwrap();
    let sources: Vec<String> = stdout
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| {
            serde_json::from_str::<serde_json::Value>(l).unwrap()["source"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();

    // Each source name appears in exactly one unbroken run.
    let mut runs: Vec<&String> = Vec::new();
    for s in &sources {
        if runs.last() != Some(&s) {
            runs.push(s);
        }
    }
    let mut unique = runs.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(
        runs.len(),
        unique.len(),
        "a file's lines were split apart: {runs:?}"
    );
}

#[test]
fn single_job_output_is_byte_identical_across_runs() {
    let dir = tempfile::tempdir().unwrap();
    for i in 0..5 {
        write_docx(
            dir.path(),
            &format!("d{i}.docx"),
            &[&format!("Deterministic content {i}.")],
        );
    }

    let run_once = || {
        Command::new(binary())
            .arg(dir.path())
            .arg("--jobs")
            .arg("1")
            .output()
            .unwrap()
            .stdout
    };

    let first = run_once();
    assert!(!first.is_empty(), "expected output, got none");
    assert_eq!(first, run_once());
}

#[test]
fn blocks_mode_carries_the_page_origin() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_docx(dir.path(), "a.docx", &["Hello there."]);
    let out = Command::new(binary())
        .args(["--emit", "blocks"])
        .arg(&path)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("\"origin\":\"synthesized\""),
        "got {stdout}"
    );
}

#[test]
fn blocks_mode_marks_ocr_text_as_ocr_with_confidence() {
    let image =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../docparse/tests/fixtures/hello_ocr.png");
    let out = Command::new(binary())
        .args(["--emit", "blocks"])
        .arg(&image)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    if out.status.success() {
        // Built with OCR: the guess must be labelled as one.
        assert!(stdout.contains("\"origin\":\"ocr\""), "got {stdout}");
        assert!(stdout.contains("\"confidence\":"), "got {stdout}");
    } else {
        // Built without OCR: it must refuse, not emit empty success.
        assert!(stdout.is_empty());
        assert!(String::from_utf8_lossy(&out.stderr).contains("OCR"));
    }
}
