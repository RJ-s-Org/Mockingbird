//! Text extraction, kind detection and chunking of the smart importer.
#![allow(clippy::unwrap_used)]

use mb_import::smart::chunk::{Chunk, chunk, estimate_tokens};
use mb_import::smart::text::{
    Block, BlockKind, TextDoc, extract_html, extract_markdown, extract_pdf, extract_text,
    is_endpoint_line,
};
use mb_import::smart::{ImportKind, SmartError, detect_kind};
use pdf_extract::content::{Content, Operation};
use pdf_extract::{Document, Object, ObjectId, Stream, dictionary};
use proptest::prelude::*;

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!(
        "{}/../../fixtures/smart/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

fn fixture_text(name: &str) -> String {
    String::from_utf8(fixture(name)).unwrap()
}

fn path(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

#[test]
fn markdown_headings_and_code_blocks() {
    let doc = extract_markdown(&fixture_text("petstore.md"));
    assert_eq!(doc.title.as_deref(), Some("Petstore API"));
    let list_pets = path(&["Petstore API", "Pets", "List pets"]);
    let code = doc
        .blocks
        .iter()
        .find(|b| b.kind == BlockKind::Code && b.heading_path == list_pets)
        .expect("a code block under List pets");
    assert!(code.text.contains("\"id\": 1"), "{}", code.text);
    // Prose of the section keeps the endpoint line and the parameter.
    assert!(doc.blocks.iter().any(|b| b.kind == BlockKind::Prose
        && b.heading_path == list_pets
        && b.text.starts_with("GET /pets")));
    assert!(
        doc.blocks
            .iter()
            .any(|b| b.heading_path == list_pets && b.text.contains("limit"))
    );
    // A later H3 replaces the previous H3 in the path.
    assert!(
        doc.blocks
            .iter()
            .any(|b| b.heading_path == path(&["Petstore API", "Pets", "Create a pet"]))
    );
    // An H2 replaces the H2 and drops the H3.
    assert!(
        doc.blocks
            .iter()
            .any(|b| b.heading_path == path(&["Petstore API", "Pet fields"]))
    );
}

#[test]
fn markdown_tables_flattened() {
    let doc = extract_markdown(&fixture_text("petstore.md"));
    let table = doc
        .blocks
        .iter()
        .find(|b| b.heading_path == path(&["Petstore API", "Pet fields"]))
        .unwrap();
    assert!(
        table.text.contains("Field | Type | Description"),
        "{}",
        table.text
    );
    assert!(
        table.text.contains("id | integer | The pet id"),
        "{}",
        table.text
    );
}

#[test]
fn html_skips_script_style_nav() {
    let doc = extract_html(&fixture_text("petstore.html"));
    assert_eq!(doc.title.as_deref(), Some("Petstore API Reference"));
    for b in &doc.blocks {
        assert!(!b.text.contains("tracking()"), "{b:?}");
        assert!(!b.text.contains("Home | About"), "{b:?}");
        assert!(!b.text.contains("font-family"), "{b:?}");
        assert!(!b.text.contains("Copyright"), "{b:?}");
        assert!(!b.text.contains("Enable JavaScript"), "{b:?}");
    }
    let list_pets = path(&["Petstore API", "Pets", "List pets"]);
    let code = doc
        .blocks
        .iter()
        .find(|b| b.kind == BlockKind::Code && b.heading_path == list_pets)
        .expect("<pre> becomes a Code block");
    assert!(code.text.contains("\"id\": 1"));
    assert!(
        code.text.contains("\n    \"name\": \"Rex\","),
        "verbatim: {}",
        code.text
    );
    assert!(doc.blocks.iter().any(|b| b.kind == BlockKind::Prose
        && b.heading_path == list_pets
        && b.text.starts_with("GET /pets")));
    let table = doc
        .blocks
        .iter()
        .find(|b| b.heading_path == path(&["Petstore API", "Pet fields"]))
        .unwrap();
    assert!(
        table.text.contains("id | integer | The pet id"),
        "{}",
        table.text
    );
}

#[test]
fn html_title_falls_back_to_first_h1() {
    let doc =
        extract_html("<html><body><h1>Orders  API</h1><p>Hello</p><h1>Second</h1></body></html>");
    assert_eq!(doc.title.as_deref(), Some("Orders API"));
}

#[test]
fn endpoint_lines_start_blocks() {
    let doc = extract_markdown("intro\nGET /a returns a\nPOST /b creates b");
    let texts: Vec<&str> = doc.blocks.iter().map(|b| b.text.as_str()).collect();
    assert_eq!(texts, ["intro", "GET /a returns a", "POST /b creates b"]);
    assert!(doc.blocks.iter().all(|b| b.kind == BlockKind::Prose));
}

#[test]
fn endpoint_line_detection() {
    assert!(is_endpoint_line("GET /pets"));
    assert!(is_endpoint_line("  delete /pets/{id} removes"));
    assert!(is_endpoint_line("Post https://api.example.com/x"));
    assert!(!is_endpoint_line("GET pets"));
    assert!(!is_endpoint_line("GETTING /pets"));
    assert!(!is_endpoint_line("GET"));
    assert!(!is_endpoint_line(""));
}

/// A PDF with one Helvetica text line per page, after lopdf's
/// document-creation example.
fn build_pdf(lines: &[&str]) -> Vec<u8> {
    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();
    let font_id = doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => "Helvetica",
    });
    let resources_id = doc.add_object(dictionary! {
        "Font" => dictionary! { "F1" => font_id },
    });
    let mut kids: Vec<Object> = Vec::new();
    for line in lines {
        let content = Content {
            operations: vec![
                Operation::new("BT", vec![]),
                Operation::new("Tf", vec!["F1".into(), 24.into()]),
                Operation::new("Td", vec![100.into(), 600.into()]),
                Operation::new("Tj", vec![Object::string_literal(*line)]),
                Operation::new("ET", vec![]),
            ],
        };
        let content_id = doc.add_object(Stream::new(dictionary! {}, content.encode().unwrap()));
        let page_id: ObjectId = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
        });
        kids.push(page_id.into());
    }
    finish_pdf(doc, pages_id, resources_id, kids)
}

