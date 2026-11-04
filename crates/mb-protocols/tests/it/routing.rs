//! REST routing: OData-style keys served end to end, every contract of the
//! repository routed, and the router agreeing with `matchit` (which it
//! replaced) wherever `matchit` accepted the routes.
//!
//! The corpus tests never print contract text: a failure names the file
//! and counts or indexes operations.
#![cfg(test)]

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use mb_engine::{Engine, EngineConfig};
use mb_mcm::{Binding, Bundle, HttpMethod};
use mb_protocols::http_rest::{self, RouteTable};
use mb_protocols::{MockRuntime, RequestRecord, RequestRecorder};
use mb_state::MemoryStore;
use serde_json::{Value, json};
use tower::ServiceExt;

const ODATA: &str = r#"
openapi: 3.0.3
info: {title: Ledger, version: "1"}
servers: [{url: /api/v1}]
components:
  parameters:
    company: {name: company_id, in: path, required: true, schema: {type: string}}
    customer: {name: customer_id, in: path, required: true, schema: {type: string}}
    invoice: {name: sales_invoice_id, in: path, required: true, schema: {type: string}}
  schemas:
    Customer:
      type: object
      required: [id, displayName]
      properties: {id: {type: string}, displayName: {type: string}}
    Invoice:
      type: object
      required: [id, totalAmount]
      properties: {id: {type: string}, totalAmount: {type: number}}
  responses:
    customer:
      description: ok
      content: {application/json: {schema: {$ref: '#/components/schemas/Customer'}}}
    invoice:
      description: ok
      content: {application/json: {schema: {$ref: '#/components/schemas/Invoice'}}}
paths:
  /companies({company_id})/customers:
    parameters: [{$ref: '#/components/parameters/company'}]
    get:
      operationId: listCustomers
      responses:
        "200":
          description: ok
          content: {application/json: {schema: {type: array, items: {$ref: '#/components/schemas/Customer'}}}}
  /companies({company_id})/customers({customer_id}):
    parameters: [{$ref: '#/components/parameters/company'}, {$ref: '#/components/parameters/customer'}]
    get: {operationId: getCustomer, responses: {"200": {$ref: '#/components/responses/customer'}}}
    patch:
      operationId: updateCustomer
      requestBody:
        content: {application/json: {schema: {type: object, properties: {displayName: {type: string}}}}}
      responses: {"200": {$ref: '#/components/responses/customer'}}
    delete: {operationId: deleteCustomer, responses: {"204": {description: deleted}}}
  /companies({company_id})/salesInvoices:
    parameters: [{$ref: '#/components/parameters/company'}]
    get:
      operationId: listSalesInvoices
      responses:
        "200":
          description: ok
          content: {application/json: {schema: {type: array, items: {$ref: '#/components/schemas/Invoice'}}}}
  /companies({company_id})/salesInvoices({sales_invoice_id}):
    parameters: [{$ref: '#/components/parameters/company'}, {$ref: '#/components/parameters/invoice'}]
    get: {operationId: getSalesInvoice, responses: {"200": {$ref: '#/components/responses/invoice'}}}
    patch:
      operationId: updateSalesInvoice
      requestBody:
        content: {application/json: {schema: {type: object, properties: {totalAmount: {type: number}}}}}
      responses: {"200": {$ref: '#/components/responses/invoice'}}
    delete: {operationId: deleteSalesInvoice, responses: {"204": {description: deleted}}}
"#;

#[derive(Default)]
struct Operations(Mutex<Vec<(Option<String>, u16)>>);

impl RequestRecorder for Operations {
    fn record(&self, r: RequestRecord) {
        self.0.lock().unwrap().push((r.operation_id, r.status));
    }
}

fn request(method: &str, uri: &str) -> Request<Body> {
    let builder = Request::builder().method(method).uri(uri);
    if method == "PATCH" {
        builder
            .header("content-type", "application/json")
            .body(Body::from(json!({}).to_string()))
            .unwrap()
    } else {
        builder.body(Body::empty()).unwrap()
    }
}

