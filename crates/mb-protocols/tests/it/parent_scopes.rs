//! Parent-scoped record identity: a record addressed under parent path parameters by
//! a key that is not unique on its own is told apart by its parent (issue `#5` of
//! `a/x` is not `#5` of `b/x`), and its sequential number counts within the parent,
//! shared with the types that number with it there (an issue and a pull request).
#![cfg(feature = "http-rest")]

use axum::http::StatusCode;
use serde_json::{Value, json};

use crate::support::{self, HarnessOptions};

const CONTRACT: &str = r##"
openapi: 3.0.3
info: {title: Repos, version: "1"}
paths:
  /repos/{owner}/{repo}/issues:
    parameters:
      - {name: owner, in: path, required: true, schema: {type: string}}
      - {name: repo, in: path, required: true, schema: {type: string}}
    get:
      operationId: listIssues
      responses:
        "200": {description: ok, content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/Issue"}}}}}
    post:
      operationId: createIssue
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, required: [title], properties: {title: {type: string}}}}}
      responses:
        "201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Issue"}}}}
  /repos/{owner}/{repo}/issues/{issue_number}:
    parameters:
      - {name: owner, in: path, required: true, schema: {type: string}}
      - {name: repo, in: path, required: true, schema: {type: string}}
      - {name: issue_number, in: path, required: true, schema: {type: integer}}
    get:
      operationId: getIssue
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Issue"}}}}
        "404": {description: missing}
    patch:
      operationId: updateIssue
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, properties: {title: {type: string}}}}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Issue"}}}}
    delete:
      operationId: deleteIssue
      responses:
        "204": {description: gone}
  /repos/{owner}/{repo}/pulls:
    parameters:
      - {name: owner, in: path, required: true, schema: {type: string}}
      - {name: repo, in: path, required: true, schema: {type: string}}
    post:
      operationId: createPull
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, required: [title], properties: {title: {type: string}}}}}
      responses:
        "201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/PullRequest"}}}}
  /repos/{owner}/{repo}/pulls/{pull_number}:
    parameters:
      - {name: owner, in: path, required: true, schema: {type: string}}
      - {name: repo, in: path, required: true, schema: {type: string}}
      - {name: pull_number, in: path, required: true, schema: {type: integer}}
    get:
      operationId: getPull
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/PullRequest"}}}}
  /v1/customers/{customer}/sources:
    parameters:
      - {name: customer, in: path, required: true, schema: {type: string}}
    post:
      operationId: createSource
      requestBody:
        content: {application/json: {schema: {type: object, properties: {kind: {type: string}}}}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Source"}}}}
  /v1/customers/{customer}/sources/{id}:
    parameters:
      - {name: customer, in: path, required: true, schema: {type: string}}
      - {name: id, in: path, required: true, schema: {type: string, pattern: "^src_[A-Za-z0-9]+$"}}
    get:
      operationId: getSource
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Source"}}}}
components:
  schemas:
    Issue:
      type: object
      required: [id, number, title]
      properties:
        id: {type: integer}
        number: {type: integer}
        title: {type: string}
        pull_request:
          type: object
          properties: {url: {type: string}, html_url: {type: string}}
    PullRequest:
      type: object
      required: [id, number, title]
      properties:
        id: {type: integer}
        number: {type: integer}
        title: {type: string}
    Source:
      type: object
      required: [id]
      properties:
        id: {type: string, pattern: "^src_[A-Za-z0-9]+$"}
        customer: {type: string}
        kind: {type: string}
"##;

/// The runtime of [`CONTRACT`], with the identity a list goes by described.
fn harness(script: Vec<Value>) -> support::DescribedHarness {
    let world = json!({"types": {
        "issues": {"identity": {"member": "id"}},
        "pulls": {"identity": {"member": "id"}},
    }});
    support::harness(CONTRACT, script, &HarnessOptions::default(), |rt| {
        support::with_effective(rt, world, false)
    })
}

fn source(headers: &axum::http::HeaderMap) -> &str {
    headers
        .get("x-mockingbird-source")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
}

/// The stored ids of type `t`, as the store keys them, sorted.
async fn keys(h: &support::DescribedHarness, t: &str) -> Vec<String> {
    let session = mb_state::SessionKey {
        workspace: h.rt.workspace.clone(),
        mock: h.rt.name.clone(),
        session: "default".into(),
    };
    let all = h.rt.state.export(&session).await.unwrap();
    let mut ids: Vec<String> = all
        .iter()
        .filter(|r| r.resource_type == t)
        .map(|r| r.id.clone())
        .collect();
    ids.sort();
    ids
}

async fn create(h: &support::DescribedHarness, uri: &str, title: &str) -> Value {
    let (status, headers, body) = h
        .send("POST", uri, Some(json!({"title": title})), &[])
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(source(&headers), "state", "{body}");
    body
}

/// Issue `#5` read in `a/x` and in `b/x` are two records, even when the model gives
/// both the same `id`: each read after is answered from its own repository's record,
/// with no model call.
#[tokio::test]
async fn one_number_read_in_two_repositories_is_two_records() {
    let h = harness(vec![
        json!({"id": 101, "number": 5, "title": "A five"}),
        json!({"id": 101, "number": 5, "title": "B five"}),
    ]);
    let (status, _, a) = h.send("GET", "/repos/a/x/issues/5", None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{a}");
    assert_eq!(a["title"], "A five");
    let (status, _, b) = h.send("GET", "/repos/b/x/issues/5", None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{b}");
    assert_eq!(b["title"], "B five", "repository b took a's issue: {b}");
    assert_eq!(
        keys(&h, "issues").await,
        ["a\u{1f}x\u{1f}101", "b\u{1f}x\u{1f}101"]
    );

    let calls = h.provider.requests().len();
    for (repo, title) in [("a", "A five"), ("b", "B five")] {
        let (status, headers, read) = h
            .send("GET", &format!("/repos/{repo}/x/issues/5"), None, &[])
            .await;
        assert_eq!(status, StatusCode::OK, "{read}");
        assert_eq!(source(&headers), "state", "{read}");
        assert_eq!(read["title"], title, "{read}");
    }
    assert_eq!(h.provider.requests().len(), calls, "answered from state");
}

/// Issue `#5` of `a/x` and of `b/x` with ids of their own: each read after finds its
/// repository's by number, though the other repository's read was the last to say
/// which id `#5` is.
#[tokio::test]
async fn one_number_with_two_ids_reads_apart() {
    let h = harness(vec![
        json!({"id": 101, "number": 5, "title": "A five"}),
        json!({"id": 202, "number": 5, "title": "B five"}),
    ]);
    h.send("GET", "/repos/a/x/issues/5", None, &[]).await;
    h.send("GET", "/repos/b/x/issues/5", None, &[]).await;
    assert_eq!(
        keys(&h, "issues").await,
        ["a\u{1f}x\u{1f}101", "b\u{1f}x\u{1f}202"]
    );
    for (repo, title) in [("a", "A five"), ("b", "B five"), ("a", "A five")] {
        let (_, headers, read) = h
            .send("GET", &format!("/repos/{repo}/x/issues/5"), None, &[])
            .await;
        assert_eq!(source(&headers), "state", "{read}");
        assert_eq!(read["title"], title, "{read}");
    }
}

/// A delete under `a/x` removes `a/x`'s issue `#1` only: `b/x`'s still reads.
#[tokio::test]
async fn a_delete_under_one_repository_leaves_the_other_s() {
    let h = support::harness(
        CONTRACT,
        Vec::new(),
        &HarnessOptions {
            unknown_ids_not_found: true,
            ..HarnessOptions::default()
        },
        |rt| {
            let world = json!({"types": {"issues": {"identity": {"member": "id"}}}});
            support::with_effective(rt, world, false)
        },
    );
    create(&h, "/repos/a/x/issues", "A one").await;
    create(&h, "/repos/b/x/issues", "B one").await;
    let (status, _, _) = h.send("DELETE", "/repos/a/x/issues/1", None, &[]).await;
    assert!(status.is_success(), "{status}");
    let (status, _, gone) = h.send("GET", "/repos/a/x/issues/1", None, &[]).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{gone}");
    let (status, headers, kept) = h.send("GET", "/repos/b/x/issues/1", None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{kept}");
    assert_eq!(source(&headers), "state", "{kept}");
    assert_eq!(kept["title"], "B one", "{kept}");
}

/// A record stored under its own id alone (seeded, or written before its type was
/// scoped) is found under a parent its links do not contradict; a write there moves it
/// under that parent, and a delete there removes it: it does not read back.
#[tokio::test]
async fn a_record_stored_under_no_parent_moves_to_the_one_that_writes_it() {
    let h = harness(Vec::new());
    let session = mb_state::SessionKey {
        workspace: h.rt.workspace.clone(),
        mock: h.rt.name.clone(),
        session: "default".into(),
    };
    h.rt.state
        .upsert(
            &session,
            mb_state::StoredResource {
                resource_type: "issues".into(),
                id: "900".into(),
                data: json!({"id": 900, "number": 9, "title": "Seeded"}),
            },
        )
        .await
        .unwrap();
    let (_, headers, read) = h.send("GET", "/repos/a/x/issues/9", None, &[]).await;
    assert_eq!(source(&headers), "state", "{read}");
    assert_eq!(read["title"], "Seeded", "{read}");
    let (status, _, patched) = h
        .send(
            "PATCH",
            "/repos/a/x/issues/9",
            Some(json!({"title": "Patched"})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{patched}");
    assert_eq!(keys(&h, "issues").await, ["a\u{1f}x\u{1f}900"]);
    let (status, _, _) = h.send("DELETE", "/repos/a/x/issues/9", None, &[]).await;
    assert!(status.is_success(), "{status}");
    assert!(keys(&h, "issues").await.is_empty());
}

/// Creates number within their repository: 1 and 2 in `a/x`, 1 in `b/x`; each list
/// shows its repository's issues only, and each read by number its own.
#[tokio::test]
async fn creates_number_within_their_repository_and_lists_stay_apart() {
    let h = harness(Vec::new());
    let first = create(&h, "/repos/a/x/issues", "A one").await;
    let second = create(&h, "/repos/a/x/issues", "A two").await;
    let other = create(&h, "/repos/b/x/issues", "B one").await;
    assert_eq!(first["number"], 1, "{first}");
    assert_eq!(second["number"], 2, "{second}");
    assert_eq!(other["number"], 1, "{other}");

    for (repo, titles) in [("a", vec!["A one", "A two"]), ("b", vec!["B one"])] {
        let (status, headers, list) = h
            .send("GET", &format!("/repos/{repo}/x/issues"), None, &[])
            .await;
        assert_eq!(status, StatusCode::OK, "{list}");
        let last = h.records().pop().map(|r| (r.fallback_reason, r.request));
        assert_eq!(source(&headers), "state", "{list} {last:?}");
        let mut listed: Vec<&str> = list
            .as_array()
            .unwrap_or_else(|| panic!("{list}"))
            .iter()
            .filter_map(|i| i["title"].as_str())
            .collect();
        listed.sort_unstable();
        assert_eq!(listed, titles, "{list}");
    }
    let (_, headers, read) = h.send("GET", "/repos/b/x/issues/1", None, &[]).await;
    assert_eq!(source(&headers), "state", "{read}");
    assert_eq!(read["title"], "B one", "{read}");
    let (_, headers, read) = h.send("GET", "/repos/a/x/issues/1", None, &[]).await;
    assert_eq!(source(&headers), "state", "{read}");
    assert_eq!(read["title"], "A one", "{read}");
    assert!(h.provider.requests().is_empty(), "no model call");
}

/// An issue's `pull_request` member holds none of a pull request's keys: it marks the
/// issue as one, so the two number together under a repository (issue 1, pull 2,
/// issue 3), and another repository starts at 1.
#[tokio::test]
async fn issues_and_pull_requests_share_one_sequence_per_repository() {
    let h = harness(Vec::new());
    let issue = create(&h, "/repos/a/x/issues", "First issue").await;
    let pull = create(&h, "/repos/a/x/pulls", "First pull").await;
    let again = create(&h, "/repos/a/x/issues", "Second issue").await;
    let elsewhere = create(&h, "/repos/b/x/pulls", "Elsewhere").await;
    assert_eq!(issue["number"], 1, "{issue}");
    assert_eq!(pull["number"], 2, "{pull}");
    assert_eq!(again["number"], 3, "{again}");
    assert_eq!(elsewhere["number"], 1, "{elsewhere}");
    let (_, headers, read) = h.send("GET", "/repos/a/x/pulls/2", None, &[]).await;
    assert_eq!(source(&headers), "state", "{read}");
    assert_eq!(read["title"], "First pull", "{read}");
}

/// A key the contract marks unique on its own (a pattern with a literal prefix,
/// Stripe's `src_…`) stays global: its record is stored under its id alone.
#[tokio::test]
async fn a_prefixed_id_stays_global() {
    let h = harness(vec![
        json!({"id": "src_9s8dF2", "customer": "cus_A", "kind": "card"}),
    ]);
    let (status, _, made) = h
        .send(
            "POST",
            "/v1/customers/cus_A/sources",
            Some(json!({"kind": "card"})),
            &[support::SEED],
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{made}");
    assert_eq!(made["id"], "src_9s8dF2", "{made}");
    assert_eq!(keys(&h, "sources").await, ["src_9s8dF2"]);
    let (_, headers, read) = h
        .send("GET", "/v1/customers/cus_A/sources/src_9s8dF2", None, &[])
        .await;
    assert_eq!(source(&headers), "state", "{read}");
    assert_eq!(read["kind"], "card", "{read}");
}
