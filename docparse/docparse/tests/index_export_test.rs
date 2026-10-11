use docparse::index::Index;
use docparse::model::{BlockRef, Chunk, PageOrigin};

fn chunk(ordinal: u64, page: u32, origin: PageOrigin) -> Chunk {
    Chunk {
        id: ordinal,
        source: "scan.png".to_string(),
        page_start: page,
        page_end: page,
        blocks: vec![BlockRef { page, block: 0 }],
        text: format!("chunk {ordinal}"),
        origin,
        hard_split: false,
    }
}

#[test]
fn exported_ocr_chunks_carry_their_confidence() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut index = Index::open(dir.path(), None).unwrap();
        let chunks = vec![
            chunk(0, 1, PageOrigin::Ocr { confidence: 0.75 }),
            chunk(1, 2, PageOrigin::Native),
        ];
        index
            .store_parsed("h", 10, Some(2), &chunks, "sub/scan.png", 1)
            .unwrap();
    }

    let index = Index::open_existing(dir.path(), None).unwrap();
    let mut out = Vec::new();
    let n = index.export(&mut out).unwrap();
    assert_eq!(n, 2);

    let text = String::from_utf8(out).unwrap();
    let lines: Vec<serde_json::Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines[0]["origin"], "ocr");
    assert_eq!(lines[0]["confidence"], 0.75);
    assert_eq!(lines[1]["origin"], "native");
    assert!(lines[1].get("confidence").is_none());
    // `source` is the file NAME, not the relative path.
    assert_eq!(lines[0]["source"], "scan.png");
}