#[tokio::test]
async fn serves_sibling_collections_under_an_odata_key() {
    let bundle = Arc::new(mb_import::compile(ODATA.as_bytes()).unwrap());
    assert!(RouteTable::new(&bundle).skipped().is_empty());
    let recorder = Arc::new(Operations::default());
    let mut rt = MockRuntime::new(
        "ledger",
        bundle,
        Arc::new(Engine::new(None, EngineConfig::default())),
        Arc::new(MemoryStore::new(Duration::from_secs(3600))),
    );
    rt.base_paths = mb_import::base_paths(ODATA.as_bytes());
    rt.recorder = Some(Arc::clone(&recorder) as Arc<dyn RequestRecorder>);
    let (router, skipped) = http_rest::router_and_skipped(Arc::new(rt));
    assert!(skipped.is_empty(), "{skipped:?}");

    let cases = [
        ("GET", "/companies(c1)/customers(k1)", "getCustomer", 200),
        (
            "PATCH",
            "/companies(c1)/customers(k1)",
            "updateCustomer",
            200,
        ),
        (
            "DELETE",
            "/companies(c1)/customers(k1)",
            "deleteCustomer",
            204,
        ),
        ("GET", "/companies(c1)/customers", "listCustomers", 200),
        (
            "GET",
            "/companies(c1)/salesInvoices(s1)",
            "getSalesInvoice",
            200,
        ),
        (
            "PATCH",
            "/companies(c1)/salesInvoices(s1)",
            "updateSalesInvoice",
            200,
        ),
        (
            "DELETE",
            "/companies(c1)/salesInvoices(s1)",
            "deleteSalesInvoice",
            204,
        ),
        (
            "GET",
            "/api/v1/companies(c%201)/salesInvoices",
            "listSalesInvoices",
            200,
        ),
        (
            "GET",
            "/api/v1/companies(c1)/salesInvoices(s1)/",
            "getSalesInvoice",
            200,
        ),
    ];
    for (method, uri, operation, status) in cases {
        let resp = router.clone().oneshot(request(method, uri)).await.unwrap();
        assert_eq!(resp.status().as_u16(), status, "{method} {uri}");
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        if method != "DELETE" && !operation.starts_with("list") {
            let body: Value = serde_json::from_slice(&body).unwrap();
            let member = if operation.ends_with("Customer") {
                "displayName"
            } else {
                "totalAmount"
            };
            assert!(body.get(member).is_some(), "{method} {uri}: {body}");
        }
        let last = recorder.0.lock().unwrap().last().cloned();
        assert_eq!(
            last,
            Some((Some(operation.to_owned()), status)),
            "{method} {uri}"
        );
    }

    for (method, uri, status) in [
        ("GET", "/companies(c1)/vendors(v1)", StatusCode::NOT_FOUND),
        ("GET", "/companies()/customers", StatusCode::NOT_FOUND),
        (
            "POST",
            "/companies(c1)/customers(k1)",
            StatusCode::METHOD_NOT_ALLOWED,
        ),
    ] {
        let resp = router.clone().oneshot(request(method, uri)).await.unwrap();
        assert_eq!(resp.status(), status, "{method} {uri}");
    }
}

#[tokio::test]
async fn a_conflicting_route_is_skipped_and_the_rest_is_served() {
    let src = r#"
openapi: 3.0.3
info: {title: Twins, version: "1"}
paths:
  /items/{id}:
    get: {operationId: getItem, responses: {"200": {description: ok}}}
  /items/{itemId}:
    get: {operationId: getItemAgain, responses: {"200": {description: ok}}}
    delete: {operationId: deleteItem, responses: {"204": {description: ok}}}
"#;
    let bundle = Arc::new(mb_import::compile(src.as_bytes()).unwrap());
    let rt = MockRuntime::new(
        "twins",
        bundle,
        Arc::new(Engine::new(None, EngineConfig::default())),
        Arc::new(MemoryStore::new(Duration::from_secs(3600))),
    );
    let (router, skipped) = http_rest::router_and_skipped(Arc::new(rt));
    let messages: Vec<String> = skipped.iter().map(ToString::to_string).collect();
    assert_eq!(
        messages,
        [
            "operation getItemAgain: cannot route GET /items/{itemId}: it matches the same \
          requests as operation getItem (GET /items/{id}), which takes them"
        ]
    );
    let resp = router
        .clone()
        .oneshot(request("GET", "/items/1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = router.oneshot(request("DELETE", "/items/1")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
}

// --- The repository's contracts ---------------------------------------

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn files(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            files(&path, out);
        } else {
            out.push(path);
        }
    }
}

struct Contract {
    /// Relative to the repository root.
    file: String,
    bundle: Bundle,
    base_paths: Vec<String>,
}