fn finish_pdf(
    mut doc: Document,
    pages_id: ObjectId,
    resources_id: ObjectId,
    kids: Vec<Object>,
) -> Vec<u8> {
    let count = kids.len() as i64;
    let pages = dictionary! {
        "Type" => "Pages",
        "Kids" => kids,
        "Count" => count,
        "Resources" => resources_id,
        "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
    };
    doc.objects.insert(pages_id, Object::Dictionary(pages));
    let catalog_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
    });
    doc.trailer.set("Root", catalog_id);
    let mut out = Vec::new();
    doc.save_to(&mut out).unwrap();
    out
}

fn empty_pages_pdf(n: usize) -> Vec<u8> {
    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();
    let resources_id = doc.add_object(dictionary! {});
    let kids = (0..n)
        .map(|_| {
            doc.add_object(dictionary! {
                "Type" => "Page",
                "Parent" => pages_id,
            })
            .into()
        })
        .collect();
    finish_pdf(doc, pages_id, resources_id, kids)
}

#[test]
fn pdf_text_by_page() {
    let doc = extract_pdf(&build_pdf(&["GET /pets", "POST /pets"])).unwrap();
    let page1 = doc
        .blocks
        .iter()
        .find(|b| b.heading_path == path(&["Page 1"]))
        .expect("page 1");
    assert!(page1.text.contains("GET /pets"), "{page1:?}");
    let page2 = doc
        .blocks
        .iter()
        .find(|b| b.heading_path == path(&["Page 2"]))
        .expect("page 2");
    assert!(page2.text.contains("POST /pets"), "{page2:?}");
    assert!(doc.blocks.iter().all(|b| b.kind == BlockKind::Prose));
}

#[test]
fn pdf_page_cap() {
    match extract_pdf(&empty_pages_pdf(501)) {
        Err(SmartError::Unreadable(m)) => assert!(m.contains("500"), "{m}"),
        other => panic!("expected Unreadable, got {other:?}"),
    }
}

#[test]
fn truncated_pdf_is_an_error() {
    let pdf = build_pdf(&["GET /pets", "POST /pets"]);
    assert!(extract_pdf(&pdf[..100]).is_err());
    assert!(extract_text(ImportKind::Pdf, &pdf[..100]).is_err());
}

