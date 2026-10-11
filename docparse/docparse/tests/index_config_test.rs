use docparse::chunk::ChunkConfig;
use docparse::index::{Index, IndexConfig, Staleness};

fn cfg() -> IndexConfig {
    IndexConfig {
        chunk: ChunkConfig::default(),
        ocr: false,
        ocr_dpi: 150,
    }
}

#[test]
fn the_same_settings_fingerprint_identically() {
    assert_eq!(cfg().fingerprint(), cfg().fingerprint());
}

#[test]
fn every_output_affecting_setting_changes_the_fingerprint() {
    let base = cfg().fingerprint();

    let mut a = cfg();
    a.chunk.target_chars = 500;
    assert_ne!(a.fingerprint(), base, "target_chars must count");

    let mut b = cfg();
    b.chunk.max_chars = 2000;
    assert_ne!(b.fingerprint(), base, "max_chars must count");

    let mut c = cfg();
    c.chunk.overlap_sentences = 3;
    assert_ne!(c.fingerprint(), base, "overlap_sentences must count");

    let mut d = cfg();
    d.chunk.cross_page = true;
    assert_ne!(d.fingerprint(), base, "cross_page must count");

    let mut e = cfg();
    e.ocr = true;
    assert_ne!(e.fingerprint(), base, "ocr must count");

    let mut f = cfg();
    f.ocr_dpi = 300;
    assert_ne!(f.fingerprint(), base, "ocr_dpi must count");
}

#[test]
fn a_brand_new_index_reports_first_run() {
    let dir = tempfile::tempdir().unwrap();
    let index = Index::open(dir.path(), None).unwrap();
    assert_eq!(index.staleness(&cfg()).unwrap(), Staleness::FirstRun);
}

#[test]
fn unchanged_settings_report_current() {
    let dir = tempfile::tempdir().unwrap();
    let mut index = Index::open(dir.path(), None).unwrap();
    index.record_config(&cfg()).unwrap();
    assert_eq!(index.staleness(&cfg()).unwrap(), Staleness::Current);
}

#[test]
fn a_changed_chunk_size_reports_what_changed() {
    let dir = tempfile::tempdir().unwrap();
    let mut index = Index::open(dir.path(), None).unwrap();
    index.record_config(&cfg()).unwrap();

    let mut changed = cfg();
    changed.chunk.target_chars = 400;

    match index.staleness(&changed).unwrap() {
        Staleness::ConfigChanged { from, to } => {
            // The message must name the setting, not just report a hash diff.
            assert!(from.contains("800"), "old value should appear: {from}");
            assert!(to.contains("400"), "new value should appear: {to}");
        }
        other => panic!("expected ConfigChanged, got {other:?}"),
    }
}

#[test]
fn a_bumped_extraction_version_invalidates_everything() {
    let dir = tempfile::tempdir().unwrap();
    let mut index = Index::open(dir.path(), None).unwrap();
    index.record_config(&cfg()).unwrap();

    // Simulate the real case: a code change altered output for unchanged
    // input, exactly as the segment-merging fix did.
    index.set_meta("extraction_version", "0").unwrap();

    match index.staleness(&cfg()).unwrap() {
        Staleness::ExtractionChanged { from, to } => {
            assert_eq!(from, 0);
            assert_eq!(to, docparse::index::EXTRACTION_VERSION);
        }
        other => panic!("expected ExtractionChanged, got {other:?}"),
    }
}

#[test]
fn a_fingerprint_without_a_version_is_never_current() {
    let dir = tempfile::tempdir().unwrap();
    let index = Index::open(dir.path(), None).unwrap();
    // The state an interrupted record_config would have left.
    index
        .set_meta("config_fingerprint", &cfg().fingerprint())
        .unwrap();
    assert_ne!(index.staleness(&cfg()).unwrap(), Staleness::Current);
}

#[test]
fn an_unparseable_version_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let index = Index::open(dir.path(), None).unwrap();
    index
        .set_meta("config_fingerprint", &cfg().fingerprint())
        .unwrap();
    index.set_meta("extraction_version", "abc").unwrap();
    assert!(index.staleness(&cfg()).is_err());
}

#[test]
fn extraction_change_takes_precedence_over_config_change() {
    let dir = tempfile::tempdir().unwrap();
    let mut index = Index::open(dir.path(), None).unwrap();
    index.record_config(&cfg()).unwrap();
    index.set_meta("extraction_version", "0").unwrap();

    let mut changed = cfg();
    changed.chunk.target_chars = 400;

    assert!(matches!(
        index.staleness(&changed).unwrap(),
        Staleness::ExtractionChanged { .. }
    ));
}