/// Every file under `root` (relative to the repository) that compiles to a
/// bundle with HTTP operations; panics when fewer than `at_least` do.
fn contracts(root: &str, at_least: usize) -> Vec<Contract> {
    let mut paths = Vec::new();
    files(&repo().join(root), &mut paths);
    let found: Vec<Contract> = paths
        .into_iter()
        .filter_map(|path| {
            let bytes = std::fs::read(&path).ok()?;
            let bundle = mb_import::compile(&bytes).ok()?;
            let file = path
                .strip_prefix(repo())
                .unwrap_or(&path)
                .display()
                .to_string();
            Some(Contract {
                file,
                base_paths: mb_import::base_paths(&bytes),
                bundle,
            })
        })
        .filter(|c| http_routes(&c.bundle).next().is_some())
        .collect();
    assert!(
        found.len() >= at_least,
        "{root}: {} HTTP contracts, expected at least {at_least}",
        found.len()
    );
    found
}

fn http_routes(bundle: &Bundle) -> impl Iterator<Item = (usize, &str, HttpMethod, &str)> {
    bundle
        .operations
        .iter()
        .enumerate()
        .filter_map(|(i, op)| match &op.binding {
            Binding::Http { method, path } => Some((i, op.id.as_str(), *method, path.as_str())),
            _ => None,
        })
}

/// `path` with every parameter replaced by `value` (a catch-all by two
/// segments), braces escaped as `{{`/`}}` kept as single braces.
fn sample(path: &str, value: &str) -> String {
    let mut out = String::new();
    let mut chars = path.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                out.push('{');
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                out.push('}');
            }
            '{' => {
                let mut name = String::new();
                for c in chars.by_ref() {
                    if c == '}' {
                        break;
                    }
                    name.push(c);
                }
                out.push_str(value);
                if name.starts_with('*') {
                    out.push_str("/r1s");
                }
            }
            c => out.push(c),
        }
    }
    out
}