#[test]
fn non_utf8_markdown_is_unreadable() {
    let bytes = [b'#', b' ', 0xff, 0xfe, b'x'];
    assert!(matches!(
        extract_text(ImportKind::Markdown, &bytes),
        Err(SmartError::Unreadable(_))
    ));
    assert!(matches!(
        extract_text(ImportKind::Text, &bytes),
        Err(SmartError::Unreadable(_))
    ));
}

#[test]
fn extract_text_dispatches_by_kind() {
    let md = extract_text(ImportKind::Text, b"# T\n\nGET /a").unwrap();
    assert_eq!(md.title.as_deref(), Some("T"));
    let html = extract_text(ImportKind::Html, b"<title>X</title><p>hi</p>").unwrap();
    assert_eq!(html.title.as_deref(), Some("X"));
    assert!(matches!(
        extract_text(ImportKind::Har, b"{}"),
        Err(SmartError::Unsupported(_))
    ));
    assert!(matches!(
        extract_text(ImportKind::Postman, b"{}"),
        Err(SmartError::Unsupported(_))
    ));
}

#[test]
fn detect_kinds() {
    let har = br#"{"log":{"version":"1.2","creator":{"name":"x","version":"1"},"entries":[]}}"#;
    let postman = br#"{"info":{"_postman_id":"1","name":"Pets","schema":"https://schema.getpostman.com/json/collection/v2.1.0/collection.json"},"item":[]}"#;
    let postman_no_schema = br#"{"info":{"_postman_id":"1","name":"Pets"},"item":[]}"#;
    let openapi = br#"{"openapi":"3.1.0","info":{"title":"x","version":"1"},"paths":{}}"#;
    let cases: &[(Option<&str>, &[u8], Option<ImportKind>)] = &[
        (Some("a.pdf"), b"%PDF-1.7\n", Some(ImportKind::Pdf)),
        (None, b"%PDF-1.4\n", Some(ImportKind::Pdf)),
        (Some("rec.har"), har, Some(ImportKind::Har)),
        (None, har, Some(ImportKind::Har)),
        (Some("c.json"), postman, Some(ImportKind::Postman)),
        (None, postman_no_schema, Some(ImportKind::Postman)),
        (Some("a.md"), b"# Title", Some(ImportKind::Markdown)),
        (Some("A.MARKDOWN"), b"# Title", Some(ImportKind::Markdown)),
        (
            Some("x"),
            b"<!DOCTYPE html><html></html>",
            Some(ImportKind::Html),
        ),
        (
            None,
            b"  \n<HTML><body></body></HTML>",
            Some(ImportKind::Html),
        ),
        (Some("page.htm"), b"hello", Some(ImportKind::Html)),
        (Some("a.txt"), b"plain", Some(ImportKind::Text)),
        (Some("api.json"), openapi, None),
        (Some("a.bin"), &[0, 1, 2, 3], None),
        (None, b"just text", None),
    ];
    for (name, bytes, want) in cases {
        assert_eq!(detect_kind(*name, bytes), *want, "{name:?}");
    }
}

#[test]
fn import_kind_wire_strings() {
    for (kind, s, llm) in [
        (ImportKind::Markdown, "markdown", true),
        (ImportKind::Html, "html", true),
        (ImportKind::Pdf, "pdf", true),
        (ImportKind::Text, "text", true),
        (ImportKind::Har, "har", false),
        (ImportKind::Postman, "postman", false),
    ] {
        assert_eq!(kind.as_str(), s);
        assert_eq!(ImportKind::parse(s), Some(kind));
        assert_eq!(kind.needs_llm(), llm);
        assert_eq!(serde_json::to_value(kind).unwrap(), serde_json::json!(s));
    }
    assert_eq!(ImportKind::parse("url"), None);
}

// ---- chunking ----

fn prose(heading: &[&str], text: &str) -> Block {
    Block {
        heading_path: path(heading),
        kind: BlockKind::Prose,
        text: text.to_string(),
    }
}

fn code(heading: &[&str], text: &str) -> Block {
    Block {
        heading_path: path(heading),
        kind: BlockKind::Code,
        text: text.to_string(),
    }
}

fn doc(blocks: Vec<Block>) -> TextDoc {
    TextDoc {
        title: None,
        blocks,
    }
}

