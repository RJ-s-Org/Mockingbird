#![cfg(test)]
use std::io::Write;
use std::path::PathBuf;

use mb_import::{
    ImportError, SourceFile, SourceFormat, compile, compile_files, pack_dir, read_source, sniff,
    unpack_archive,
};
use sha2::{Digest, Sha256};

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures")
}

fn hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A deflated zip of `(name, bytes)` entries, names stored verbatim.
fn zip_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default();
    for (name, bytes) in entries {
        w.start_file(*name, opts).unwrap();
        w.write_all(bytes).unwrap();
    }
    w.finish().unwrap().into_inner()
}

/// A tar.gz of regular files.
fn tar_gz_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut b = tar::Builder::new(gz);
    for (name, bytes) in entries {
        let mut h = tar::Header::new_gnu();
        h.set_size(bytes.len() as u64);
        h.set_mode(0o644);
        b.append_data(&mut h, name, *bytes).unwrap();
    }
    b.into_inner().unwrap().finish().unwrap()
}

fn assert_invalid_entry(err: ImportError, entry: &str) {
    assert!(
        matches!(&err, ImportError::Invalid { pointer, .. } if pointer == entry),
        "expected Invalid naming {entry:?}, got {err:?}"
    );
}

#[test]
fn zip_slip_rejected() {
    for name in ["../evil.proto", "/abs.proto", "a\\b.proto", "C:/x.proto"] {
        let zip = zip_of(&[(name, b"syntax = \"proto3\";")]);
        assert_invalid_entry(unpack_archive(&zip).unwrap_err(), name);
        assert_invalid_entry(compile(&zip).unwrap_err(), name);
    }
}

#[test]
fn symlink_entry_rejected() {
    let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut b = tar::Builder::new(gz);
    let mut h = tar::Header::new_gnu();
    h.set_entry_type(tar::EntryType::Symlink);
    h.set_size(0);
    h.set_mode(0o777);
    b.append_link(&mut h, "link.proto", "/etc/passwd").unwrap();
    let bytes = b.into_inner().unwrap().finish().unwrap();
    assert_invalid_entry(unpack_archive(&bytes).unwrap_err(), "link.proto");
}

#[test]
fn too_many_files_rejected() {
    let names: Vec<String> = (0..501).map(|i| format!("f{i:03}.txt")).collect();
    let entries: Vec<(&str, &[u8])> = names.iter().map(|n| (n.as_str(), &b"x"[..])).collect();
    let err = unpack_archive(&zip_of(&entries)).unwrap_err();
    assert!(matches!(err, ImportError::Invalid { .. }), "{err:?}");
    // 500 is still fine.
    assert_eq!(unpack_archive(&zip_of(&entries[..500])).unwrap().len(), 500);
}

#[test]
fn unpacked_size_limit() {
    let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    w.start_file("zeros.bin", zip::write::SimpleFileOptions::default())
        .unwrap();
    let chunk = vec![0u8; 1024 * 1024];
    for _ in 0..65 {
        w.write_all(&chunk).unwrap();
    }
    let zip = w.finish().unwrap().into_inner();
    assert!(zip.len() < 1024 * 1024, "the zip itself is small");
    let err = unpack_archive(&zip).unwrap_err();
    assert!(
        matches!(&err, ImportError::Invalid { message, .. } if message.contains("unpacks to more than")),
        "{err:?}"
    );
}

#[test]
fn multi_file_proto_zip_compiles() {
    let dir = fixtures().join("proto/multi");
    let paths = ["common/v1/money.proto", "orders/v1/orders.proto"];
    let contents: Vec<Vec<u8>> = paths
        .iter()
        .map(|p| std::fs::read(dir.join(p)).unwrap())
        .collect();
    let entries: Vec<(&str, &[u8])> = paths
        .iter()
        .zip(&contents)
        .map(|(p, c)| (*p, c.as_slice()))
        .collect();
    let zip = zip_of(&entries);
    assert_eq!(sniff(&zip).unwrap(), SourceFormat::Archive);
    let b = compile(&zip).unwrap();
    assert_eq!(b.source_digest, hex(&zip));
    assert_eq!(b.title, "orders.v1");
    let ids: Vec<&str> = b.operations.iter().map(|o| o.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "OrderService.GetOrder",
            "OrderService.CreateOrder",
            "OrderService.ListOrders",
        ]
    );
    assert!(b.defs.contains_key("common.v1.Money"));
}

#[test]
fn tar_gz_wsdl_with_xsd_compiles() {
    let dir = fixtures().join("wsdl/external");
    let wsdl = std::fs::read(dir.join("service.wsdl")).unwrap();
    let xsd = std::fs::read(dir.join("types.xsd")).unwrap();
    let archive = tar_gz_of(&[("./service.wsdl", &wsdl), ("./types.xsd", &xsd)]);
    assert_eq!(sniff(&archive).unwrap(), SourceFormat::Archive);
    let b = compile(&archive).unwrap();
    assert_eq!(b.source_digest, hex(&archive));
    assert_eq!(b.title, "Orders");
    assert!(b.operation("GetOrder").is_some());

    let loose = compile_files(&[
        SourceFile {
            path: "service.wsdl".into(),
            bytes: wsdl,
        },
        SourceFile {
            path: "types.xsd".into(),
            bytes: xsd,
        },
    ])
    .unwrap();
    assert_eq!(b.operations, loose.operations);
    assert_eq!(b.defs, loose.defs);
}