#[test]
fn every_http_operation_of_every_repository_contract_is_routed() {
    let corpus = [
        contracts("scripts/llm-bench/contracts", 20),
        contracts("examples/apis", 15),
        contracts("fixtures", 3),
    ];
    let mut failures = Vec::new();
    let mut routed = 0;
    for contract in corpus.iter().flatten() {
        let table = RouteTable::with_base_paths(&contract.bundle, &contract.base_paths);
        let skipped: BTreeSet<&str> = table.skipped().iter().map(|e| e.operation()).collect();
        let mut bad = Vec::new();
        for (index, id, method, path) in http_routes(&contract.bundle) {
            let found = table.find(method, &sample(path, "p0q")).map(|(op, _)| op);
            if skipped.contains(id) || found.as_deref() != Some(id) {
                bad.push(index);
            } else {
                routed += 1;
            }
        }
        if !bad.is_empty() {
            // Operation indexes only: no contract text in the output.
            failures.push(format!(
                "{}: {} HTTP operations not routed (operation indexes {bad:?})",
                contract.file,
                bad.len()
            ));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
    assert!(routed >= 500, "only {routed} operations checked");
}

/// `matchit`'s routers for `bundle`, built as the REST adapter built them,
/// or `None` when one route was refused (the mock did not mount then).
fn matchit_routers(bundle: &Bundle) -> Option<HashMap<HttpMethod, matchit::Router<String>>> {
    let mut routers: HashMap<HttpMethod, matchit::Router<String>> = HashMap::new();
    for (_, id, method, path) in http_routes(bundle) {
        routers
            .entry(method)
            .or_default()
            .insert(normalize(path), id.to_owned())
            .ok()?;
    }
    Some(routers)
}

fn normalize(path: &str) -> &str {
    match path.strip_suffix('/') {
        Some(stripped) if !stripped.is_empty() => stripped,
        _ => path,
    }
}

/// Request paths probing `routes`: each route with sample parameters, and
/// with each parameter segment replaced by every static segment found at
/// that depth (so static and parameter siblings compete).
fn probes(routes: &[&str]) -> BTreeSet<String> {
    let mut statics: HashMap<usize, BTreeSet<&str>> = HashMap::new();
    for route in routes {
        for (depth, segment) in route.split('/').enumerate() {
            if !segment.contains(['{', '}']) {
                statics.entry(depth).or_default().insert(segment);
            }
        }
    }
    let mut out = BTreeSet::new();
    for &route in routes {
        let segments: Vec<&str> = route.split('/').collect();
        out.insert(sample(route, "p0q"));
        out.insert(sample(route, "p0q.json"));
        for (depth, segment) in segments.iter().enumerate() {
            if !segment.contains('{') {
                continue;
            }
            for literal in statics.get(&depth).into_iter().flatten().take(40) {
                let mut probe = segments.clone();
                probe[depth] = literal;
                out.insert(sample(&probe.join("/"), "p0q"));
            }
        }
    }
    out
}

type Found = Option<(String, Vec<(String, String)>)>;

fn ours(table: &RouteTable, method: HttpMethod, path: &str) -> Found {
    table.find(method, path).map(|(op, params)| {
        let mut params: Vec<(String, String)> = params
            .into_iter()
            .map(|(k, v)| (k, v.as_str().unwrap_or_default().to_owned()))
            .collect();
        params.sort();
        (op, params)
    })
}

fn theirs(
    routers: &HashMap<HttpMethod, matchit::Router<String>>,
    method: HttpMethod,
    path: &str,
) -> Found {
    let matched = routers.get(&method)?.at(normalize(path)).ok()?;
    let mut params: Vec<(String, String)> = matched
        .params
        .iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
    params.sort();
    Some((matched.value.clone(), params))
}

/// The routes, as a one-method bundle: operation `i` is `routes[i]`.
fn get_bundle(routes: &[&str]) -> Bundle {
    let paths: serde_json::Map<String, Value> = routes
        .iter()
        .enumerate()
        .map(|(i, route)| {
            (
                (*route).to_owned(),
                json!({"get": {"operationId": format!("r{i}"), "responses": {"200": {"description": "ok"}}}}),
            )
        })
        .collect();
    let doc = json!({"openapi": "3.0.3", "info": {"title": "T", "version": "1"}, "paths": paths});
    mb_import::compile(doc.to_string().as_bytes()).unwrap()
}

#[test]
fn routes_as_matchit_did_wherever_matchit_accepted_the_routes() {
    let mut sets: Vec<(String, Bundle)> = [
        &["/a/b/c", "/a/{x}/d"][..],
        &["/a/b/{y}/c", "/a/{x}/d/e"],
        &["/f/{x}.png", "/f/{x}", "/f/index.png"],
        &["/f/{x}.png", "/f/{x}.tar.gz", "/f/{x}.gz"],
        &["/u/{id}", "/u/{user_id}/posts"],
        &["/c({id})", "/c({id})/a"],
        &["/i/{id}:archive", "/i/{id}:restore", "/i/{id}", "/i/{id}/x"],
        &["/{*rest}", "/a/{x}", "/a/b"],
        &["/x/{{lit}}", "/x/{p}"],
        &["/a/b", "/a/{x}", "/a/b/c", "/a/{x}/d"],
        &["/a/pre{x}", "/a/p{x}", "/a/{y}"],
        &["/a/{x}/{y}", "/a/{x}/b"],
        &["/", "/{x}"],
        &["/a/{x}/{*rest}", "/a/{x}"],
        &["/a/x{p}y", "/a/{p}.json", "/a/{q}.json/meta"],
        &["/a/{p}x/b", "/a/{p}x/c"],
    ]
    .iter()
    .enumerate()
    .map(|(i, routes)| (format!("route set {i}"), get_bundle(routes)))
    .collect();
    for contract in [
        contracts("scripts/llm-bench/contracts", 20),
        contracts("examples/apis", 15),
        contracts("fixtures", 3),
    ]
    .into_iter()
    .flatten()
    {
        sets.push((contract.file, contract.bundle));
    }

    let mut compared = 0;
    let mut differences = Vec::new();
    for (name, bundle) in &sets {
        let Some(routers) = matchit_routers(bundle) else {
            continue;
        };
        let table = RouteTable::new(bundle);
        assert!(
            table.skipped().is_empty(),
            "{name}: matchit took every route"
        );
        let routes: Vec<&str> = http_routes(bundle).map(|(_, _, _, path)| path).collect();
        let paths = probes(&routes);
        let mut methods: Vec<HttpMethod> = Vec::new();
        for (_, _, method, _) in http_routes(bundle) {
            if !methods.contains(&method) {
                methods.push(method);
            }
        }
        let mut differ = 0;
        for &method in &methods {
            for path in &paths {
                compared += 1;
                if ours(&table, method, path) != theirs(&routers, method, path) {
                    differ += 1;
                }
            }
        }
        if differ > 0 {
            // Counts only: no contract text in the output.
            differences.push(format!("{name}: {differ} probes routed differently"));
        }
    }
    assert!(differences.is_empty(), "{differences:#?}");
    assert!(compared >= 2000, "only {compared} probes compared");
}
