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

/// A state store counting the `issues` records under `b/x` its listings return.
struct ListSpy {
    inner: std::sync::Arc<dyn mb_state::StateStore>,
    issues: std::sync::atomic::AtomicUsize,
}

impl ListSpy {
    fn count(&self, t: &str, listed: &[mb_state::StoredResource]) {
        if t == "issues" {
            let under_b = listed
                .iter()
                .filter(|r| r.id.starts_with("b\u{1f}"))
                .count();
            self.issues
                .fetch_add(under_b, std::sync::atomic::Ordering::SeqCst);
        }
    }

    fn read(&self) -> usize {
        self.issues.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl mb_state::StateStore for ListSpy {
    async fn get(
        &self,
        s: &mb_state::SessionKey,
        t: &str,
        id: &str,
    ) -> Result<Option<mb_state::StoredResource>, mb_state::StateError> {
        self.inner.get(s, t, id).await
    }
    async fn list(
        &self,
        s: &mb_state::SessionKey,
        t: &str,
        limit: usize,
    ) -> Result<Vec<mb_state::StoredResource>, mb_state::StateError> {
        let listed = self.inner.list(s, t, limit).await?;
        self.count(t, &listed);
        Ok(listed)
    }
    async fn list_prefix(
        &self,
        s: &mb_state::SessionKey,
        t: &str,
        prefix: &str,
        limit: usize,
    ) -> Result<Vec<mb_state::StoredResource>, mb_state::StateError> {
        let listed = self.inner.list_prefix(s, t, prefix, limit).await?;
        self.count(t, &listed);
        Ok(listed)
    }
    async fn upsert(
        &self,
        s: &mb_state::SessionKey,
        r: mb_state::StoredResource,
    ) -> Result<(), mb_state::StateError> {
        self.inner.upsert(s, r).await
    }
    async fn insert_seeded(
        &self,
        s: &mb_state::SessionKey,
        r: mb_state::StoredResource,
    ) -> Result<bool, mb_state::StateError> {
        self.inner.insert_seeded(s, r).await
    }
    async fn insert_seeded_all(
        &self,
        s: &mb_state::SessionKey,
        r: Vec<mb_state::StoredResource>,
    ) -> Result<Vec<bool>, mb_state::StateError> {
        self.inner.insert_seeded_all(s, r).await
    }
    async fn delete(
        &self,
        s: &mb_state::SessionKey,
        t: &str,
        id: &str,
    ) -> Result<bool, mb_state::StateError> {
        self.inner.delete(s, t, id).await
    }
    async fn reset(&self, s: &mb_state::SessionKey) -> Result<(), mb_state::StateError> {
        self.inner.reset(s).await
    }
    async fn reset_mock(&self, workspace: &str, mock: &str) -> Result<(), mb_state::StateError> {
        self.inner.reset_mock(workspace, mock).await
    }
    async fn touch(&self, s: &mb_state::SessionKey) -> Result<(), mb_state::StateError> {
        self.inner.touch(s).await
    }
    async fn touch_live(&self, s: &mb_state::SessionKey) -> Result<bool, mb_state::StateError> {
        self.inner.touch_live(s).await
    }
    async fn sessions(
        &self,
        workspace: &str,
        mock: &str,
    ) -> Result<Vec<mb_state::SessionInfo>, mb_state::StateError> {
        self.inner.sessions(workspace, mock).await
    }
    async fn export(
        &self,
        s: &mb_state::SessionKey,
    ) -> Result<Vec<mb_state::StoredResource>, mb_state::StateError> {
        self.inner.export(s).await
    }
    async fn seed(
        &self,
        s: &mb_state::SessionKey,
        resources: Vec<mb_state::StoredResource>,
    ) -> Result<(), mb_state::StateError> {
        self.inner.seed(s, resources).await
    }
    async fn ping(&self) -> Result<(), mb_state::StateError> {
        self.inner.ping().await
    }
    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<mb_state::StateChanged> {
        self.inner.subscribe()
    }
    fn response_cache(&self) -> Option<&mb_state::ResponseCache> {
        self.inner.response_cache()
    }
}

/// A list under one repository reads that repository's issues, not every one the
/// session holds: with 200 held in `b/x` and two in `a/x`, listing `a/x` returns its
/// two and reads none of `b/x`'s (after the first list of the type in the session,
/// which reads the type once to learn none is held under no parent).
#[tokio::test]
async fn a_list_under_one_repository_reads_that_repository_s_issues() {
    let spy = std::sync::Arc::new(ListSpy {
        inner: std::sync::Arc::new(mb_state::MemoryStore::new(std::time::Duration::from_secs(
            3600,
        ))),
        issues: std::sync::atomic::AtomicUsize::new(0),
    });
    let store = std::sync::Arc::clone(&spy);
    let world = json!({"types": {
        "issues": {"identity": {"member": "id"}},
        "pulls": {"identity": {"member": "id"}},
    }});
    let h = support::harness(
        CONTRACT,
        Vec::new(),
        &HarnessOptions::default(),
        |mut rt| {
            rt.state = store as std::sync::Arc<dyn mb_state::StateStore>;
            support::with_effective(rt, world, false)
        },
    );
    create(&h, "/repos/a/x/issues", "A one").await;
    create(&h, "/repos/a/x/issues", "A two").await;
    let session = mb_state::SessionKey {
        workspace: h.rt.workspace.clone(),
        mock: h.rt.name.clone(),
        session: "default".into(),
    };
    for n in 1..=200 {
        let resource = mb_state::StoredResource {
            resource_type: "issues".into(),
            id: format!("b\u{1f}x\u{1f}{}", 1000 + n),
            data: json!({"id": 1000 + n, "number": n, "title": format!("B {n}")}),
        };
        h.rt.state.upsert(&session, resource).await.unwrap();
    }
    for round in 0..2 {
        let before = spy.read();
        let (status, headers, list) = h.send("GET", "/repos/a/x/issues", None, &[]).await;
        assert_eq!(status, StatusCode::OK, "{list}");
        assert_eq!(source(&headers), "state", "{list}");
        let mut titles: Vec<&str> = list
            .as_array()
            .unwrap_or_else(|| panic!("{list}"))
            .iter()
            .filter_map(|i| i["title"].as_str())
            .collect();
        titles.sort_unstable();
        assert_eq!(titles, ["A one", "A two"], "{list}");
        let read = spy.read() - before;
        if round == 1 {
            assert_eq!(read, 0, "read {read} of b/x's issues to list a/x's");
        }
    }
}