#[test]
fn pack_dir_is_deterministic() {
    let dir = fixtures().join("proto/multi");
    let a = pack_dir(&dir).unwrap();
    let b = pack_dir(&dir).unwrap();
    assert_eq!(a, b);
    assert_eq!(read_source(&dir).unwrap(), a);
    // gzip header: magic, deflate, no flags, mtime 0.
    assert_eq!(&a[..8], &[0x1f, 0x8b, 8, 0, 0, 0, 0, 0]);

    let files = unpack_archive(&a).unwrap();
    let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, ["common/v1/money.proto", "orders/v1/orders.proto"]);

    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(a.as_slice()));
    for entry in tar.entries().unwrap() {
        let h = entry.unwrap().header().clone();
        assert_eq!(h.mtime().unwrap(), 0);
        assert_eq!(h.mode().unwrap(), 0o644);
        assert_eq!(h.uid().unwrap(), 0);
        assert_eq!(h.gid().unwrap(), 0);
    }

    // Packed directories compile like the loose files.
    let compiled = compile(&a).unwrap();
    assert_eq!(compiled.source_digest, hex(&a));
    assert_eq!(compiled.operations.len(), 3);

    // A single file reads as its bytes.
    let file = fixtures().join("proto/helloworld.proto");
    assert_eq!(read_source(&file).unwrap(), std::fs::read(&file).unwrap());
}

#[test]
fn single_top_level_dir_stripped() {
    let zip = zip_of(&[
        ("contract/", b""),
        ("contract/a.graphql", b"type Query { a: Int }"),
        ("contract/sub/b.graphql", b"type B { b: Int }"),
    ]);
    let paths: Vec<String> = unpack_archive(&zip)
        .unwrap()
        .into_iter()
        .map(|f| f.path)
        .collect();
    assert_eq!(paths, ["a.graphql", "sub/b.graphql"]);

    // Two top-level directories are kept.
    let zip = zip_of(&[("x/a.graphql", b"type Query { a: Int }"), ("y/b.txt", b"")]);
    let paths: Vec<String> = unpack_archive(&zip)
        .unwrap()
        .into_iter()
        .map(|f| f.path)
        .collect();
    assert_eq!(paths, ["x/a.graphql", "y/b.txt"]);
}

#[test]
fn graphql_files_are_joined() {
    let zip = zip_of(&[
        ("schema/b.graphqls", b"type Query { user: User }"),
        ("schema/a.gql", b"type User { id: ID! }"),
    ]);
    let b = compile(&zip).unwrap();
    assert!(b.operation("Query.user").is_some());
    assert_eq!(b.source_digest, hex(&zip));
}

#[test]
fn several_wsdl_and_unknown_files() {
    let zip = zip_of(&[("a.wsdl", b"<a/>"), ("b.wsdl", b"<b/>")]);
    assert!(matches!(
        compile(&zip).unwrap_err(),
        ImportError::Invalid { .. }
    ));
    let zip = zip_of(&[("a.txt", b"x"), ("b.txt", b"y")]);
    assert!(matches!(
        compile(&zip).unwrap_err(),
        ImportError::Unsupported(m) if m.starts_with("archive contains no recognised contract")
    ));
    // A single file is compiled on its own.
    let openapi = std::fs::read(fixtures().join("openapi/orders-3.0.yaml")).unwrap();
    let zip = zip_of(&[("orders.yaml", &openapi)]);
    let b = compile(&zip).unwrap();
    assert_eq!(b.source_digest, hex(&zip));
    assert!(!b.operations.is_empty());
}

#[test]
fn macos_metadata_and_hidden_entries_are_dropped() {
    let zip = zip_of(&[
        ("api/a.proto", b"syntax = \"proto3\"; message A { int32 x = 1; } service S { rpc Get(A) returns (A); }"),
        ("__MACOSX/api/._a.proto", b"\x00\x05\x16\x07 not a proto"),
        ("api/.DS_Store", b"\x00\x00\x00\x01Bud1"),
        ("api/.git/config", b"[core]"),
    ]);
    let paths: Vec<String> = unpack_archive(&zip)
        .unwrap()
        .into_iter()
        .map(|f| f.path)
        .collect();
    assert_eq!(paths, ["a.proto"]);
    assert!(!compile(&zip).unwrap().operations.is_empty());
}

#[test]
fn a_single_contract_beside_incidental_files_compiles() {
    let openapi = std::fs::read(fixtures().join("openapi/orders-3.0.yaml")).unwrap();
    let zip = zip_of(&[
        (
            "README.md",
            b"# Orders API\n\nThe service and message formats are below.",
        ),
        ("LICENSE", b"MIT"),
        ("openapi.yaml", &openapi),
    ]);
    let b = compile(&zip).unwrap();
    assert!(!b.operations.is_empty());
    assert_eq!(b.source_digest, hex(&zip));

    // Two contracts are named in the error.
    let swagger = std::fs::read(fixtures().join("swagger/orders-2.0.json")).unwrap();
    let zip = zip_of(&[
        ("a.yaml", &openapi),
        ("b.json", &swagger),
        ("README.md", b"x"),
    ]);
    match compile(&zip).unwrap_err() {
        ImportError::Invalid { message, .. } => {
            assert!(
                message.contains("a.yaml") && message.contains("b.json"),
                "{message}"
            );
            assert!(!message.contains("README"), "{message}");
        }
        other => panic!("{other:?}"),
    }
}
