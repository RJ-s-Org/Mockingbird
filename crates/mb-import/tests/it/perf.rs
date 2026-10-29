//! Timing harnesses for the import hot paths (the `MB-IMPORT-PERF-*` audit
//! findings). They are ignored by default; run them in release mode:
//!
//! ```sh
//! cargo test -p mockingbird-import --all-features --release --test it perf:: -- --ignored --nocapture --test-threads 1
//! ```
//!
//! Each prints the time at a size `n` and at `10n` (or a with/without pair),
//! so the scaling is visible. They assert nothing about the time.
#![cfg(test)]

use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// The best of `runs` timings of `f`.
fn best<T>(runs: usize, mut f: impl FnMut() -> T) -> Duration {
    (0..runs)
        .map(|_| {
            let start = Instant::now();
            std::hint::black_box(f());
            start.elapsed()
        })
        .min()
        .unwrap_or_default()
}

/// An OpenAPI 3.0 document with `n` operations; operation `i` returns
/// component `C{i}`, and each component points at the next, so every
/// response reaches all `n` components.
fn chained_spec(n: usize) -> Value {
    let mut schemas = serde_json::Map::new();
    let mut paths = serde_json::Map::new();
    for i in 0..n {
        schemas.insert(
            format!("C{i}"),
            json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string"},
                    "count": {"type": "integer", "minimum": 0},
                    "tags": {"type": "array", "items": {"type": "string"}},
                    "next": {"$ref": format!("#/components/schemas/C{}", (i + 1) % n)}
                }
            }),
        );
        paths.insert(
            format!("/r{i}"),
            json!({"get": {
                "operationId": format!("get{i}"),
                "parameters": [{"name": "q", "in": "query", "schema": {"type": "string"}}],
                "responses": {"200": {
                    "description": "ok",
                    "content": {"application/json": {
                        "schema": {"$ref": format!("#/components/schemas/C{i}")}
                    }}
                }}
            }}),
        );
    }
    json!({
        "openapi": "3.0.3",
        "info": {"title": "Chained", "version": "1"},
        "paths": paths,
        "components": {"schemas": schemas}
    })
}

/// MB-IMPORT-PERF-2: `compile_draft` at `n` and `10n` components and
/// operations, next to the plain compile of the same document.
#[cfg(feature = "smart")]
#[test]
#[ignore = "timing harness"]
fn perf_2_compile_draft() {
    for n in [40, 400] {
        let doc = chained_spec(n);
        let bytes = serde_json::to_vec(&doc).unwrap();
        let draft = best(3, || {
            mb_import::smart::assemble::compile_draft(&doc).unwrap()
        });
        let plain = best(3, || mb_import::compile(&bytes).unwrap());
        println!("PERF-2 n={n}: compile_draft {draft:?} (plain compile {plain:?})");
    }
}

/// MB-IMPORT-PERF-3: `bundle_warnings` when every response reaches `n`
/// definitions.
#[test]
#[ignore = "timing harness"]
fn perf_3_bundle_warnings() {
    for n in [100, 1000] {
        let bytes = serde_json::to_vec(&chained_spec(n)).unwrap();
        let bundle = mb_import::compile(&bytes).unwrap();
        let t = best(3, || mb_import::bundle_warnings(&bundle));
        println!("PERF-3 n={n}: bundle_warnings {t:?}");
    }
}

/// MB-IMPORT-PERF-4: `compile` of a YAML document against one parse of it.
#[test]
#[ignore = "timing harness"]
fn perf_4_compile_parses_once() {
    for n in [200, 2000] {
        let doc = chained_spec(n);
        let yaml = serde_saphyr::to_string(&doc).unwrap();
        let json = serde_json::to_vec(&doc).unwrap();
        let parse = best(3, || mb_import::parse_document(yaml.as_bytes()).unwrap());
        let yaml_compile = best(3, || mb_import::compile(yaml.as_bytes()).unwrap());
        let json_parse = best(3, || mb_import::parse_document(&json).unwrap());
        let json_compile = best(3, || mb_import::compile(&json).unwrap());
        println!(
            "PERF-4 n={n}: YAML parse {parse:?}, compile {yaml_compile:?}; JSON parse {json_parse:?}, compile {json_compile:?}"
        );
    }
}