/// `n` numbered lines of 40 chars (10 tokens) each.
fn lines(tag: &str, n: usize) -> String {
    (0..n)
        .map(|i| format!("{tag} line {i:04} {}", "x".repeat(40 - tag.len() - 11)))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn estimate_tokens_is_ceil_chars_over_four() {
    assert_eq!(estimate_tokens(""), 0);
    assert_eq!(estimate_tokens("abc"), 1);
    assert_eq!(estimate_tokens("abcd"), 1);
    assert_eq!(estimate_tokens("abcde"), 2);
    assert_eq!(estimate_tokens("ééééé"), 2);
}

#[test]
fn chunk_small_doc_single_chunk() {
    let d = extract_markdown(&fixture_text("petstore.md"));
    let chunks = chunk(&d, 6000, 400);
    assert_eq!(chunks.len(), 1);
    let c = &chunks[0];
    assert_eq!(c.index, 0);
    assert_eq!(c.heading, "Petstore API");
    assert!(c.text.starts_with("# Petstore API\n"), "{}", c.text);
    assert!(c.text.contains("# Petstore API > Pets > List pets\n"));
    assert!(c.text.contains("\"id\": 1"));
    assert!(!c.text.contains("[…continued]"));
    assert_eq!(c.tokens, estimate_tokens(&c.text));
}

#[test]
fn chunk_heading_line_only_when_path_changes() {
    let d = doc(vec![
        prose(&["A"], "one"),
        prose(&["A"], "two"),
        prose(&["A", "B"], "three"),
    ]);
    let chunks = chunk(&d, 6000, 400);
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].text.matches("# A\n").count(), 1);
    assert_eq!(chunks[0].text.matches("# A > B\n").count(), 1);
}

#[test]
fn chunk_respects_max_tokens() {
    let d = doc(vec![
        prose(&["A"], &lines("a", 30)),
        prose(&["A", "B"], &lines("b", 30)),
        code(&["A", "B"], &lines("c", 12)),
        prose(&["C"], &lines("d", 70)),
    ]);
    let (max, overlap) = (100, 20);
    let chunks = chunk(&d, max, overlap);
    assert!(chunks.len() > 3);
    for (i, c) in chunks.iter().enumerate() {
        assert_eq!(c.index, i);
        assert_eq!(c.tokens, estimate_tokens(&c.text));
        assert!(
            c.tokens <= max + overlap + 8,
            "chunk {i}: {} tokens",
            c.tokens
        );
    }
}

#[test]
fn chunk_overlap_repeats_tail() {
    let d = doc(vec![prose(&["A"], &lines("a", 60))]);
    let chunks = chunk(&d, 200, 40);
    assert!(chunks.len() >= 2);
    for pair in chunks.windows(2) {
        let last_line = pair[0].text.lines().last().unwrap();
        assert!(
            pair[1].text.starts_with("[…continued]\n"),
            "{}",
            pair[1].text
        );
        assert!(pair[1].text.contains(last_line), "{last_line}");
        // The overlap holds at least `overlap` tokens of whole lines.
        let body_start = pair[1].text.find("# A\n").unwrap();
        let overlap = &pair[1].text["[…continued]\n".len()..body_start];
        assert!(estimate_tokens(overlap) >= 40, "{overlap:?}");
        assert!(pair[0].text.contains(overlap.trim_end()));
    }
    // Every source line appears in some chunk.
    for line in lines("a", 60).lines() {
        assert!(chunks.iter().any(|c| c.text.contains(line)), "{line}");
    }
}

#[test]
fn chunk_keeps_code_block_whole() {
    let code_text = lines("c", 15); // ~150 tokens
    let d = doc(vec![
        prose(&["A"], &lines("a", 12)), // ~120 tokens
        code(&["A"], &code_text),
        prose(&["A"], &lines("b", 5)),
    ]);
    let chunks = chunk(&d, 200, 20);
    let holders: Vec<&Chunk> = chunks
        .iter()
        .filter(|c| c.text.contains("c line 0000"))
        .collect();
    assert_eq!(holders.len(), 1, "{chunks:#?}");
    assert!(holders[0].text.contains(&code_text));
}

#[test]
fn chunk_splits_oversized_block() {
    let big = lines("a", 100); // ~1000 tokens
    let d = doc(vec![code(&["A"], &big)]);
    let chunks = chunk(&d, 200, 20);
    assert!(chunks.len() >= 5);
    for c in &chunks {
        assert!(c.tokens <= 200 + 20 + 8, "{}", c.tokens);
    }
    for line in big.lines() {
        assert!(chunks.iter().any(|c| c.text.contains(line)), "{line}");
    }
    // A single over-long line is split at char boundaries.
    let long_line = "é".repeat(3000);
    let chunks = chunk(&doc(vec![prose(&["A"], &long_line)]), 200, 20);
    assert!(chunks.len() >= 3);
    let total: usize = chunks
        .iter()
        .map(|c| c.text.chars().filter(|&ch| ch == 'é').count())
        .sum();
    assert!(total >= 3000);
    for c in &chunks {
        assert!(c.tokens <= 200 + 20 + 8, "{}", c.tokens);
    }
}

#[test]
fn chunk_prefers_heading_boundary() {
    // Section A fills more than half a chunk; section B would still fit,
    // but it starts a new level-2 section, so it opens a new chunk.
    let d = doc(vec![
        prose(&["Doc", "A"], &lines("a", 12)),
        prose(&["Doc", "B"], &lines("b", 3)),
    ]);
    let chunks = chunk(&d, 200, 20);
    assert_eq!(chunks.len(), 2, "{chunks:#?}");
    assert!(!chunks[0].text.contains("b line"));
    assert_eq!(chunks[1].heading, "Doc > B");
    // A level-3 change is not a section boundary.
    let d = doc(vec![
        prose(&["Doc", "A", "x"], &lines("a", 12)),
        prose(&["Doc", "A", "y"], &lines("b", 3)),
    ]);
    assert_eq!(chunk(&d, 200, 20).len(), 1);
    // Below half full a new section joins the current chunk.
    let d = doc(vec![
        prose(&["Doc", "A"], &lines("a", 5)),
        prose(&["Doc", "B"], &lines("b", 3)),
    ]);
    assert_eq!(chunk(&d, 200, 20).len(), 1);
}

#[test]
fn chunk_empty_doc_has_no_chunks() {
    assert!(chunk(&TextDoc::default(), 6000, 400).is_empty());
}

proptest! {
    #[test]
    fn chunk_bounds_hold(
        sizes in proptest::collection::vec((0usize..4, 1usize..60, 1usize..120), 1..12),
        max in 40usize..400,
    ) {
        let overlap = max / 10;
        let blocks: Vec<Block> = sizes
            .iter()
            .enumerate()
            .map(|(i, (h, n, w))| {
                let text = (0..*n)
                    .map(|j| format!("{i}-{j} {}", "w".repeat(*w)))
                    .collect::<Vec<_>>()
                    .join("\n");
                let heading = &["S0", "S1", "S2", "S3"][..=*h];
                if i % 3 == 0 { code(heading, &text) } else { prose(heading, &text) }
            })
            .collect();
        let chunks = chunk(&doc(blocks), max, overlap);
        prop_assert!(!chunks.is_empty());
        for c in &chunks {
            prop_assert_eq!(c.tokens, estimate_tokens(&c.text));
            prop_assert!(c.tokens <= max + overlap + 8, "{} > {}", c.tokens, max + overlap + 8);
        }
    }
}

/// Little-endian bit writer for hand-made deflate data.
struct Bits {
    out: Vec<u8>,
    acc: u64,
    n: u32,
}

impl Bits {
    fn push(&mut self, value: u64, bits: u32) {
        self.acc |= value << self.n;
        self.n += bits;
        while self.n >= 8 {
            self.out.push(self.acc as u8);
            self.acc >>= 8;
            self.n -= 8;
        }
    }

    /// Huffman codes are stored most significant bit first.
    fn code(&mut self, code: u64, bits: u32) {
        let reversed = (0..bits).fold(0, |r, i| (r << 1) | ((code >> i) & 1));
        self.push(reversed, bits);
    }
}