/// A HAR-like JSON object of about `mb` MiB.
#[cfg(feature = "smart")]
fn big_json(mb: usize, har: bool) -> Vec<u8> {
    let entry = json!({
        "request": {"method": "GET", "url": "https://api.example.com/items/1", "headers": []},
        "response": {"status": 200, "content": {"text": "{\"id\":1,\"name\":\"widget\",\"tags\":[\"a\",\"b\"]}"}}
    });
    let one = serde_json::to_vec(&entry).unwrap().len();
    let entries: Vec<Value> = std::iter::repeat_n(entry, mb * 1024 * 1024 / one).collect();
    let doc = if har {
        json!({"log": {"version": "1.2", "entries": entries}})
    } else {
        json!({"data": entries})
    };
    serde_json::to_vec(&doc).unwrap()
}

/// MB-IMPORT-PERF-5: `detect_kind` on 2 and 20 MiB of JSON.
#[cfg(feature = "smart")]
#[test]
#[ignore = "timing harness"]
fn perf_5_detect_kind() {
    for mb in [2, 20] {
        for har in [true, false] {
            let bytes = big_json(mb, har);
            let t = best(3, || mb_import::smart::detect_kind(None, &bytes));
            println!("PERF-5 {mb} MiB har={har}: detect_kind {t:?}");
        }
    }
}

/// MB-IMPORT-PERF-6: a fetch over `hops` redirects with and without a
/// bundle of extra roots (the platform's, when it has a PEM bundle).
#[cfg(feature = "smart")]
#[tokio::test]
#[ignore = "timing harness"]
async fn perf_6_fetch_redirects() {
    use mb_import::smart::fetch::{FetchPolicy, fetch};
    use wiremock::matchers::path;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let Some(pem) = ["/etc/ssl/cert.pem", "/etc/ssl/certs/ca-certificates.crt"]
        .iter()
        .find_map(|p| std::fs::read(p).ok())
    else {
        println!("PERF-6 skipped: no platform PEM bundle");
        return;
    };
    let hops = 10;
    let server = MockServer::start().await;
    for i in 0..hops {
        Mock::given(path(format!("/h{i}")))
            .respond_with(
                ResponseTemplate::new(302).insert_header("location", format!("/h{}", i + 1)),
            )
            .mount(&server)
            .await;
    }
    Mock::given(path(format!("/h{hops}")))
        .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
        .mount(&server)
        .await;
    let url = format!("{}/h0", server.uri());
    let mut policy = FetchPolicy::from_allowlist(&["127.0.0.0/8".to_string()]).unwrap();
    policy.max_redirects = hops;
    for (label, roots) in [("no extra roots", Vec::new()), ("extra roots", pem)] {
        policy.extra_roots_pem = roots;
        let mut times = Vec::new();
        for _ in 0..3 {
            let start = Instant::now();
            fetch(&url, &policy).await.unwrap();
            times.push(start.elapsed());
        }
        let t = times.into_iter().min().unwrap_or_default();
        println!("PERF-6 {hops} redirects, {label}: fetch {t:?}");
    }
}

/// MB-IMPORT-PERF-7: path templating of `n` sibling paths whose responses
/// all have distinct but compatible shapes.
#[cfg(feature = "smart")]
#[test]
#[ignore = "timing harness"]
fn perf_7_path_templates() {
    use mb_import::smart::paths::{Shape, infer_templates_with};
    use std::collections::BTreeMap;

    for n in [200, 2000] {
        let paths: Vec<String> = (0..n).map(|i| format!("/api/items/slug-{i}")).collect();
        let shapes: BTreeMap<String, Shape> = paths
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let body = json!({"a": 1, "b": 2, "c": 3, format!("k{i}"): 4});
                (p.clone(), Shape::of(&body))
            })
            .collect();
        let t = best(3, || infer_templates_with(&paths, &shapes));
        println!("PERF-7 n={n}: infer_templates_with {t:?}");
    }
}

/// An SDL with `interfaces` interfaces and `per` object types implementing
/// each.
fn interface_sdl(interfaces: usize, per: usize) -> String {
    let mut sdl = String::from("type Query {\n  node: Node0\n}\n");
    for i in 0..interfaces {
        sdl.push_str(&format!("interface Node{i} {{\n  id: ID!\n}}\n"));
        for j in 0..per {
            sdl.push_str(&format!(
                "type T{i}x{j} implements Node{i} {{\n  id: ID!\n  name: String\n}}\n"
            ));
        }
    }
    sdl
}

/// MB-IMPORT-PERF-12: GraphQL compile at `n` and `10n` interfaces with ten
/// implementors each.
#[test]
#[ignore = "timing harness"]
fn perf_12_graphql_interfaces() {
    for n in [100, 1000] {
        let sdl = interface_sdl(n, 10);
        let t = best(3, || mb_import::compile(sdl.as_bytes()).unwrap());
        println!("PERF-12 n={n}: compile {t:?}");
    }
}