/// A zlib stream that inflates to `1 + 258 * copies` zero bytes using one
/// fixed-Huffman block: a literal 0, then (length 258, distance 1) copies.
/// About 13 bits per 258 output bytes, so it is cheap to build.
fn zlib_zeros(copies: usize) -> Vec<u8> {
    let mut bits = Bits {
        out: vec![0x78, 0x01],
        acc: 0,
        n: 0,
    };
    bits.push(1, 1); // BFINAL
    bits.push(1, 2); // BTYPE = fixed Huffman
    bits.code(0x30, 8); // literal 0
    for _ in 0..copies {
        bits.code(0b1100_0101, 8); // length code 285 (258 bytes)
        bits.code(0, 5); // distance code 0 (distance 1)
    }
    bits.code(0, 7); // end of block
    if bits.n > 0 {
        let pad = 8 - bits.n;
        bits.push(0, pad);
    }
    let len = 1 + 258 * copies as u64;
    let adler = ((len % 65521) << 16) | 1;
    bits.out.extend_from_slice(&(adler as u32).to_be_bytes());
    bits.out
}

/// A one-page PDF whose content stream is `content`, under `filter`.
fn pdf_with_stream(filter: Object, content: Vec<u8>) -> Vec<u8> {
    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();
    let resources_id = doc.add_object(dictionary! {});
    let content_id = doc.add_object(Stream::new(dictionary! { "Filter" => filter }, content));
    let page_id: ObjectId = doc.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => pages_id,
        "Contents" => content_id,
    });
    finish_pdf(doc, pages_id, resources_id, vec![page_id.into()])
}

fn assert_bomb_refused(pdf: &[u8]) {
    match extract_pdf(pdf) {
        Err(SmartError::Unreadable(m)) => assert!(m.contains("expands"), "{m}"),
        other => panic!("expected Unreadable, got {other:?}"),
    }
}

#[test]
fn zlib_zeros_inflates_to_zeros() {
    use std::io::Read;
    let mut out = Vec::new();
    flate2::read::ZlibDecoder::new(&zlib_zeros(4)[..])
        .read_to_end(&mut out)
        .unwrap();
    assert_eq!(out, vec![0; 1 + 258 * 4]);
}

#[test]
fn pdf_flate_bomb_is_refused() {
    // About 650 KB that inflates to 100 MiB, past the 64 MiB cap.
    let bomb = zlib_zeros(100 * 1024 * 1024 / 258);
    let pdf = pdf_with_stream("FlateDecode".into(), bomb);
    assert!(pdf.len() < 1024 * 1024, "{}", pdf.len());
    assert_bomb_refused(&pdf);
}

#[test]
fn pdf_nested_flate_bomb_is_refused() {
    use std::io::Write;
    // The bomb compressed once more: only the second layer explodes.
    let inner = zlib_zeros(100 * 1024 * 1024 / 258);
    let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
    enc.write_all(&inner).unwrap();
    let outer = enc.finish().unwrap();
    let filters = vec![Object::from("FlateDecode"), Object::from("FlateDecode")];
    assert_bomb_refused(&pdf_with_stream(filters.into(), outer));
}

#[test]
fn pdf_with_long_filter_chain_is_refused() {
    let filters: Vec<Object> = (0..20).map(|_| Object::from("ASCII85Decode")).collect();
    match extract_pdf(&pdf_with_stream(filters.into(), b"~>".to_vec())) {
        Err(SmartError::Unreadable(_)) => {}
        other => panic!("expected Unreadable, got {other:?}"),
    }
}

#[test]
fn pdf_with_compressed_content_is_read() {
    use std::io::Write;
    let mut doc = Document::load_mem(&build_pdf(&["GET /pets"])).unwrap();
    for object in doc.objects.values_mut() {
        if let Object::Stream(stream) = object {
            let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
            enc.write_all(&stream.content).unwrap();
            stream.dict.set("Filter", "FlateDecode");
            stream.set_content(enc.finish().unwrap());
        }
    }
    let mut pdf = Vec::new();
    doc.save_to(&mut pdf).unwrap();
    assert!(
        String::from_utf8_lossy(&pdf).contains("FlateDecode"),
        "the fixture should be compressed"
    );
    let text = extract_pdf(&pdf).unwrap();
    assert!(
        text.blocks.iter().any(|b| b.text.contains("GET /pets")),
        "{text:?}"
    );
}
