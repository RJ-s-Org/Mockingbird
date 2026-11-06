//! The effective world description of a contract version (tier 0 overlaid
//! with the newest revision, validated) and carrying a description forward
//! to a new contract version.
#![cfg(test)]
#![allow(clippy::panic)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, header};
use mb_admin::world::{carry_forward, effective_world, runtime_world};
use mb_admin::{AdminState, UPLOAD_HEADER, router, run_next_import};
use mb_auth::{AuthError, Authenticate, Principal, PrincipalKind, Role};
use mb_import::world::tier0;
use mb_mcm::Bundle;
use mb_mcm::world::{Author, RefusalCode};
use mb_protocols::world_gaps::GapCode;
use mb_state::{MemoryStore, StateStore};
use mb_store::{
    ContractStatus, ControlStore, DEFAULT_WORKSPACE, NewContractVersion, NewMock, NewWorldRevision,
    ResourceSource,
};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

/// Widgets with a `color` member.
const V1: &str = r##"
openapi: 3.0.0
info: {title: Widgets, version: "1"}
paths:
  /widgets:
    get:
      operationId: listWidgets
      responses:
        "200":
          description: ok
          content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/Widget"}}}}
    post:
      operationId: createWidget
      requestBody: {content: {application/json: {schema: {$ref: "#/components/schemas/Widget"}}}}
      responses:
        "201":
          description: created
          content: {application/json: {schema: {$ref: "#/components/schemas/Widget"}}}
  /widgets/{widgetId}:
    get:
      operationId: getWidget
      parameters: [{name: widgetId, in: path, required: true, schema: {type: string}}]
      responses:
        "200":
          description: ok
          content: {application/json: {schema: {$ref: "#/components/schemas/Widget"}}}
        "404": {description: missing}
components:
  schemas:
    Widget:
      type: object
      properties:
        id: {type: string}
        name: {type: string}
        color: {type: string}
"##;

/// [`V1`] without the `color` member.
fn v2() -> String {
    V1.replace("        color: {type: string}\n", "")
}

fn compile(src: &str) -> Bundle {
    mb_import::compile(src.as_bytes()).unwrap()
}

/// The resource type of `getWidget`.
fn widget_type(b: &Bundle) -> String {
    b.operations
        .iter()
        .find(|o| o.id == "getWidget")
        .and_then(|o| o.resource_type.clone())
        .unwrap()
}

async fn store_with_mock() -> (Arc<dyn ControlStore>, Uuid) {
    let store: Arc<dyn ControlStore> = mb_store::connect("sqlite::memory:").await.unwrap();
    let m = store
        .create_mock(
            DEFAULT_WORKSPACE,
            NewMock {
                name: "widgets".into(),
                route_prefix: None,
                persona: None,
                llm_profile: None,
                header_overrides: false,
                source: ResourceSource::Api,
                data_plane_auth: Default::default(),
                chaos: None,
                limits: Default::default(),
                behavior: Default::default(),
                hosts: vec![],
                webhooks: vec![],
            },
        )
        .await
        .unwrap();
    (store, m.id)
}

async fn add_version(store: &dyn ControlStore, mock: Uuid, src: &str) -> (i64, Bundle) {
    let bundle = compile(src);
    let v = store
        .add_contract_version(
            mock,
            NewContractVersion {
                source: src.as_bytes().to_vec(),
                bundle: bundle.clone(),
                status: ContractStatus::Approved,
                created_by: None,
            },
        )
        .await
        .unwrap();
    (v.version, bundle)
}

async fn revise(
    store: &dyn ControlStore,
    mock: Uuid,
    version: i64,
    author: Author,
    document: Value,
) -> i64 {
    store
        .add_world_revision(
            mock,
            version,
            NewWorldRevision {
                author,
                document,
                created_by: Some("tester".into()),
                expect: None,
            },
        )
        .await
        .unwrap()
        .revision
}

#[tokio::test]
async fn effective_world_without_revisions_is_tier0_at_revision_0() {
    let (store, mock) = store_with_mock().await;
    let (version, bundle) = add_version(store.as_ref(), mock, V1).await;
    let eff = effective_world(store.as_ref(), mock, version, &bundle)
        .await
        .unwrap();
    assert_eq!(eff.revision, 0);
    assert_eq!(eff.world, tier0(&bundle));
    assert!(eff.authors.is_empty(), "{:?}", eff.authors);
    assert!(eff.refused.is_empty(), "{:?}", eff.refused);
    assert!(
        eff.gaps.iter().all(|g| g.code != GapCode::Refused),
        "{:?}",
        eff.gaps
    );
}

#[tokio::test]
async fn overlay_entries_are_marked_with_their_author() {
    let (store, mock) = store_with_mock().await;
    let (version, bundle) = add_version(store.as_ref(), mock, V1).await;
    let t = widget_type(&bundle);
    // Revision 1 (an assistant) sets the order of the list and the widget's
    // unique member; revision 2 (a person) keeps both and adds `not_found`;
    // revision 3 (the assistant again) changes only the order and adds a
    // stale entry.
    revise(
        store.as_ref(),
        mock,
        version,
        Author::Agent,
        json!({
            "version": 1,
            "types": {t.clone(): {"unique": [{"members": ["name"]}]}},
            "operations": {"listWidgets": {"order": {"member": "name"}}},
        }),
    )
    .await;
    revise(
        store.as_ref(),
        mock,
        version,
        Author::User,
        json!({
            "version": 1,
            "types": {t.clone(): {"unique": [{"members": ["name"]}]}},
            "operations": {
                "listWidgets": {"order": {"member": "name"}},
                "getWidget": {"not_found": 404},
            },
        }),
    )
    .await;
    let latest = revise(
        store.as_ref(),
        mock,
        version,
        Author::Agent,
        json!({
            "version": 1,
            "types": {t.clone(): {"unique": [{"members": ["name"]}]}},
            "operations": {
                "listWidgets": {"order": {"member": "id"}},
                "getWidget": {"not_found": 404, "order": {"member": "nope"}},
            },
        }),
    )
    .await;

    let eff = effective_world(store.as_ref(), mock, version, &bundle)
        .await
        .unwrap();
    assert_eq!(eff.revision, latest);
    let unique = format!("/types/{t}/unique");
    let expected: BTreeMap<String, Author> = [
        (unique.as_str(), Author::Agent),
        ("/operations/listWidgets/order", Author::Agent),
        ("/operations/getWidget/not_found", Author::User),
    ]
    .into_iter()
    .map(|(k, a)| (k.to_string(), a))
    .collect();
    assert_eq!(eff.authors, expected);
    // The overlay is laid over tier 0: tier 0's entries stay.
    let base = tier0(&bundle);
    assert_eq!(
        eff.world.operations["getWidget"].kind,
        base.operations["getWidget"].kind
    );
    assert_eq!(eff.world.operations["getWidget"].not_found, Some(404));
    assert_eq!(
        eff.world.operations["listWidgets"]
            .order
            .as_ref()
            .map(|o| o.member.as_str()),
        Some("id")
    );
    // The stale entry is refused, listed as a gap, and not applied.
    assert_eq!(eff.refused.len(), 1, "{:?}", eff.refused);
    assert_eq!(eff.refused[0].path, "/operations/getWidget/order/member");
    assert_eq!(eff.refused[0].code, RefusalCode::Unresolved);
    assert!(eff.world.operations["getWidget"].order.is_none());
    assert_eq!(
        eff.gaps
            .iter()
            .filter(|g| g.code == GapCode::Refused)
            .count(),
        1,
        "{:?}",
        eff.gaps
    );
}

#[tokio::test]
async fn runtime_world_equals_effective_world_without_authors() {
    let (store, mock) = store_with_mock().await;
    let (version, bundle) = add_version(store.as_ref(), mock, V1).await;
    let base = tier0(&bundle);
    let widget = widget_type(&bundle);
    let check = |label: &'static str| {
        let store = Arc::clone(&store);
        let bundle = bundle.clone();
        let base = base.clone();
        async move {
            let eff = effective_world(store.as_ref(), mock, version, &bundle)
                .await
                .unwrap();
            let (world, revision) = runtime_world(store.as_ref(), mock, version, &bundle, &base)
                .await
                .unwrap();
            assert_eq!((&world, revision), (&eff.world, eff.revision), "{label}");
        }
    };
    check("no revision").await;
    revise(
        store.as_ref(),
        mock,
        version,
        Author::Agent,
        json!({"version": 1, "types": {widget.clone(): {"identity": {"member": "name"}}}}),
    )
    .await;
    check("one revision").await;
    revise(
        store.as_ref(),
        mock,
        version,
        Author::User,
        json!({
            "version": 1,
            "types": {widget.clone(): {"identity": {"member": "color"}}},
            "operations": {"getWidget": {"target": {"path_param": "missing"}}},
        }),
    )
    .await;
    check("a revision with a refused entry").await;
    revise(
        store.as_ref(),
        mock,
        version,
        Author::Agent,
        json!({"types": 5}),
    )
    .await;
    check("a revision that is not a world").await;
    let (world, revision) = runtime_world(store.as_ref(), mock, version, &bundle, &base)
        .await
        .unwrap();
    assert_eq!((world, revision), (base.clone(), 3));
}

#[tokio::test]
async fn refused_overlay_entries_fall_back_to_tier0() {
    let (store, mock) = store_with_mock().await;
    let (version, bundle) = add_version(store.as_ref(), mock, V1).await;
    let base = tier0(&bundle);
    let target = base.operations["getWidget"].target.clone();
    assert!(target.is_some(), "tier 0 has a target for getWidget");
    revise(
        store.as_ref(),
        mock,
        version,
        Author::Agent,
        json!({
            "version": 1,
            "operations": {"getWidget": {"target": {"path_param": "missing"}}},
        }),
    )
    .await;
    let eff = effective_world(store.as_ref(), mock, version, &bundle)
        .await
        .unwrap();
    assert_eq!(eff.world.operations["getWidget"].target, target);
    assert!(!eff.refused.is_empty());
    assert!(eff.authors.is_empty(), "{:?}", eff.authors);
}

#[tokio::test]
async fn carry_forward_keeps_valid_entries_and_drops_stale() {
    let (store, mock) = store_with_mock().await;
    let (from, b1) = add_version(store.as_ref(), mock, V1).await;
    let t = widget_type(&b1);
    revise(
        store.as_ref(),
        mock,
        from,
        Author::Agent,
        json!({
            "version": 1,
            "types": {t.clone(): {"unique": [{"members": ["color"]}]}},
            "operations": {
                "listWidgets": {"order": {"member": "name"}},
                "getWidget": {"not_found": 404},
            },
        }),
    )
    .await;
    let (to, b2) = add_version(store.as_ref(), mock, &v2()).await;
    assert!(
        store
            .latest_world_revision(mock, to)
            .await
            .unwrap()
            .is_none()
    );
    let carried = carry_forward(store.as_ref(), mock, from, to, &b2)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(carried.version, to);
    assert_eq!(carried.revision, 1);
    assert_eq!(carried.author, Author::Agent);
    // The whole overlay is carried, the stale entry with it, so that the
    // new version reports it instead of losing it.
    assert_eq!(
        carried.document,
        json!({
            "version": 1,
            "types": {t.clone(): {"unique": [{"members": ["color"]}]}},
            "operations": {
                "getWidget": {"not_found": 404},
                "listWidgets": {"order": {"member": "name"}},
            },
        })
    );
    let eff = effective_world(store.as_ref(), mock, to, &b2)
        .await
        .unwrap();
    assert_eq!(eff.revision, 1);
    // The valid entries apply; the stale one is refused and listed as a gap.
    assert_eq!(eff.world.operations["getWidget"].not_found, Some(404));
    assert!(eff.world.operations["listWidgets"].order.is_some());
    assert_eq!(eff.refused.len(), 1, "{:?}", eff.refused);
    assert!(
        eff.refused[0]
            .path
            .starts_with(&format!("/types/{t}/unique")),
        "{:?}",
        eff.refused
    );
    assert_eq!(eff.refused[0].code, RefusalCode::Unresolved);
    assert!(
        eff.gaps.iter().any(|g| g.code == GapCode::Refused),
        "{:?}",
        eff.gaps
    );
    assert!(!eff.authors.contains_key(&format!("/types/{t}/unique")));

    // A version with no revision carries nothing.
    let (next, b3) = add_version(store.as_ref(), mock, &v2()).await;
    let (other, _) = add_version(store.as_ref(), mock, &v2()).await;
    assert!(
        carry_forward(store.as_ref(), mock, other, next, &b3)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn carry_forward_of_only_stale_entries_still_reports_them() {
    let (store, mock) = store_with_mock().await;
    let (from, b1) = add_version(store.as_ref(), mock, V1).await;
    let t = widget_type(&b1);
    revise(
        store.as_ref(),
        mock,
        from,
        Author::User,
        json!({"version": 1, "types": {t.clone(): {"unique": [{"members": ["color"]}]}}}),
    )
    .await;
    let (to, b2) = add_version(store.as_ref(), mock, &v2()).await;
    let carried = carry_forward(store.as_ref(), mock, from, to, &b2)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(carried.author, Author::User);
    let eff = effective_world(store.as_ref(), mock, to, &b2)
        .await
        .unwrap();
    assert_eq!(eff.revision, 1);
    assert_eq!(eff.refused.len(), 1, "{:?}", eff.refused);
    assert_eq!(eff.refused[0].code, RefusalCode::Unresolved);
    assert!(
        eff.gaps.iter().any(|g| g.code == GapCode::Refused),
        "{:?}",
        eff.gaps
    );
    assert!(eff.authors.is_empty(), "{:?}", eff.authors);
    assert_eq!(eff.world, tier0(&b2));
}

/// `Bearer editor` → an editor in `default`.
struct TestAuth;

#[async_trait]
impl Authenticate for TestAuth {
    async fn authenticate(&self, headers: &HeaderMap) -> Result<Option<Principal>, AuthError> {
        if headers.get(header::AUTHORIZATION).is_none() {
            return Ok(None);
        }
        Ok(Some(Principal {
            subject: "editor".into(),
            display_name: "editor".into(),
            kind: PrincipalKind::User,
            sysadmin: false,
            roles: [(DEFAULT_WORKSPACE.to_string(), Role::Editor)]
                .into_iter()
                .collect(),
        }))
    }
}

const BASE: &str = "/api/v1/workspaces/default/mocks";

async fn send(app: &Router, method: Method, uri: &str, ctype: &str, body: Vec<u8>) -> Value {
    let req = Request::builder()
        .method(method.clone())
        .uri(uri)
        .header(header::AUTHORIZATION, "Bearer editor")
        .header(header::CONTENT_TYPE, ctype)
        .header(UPLOAD_HEADER, "1")
        .body(Body::from(body))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(status.is_success(), "{method} {uri}: {status} {text}");
    serde_json::from_str(&text).unwrap()
}

async fn admin() -> (AdminState, Router) {
    let store = mb_store::connect("sqlite::memory:").await.unwrap();
    let state: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let st = AdminState {
        auth: Arc::new(TestAuth),
        ..AdminState::new(store, state)
    };
    let app = router(st.clone());
    send(
        &app,
        Method::POST,
        BASE,
        "application/json",
        json!({"name": "widgets"}).to_string().into_bytes(),
    )
    .await;
    (st, app)
}

#[tokio::test]
async fn carry_forward_runs_on_import_of_a_new_version() {
    let (st, app) = admin().await;
    let uri = format!("{BASE}/widgets/contracts");
    let first = send(
        &app,
        Method::POST,
        &uri,
        "application/yaml",
        V1.as_bytes().to_vec(),
    )
    .await;
    let from = first["version"].as_i64().unwrap();
    let mock = st
        .store
        .get_mock(DEFAULT_WORKSPACE, "widgets")
        .await
        .unwrap()
        .unwrap();
    let t = widget_type(&compile(V1));
    let described = json!({
        "version": 1,
        "types": {t.clone(): {"unique": [{"members": ["color"]}]}},
        "operations": {"getWidget": {"not_found": 404}},
    });
    revise(
        st.store.as_ref(),
        mock.id,
        from,
        Author::User,
        described.clone(),
    )
    .await;

    let second = send(
        &app,
        Method::POST,
        &uri,
        "application/yaml",
        v2().into_bytes(),
    )
    .await;
    let to = second["version"].as_i64().unwrap();
    assert_eq!(to, from + 1);
    let carried = st
        .store
        .latest_world_revision(mock.id, to)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(carried.revision, 1);
    assert_eq!(carried.author, Author::User);
    assert_eq!(carried.document, described);
    // The second version has no `color`: its unique entry is refused.
    let eff = effective_world(st.store.as_ref(), mock.id, to, &compile(&v2()))
        .await
        .unwrap();
    assert_eq!(eff.refused.len(), 1, "{:?}", eff.refused);
    assert_eq!(eff.refused[0].code, RefusalCode::Unresolved);

    // A third upload carries the second version's description on, skipping
    // nothing: the newest earlier version with a revision is the source.
    // It has `color` again, so the unique entry applies again.
    let third = send(
        &app,
        Method::POST,
        &uri,
        "application/yaml",
        V1.as_bytes().to_vec(),
    )
    .await;
    let third = third["version"].as_i64().unwrap();
    let carried = st
        .store
        .latest_world_revision(mock.id, third)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(carried.document, described);
    let eff = effective_world(st.store.as_ref(), mock.id, third, &compile(V1))
        .await
        .unwrap();
    assert!(eff.refused.is_empty(), "{:?}", eff.refused);
    assert_eq!(eff.authors[&format!("/types/{t}/unique")], Author::User);
}

/// One `file` part of a multipart body, and its content type.
fn multipart(filename: &str, bytes: &[u8]) -> (String, Vec<u8>) {
    let boundary = "mbboundary7MA4YWxkTrZu0gW";
    let mut body = Vec::new();
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\n\
             Content-Type: application/octet-stream\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

#[tokio::test]
async fn carry_forward_runs_on_an_import_job() {
    let (st, app) = admin().await;
    let har = std::fs::read(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/smart/petstore.har"),
    )
    .unwrap();
    let uri = format!("{BASE}/widgets/imports");
    let (ctype, body) = multipart("petstore.har", &har);
    send(&app, Method::POST, &uri, &ctype, body.clone()).await;
    assert!(run_next_import(&st).await.unwrap());
    let mock = st
        .store
        .get_mock(DEFAULT_WORKSPACE, "widgets")
        .await
        .unwrap()
        .unwrap();
    let first = st
        .store
        .latest_contract_version(mock.id, &[ContractStatus::Draft])
        .await
        .unwrap()
        .unwrap();
    // An entry tier 0 already holds always validates on the merged draft.
    let base = tier0(&first.bundle);
    let (op, kind) = base
        .operations
        .iter()
        .find_map(|(id, d)| d.kind.map(|k| (id.clone(), k)))
        .unwrap();
    let doc = json!({"version": 1, "operations": {op.clone(): {"kind": kind}}});
    revise(
        st.store.as_ref(),
        mock.id,
        first.version,
        Author::Agent,
        doc.clone(),
    )
    .await;

    send(&app, Method::POST, &uri, &ctype, body).await;
    assert!(run_next_import(&st).await.unwrap());
    let second = st
        .store
        .latest_contract_version(mock.id, &[ContractStatus::Draft])
        .await
        .unwrap()
        .unwrap();
    assert!(second.version > first.version);
    // Nothing is carried into a draft: it is carried when it is approved.
    assert!(
        st.store
            .latest_world_revision(mock.id, second.version)
            .await
            .unwrap()
            .is_none()
    );
    approve(&app, second.version).await;
    let carried = st
        .store
        .latest_world_revision(mock.id, second.version)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(carried.revision, 1);
    assert_eq!(carried.author, Author::Agent);
    assert_eq!(carried.document, doc);
}

async fn approve(app: &Router, version: i64) -> Value {
    send(
        app,
        Method::POST,
        &format!("{BASE}/widgets/contracts/{version}/approve"),
        "application/json",
        Vec::new(),
    )
    .await
}

async fn widgets_mock(st: &AdminState) -> Uuid {
    st.store
        .get_mock(DEFAULT_WORKSPACE, "widgets")
        .await
        .unwrap()
        .unwrap()
        .id
}

#[tokio::test]
async fn carry_forward_keeps_each_entrys_author() {
    let (store, mock) = store_with_mock().await;
    let (from, _) = add_version(store.as_ref(), mock, V1).await;
    // A person sets `not_found`; an assistant keeps it and adds the order.
    revise(
        store.as_ref(),
        mock,
        from,
        Author::User,
        json!({"version": 1, "operations": {"getWidget": {"not_found": 404}}}),
    )
    .await;
    revise(
        store.as_ref(),
        mock,
        from,
        Author::Agent,
        json!({
            "version": 1,
            "operations": {
                "getWidget": {"not_found": 404},
                "listWidgets": {"order": {"member": "name"}},
            },
        }),
    )
    .await;
    let (to, b2) = add_version(store.as_ref(), mock, V1).await;
    carry_forward(store.as_ref(), mock, from, to, &b2)
        .await
        .unwrap()
        .unwrap();
    let eff = effective_world(store.as_ref(), mock, to, &b2)
        .await
        .unwrap();
    let expected: BTreeMap<String, Author> = [
        ("/operations/getWidget/not_found", Author::User),
        ("/operations/listWidgets/order", Author::Agent),
    ]
    .into_iter()
    .map(|(k, a)| (k.to_string(), a))
    .collect();
    assert_eq!(eff.authors, expected);
    assert_eq!(eff.world.operations["getWidget"].not_found, Some(404));
    assert!(eff.world.operations["listWidgets"].order.is_some());

    // Carried on again, the authors still hold.
    let (next, b3) = add_version(store.as_ref(), mock, V1).await;
    carry_forward(store.as_ref(), mock, to, next, &b3)
        .await
        .unwrap()
        .unwrap();
    let eff = effective_world(store.as_ref(), mock, next, &b3)
        .await
        .unwrap();
    assert_eq!(eff.authors, expected);
}

#[tokio::test]
async fn carry_forward_runs_when_a_draft_is_approved_after_an_edit() {
    let (st, app) = admin().await;
    let uri = format!("{BASE}/widgets/contracts");
    let first = send(
        &app,
        Method::POST,
        &uri,
        "application/yaml",
        V1.as_bytes().to_vec(),
    )
    .await;
    let from = first["version"].as_i64().unwrap();
    let mock = widgets_mock(&st).await;
    let t = widget_type(&compile(V1));
    revise(
        st.store.as_ref(),
        mock,
        from,
        Author::User,
        json!({"version": 1, "types": {t.clone(): {"unique": [{"members": ["color"]}]}}}),
    )
    .await;
    // A draft without `color`, as an import may produce.
    let meta = mb_import::smart::run_import(
        mb_import::smart::ImportInput::Bytes {
            kind: mb_import::smart::ImportKind::Har,
            name: Some("petstore.har".into()),
            bytes: std::fs::read(
                std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("../../fixtures/smart/petstore.har"),
            )
            .unwrap(),
        },
        None,
        &mb_import::smart::ImportOptions::default(),
        &mb_import::smart::NoopObserver,
    )
    .await
    .unwrap()
    .meta;
    let source = v2();
    let draft = st
        .store
        .add_draft_contract_version(
            mock,
            NewContractVersion {
                source: source.clone().into_bytes(),
                bundle: compile(&source),
                status: ContractStatus::Draft,
                created_by: Some("editor".into()),
            },
            serde_json::to_value(&meta).unwrap(),
        )
        .await
        .unwrap()
        .version;
    // The draft is edited to put `color` back, then approved.
    let document = mb_import::parse_document(V1.as_bytes()).unwrap();
    let req = Request::builder()
        .method(Method::PUT)
        .uri(format!("{uri}/{draft}/draft"))
        .header(header::AUTHORIZATION, "Bearer editor")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(json!({"document": document}).to_string()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert!(resp.status().is_success(), "{}", resp.status());
    approve(&app, draft).await;
    let approved = st
        .store
        .get_contract_version(mock, draft)
        .await
        .unwrap()
        .unwrap();
    let eff = effective_world(st.store.as_ref(), mock, draft, &approved.bundle)
        .await
        .unwrap();
    assert_eq!(eff.revision, 1);
    assert_eq!(eff.authors[&format!("/types/{t}/unique")], Author::User);
}

#[tokio::test]
async fn carry_forward_on_approve_takes_the_source_as_revised_since_the_draft() {
    let (st, app) = admin().await;
    let uri = format!("{BASE}/widgets/contracts");
    let first = send(
        &app,
        Method::POST,
        &uri,
        "application/yaml",
        V1.as_bytes().to_vec(),
    )
    .await;
    let from = first["version"].as_i64().unwrap();
    let mock = widgets_mock(&st).await;
    revise(
        st.store.as_ref(),
        mock,
        from,
        Author::User,
        json!({"version": 1, "operations": {"getWidget": {"not_found": 404}}}),
    )
    .await;
    // An import job merges a HAR into the active version as a draft.
    let har = std::fs::read(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/smart/petstore.har"),
    )
    .unwrap();
    let (ctype, body) = multipart("petstore.har", &har);
    send(
        &app,
        Method::POST,
        &format!("{BASE}/widgets/imports"),
        &ctype,
        body,
    )
    .await;
    assert!(run_next_import(&st).await.unwrap());
    let draft = st
        .store
        .latest_contract_version(mock, &[ContractStatus::Draft])
        .await
        .unwrap()
        .unwrap();
    // The source is revised after the draft was made.
    revise(
        st.store.as_ref(),
        mock,
        from,
        Author::Agent,
        json!({
            "version": 1,
            "operations": {
                "getWidget": {"not_found": 404},
                "listWidgets": {"order": {"member": "name"}},
            },
        }),
    )
    .await;
    approve(&app, draft.version).await;
    let eff = effective_world(st.store.as_ref(), mock, draft.version, &draft.bundle)
        .await
        .unwrap();
    assert!(eff.refused.is_empty(), "{:?}", eff.refused);
    let expected: BTreeMap<String, Author> = [
        ("/operations/getWidget/not_found", Author::User),
        ("/operations/listWidgets/order", Author::Agent),
    ]
    .into_iter()
    .map(|(k, a)| (k.to_string(), a))
    .collect();
    assert_eq!(eff.authors, expected);
}

#[tokio::test]
async fn carry_forward_prefers_a_version_that_is_not_a_draft() {
    let (st, app) = admin().await;
    let uri = format!("{BASE}/widgets/contracts");
    let first = send(
        &app,
        Method::POST,
        &uri,
        "application/yaml",
        V1.as_bytes().to_vec(),
    )
    .await;
    let from = first["version"].as_i64().unwrap();
    let mock = widgets_mock(&st).await;
    let active_doc = json!({"version": 1, "operations": {"getWidget": {"not_found": 404}}});
    revise(
        st.store.as_ref(),
        mock,
        from,
        Author::User,
        active_doc.clone(),
    )
    .await;
    // An abandoned draft with a description of its own.
    let abandoned = st
        .store
        .add_draft_contract_version(
            mock,
            NewContractVersion {
                source: V1.as_bytes().to_vec(),
                bundle: compile(V1),
                status: ContractStatus::Draft,
                created_by: None,
            },
            json!({}),
        )
        .await
        .unwrap()
        .version;
    revise(
        st.store.as_ref(),
        mock,
        abandoned,
        Author::Agent,
        json!({"version": 1, "operations": {"listWidgets": {"order": {"member": "name"}}}}),
    )
    .await;
    let next = send(
        &app,
        Method::POST,
        &uri,
        "application/yaml",
        V1.as_bytes().to_vec(),
    )
    .await;
    let carried = st
        .store
        .latest_world_revision(mock, next["version"].as_i64().unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(carried.document, active_doc);
    assert_eq!(carried.author, Author::User);
}

#[tokio::test]
async fn carry_forward_on_approve_takes_a_later_version_revised_since_the_draft() {
    let (st, app) = admin().await;
    let uri = format!("{BASE}/widgets/contracts");
    let first = send(
        &app,
        Method::POST,
        &uri,
        "application/yaml",
        V1.as_bytes().to_vec(),
    )
    .await;
    let from = first["version"].as_i64().unwrap();
    let mock = widgets_mock(&st).await;
    revise(
        st.store.as_ref(),
        mock,
        from,
        Author::User,
        json!({"version": 1, "operations": {"getWidget": {"not_found": 404}}}),
    )
    .await;
    let draft = st
        .store
        .add_draft_contract_version(
            mock,
            NewContractVersion {
                source: V1.as_bytes().to_vec(),
                bundle: compile(V1),
                status: ContractStatus::Draft,
                created_by: None,
            },
            json!({}),
        )
        .await
        .unwrap();
    // A later upload carries from the first version and is then revised.
    let later = send(
        &app,
        Method::POST,
        &uri,
        "application/yaml",
        V1.as_bytes().to_vec(),
    )
    .await["version"]
        .as_i64()
        .unwrap();
    assert!(later > draft.version);
    let newest = json!({
        "version": 1,
        "operations": {
            "getWidget": {"not_found": 404},
            "listWidgets": {"order": {"member": "name"}},
        },
    });
    revise(
        st.store.as_ref(),
        mock,
        later,
        Author::Agent,
        newest.clone(),
    )
    .await;
    approve(&app, draft.version).await;
    let eff = effective_world(st.store.as_ref(), mock, draft.version, &draft.bundle)
        .await
        .unwrap();
    assert!(eff.world.operations["listWidgets"].order.is_some());
    assert_eq!(
        eff.authors.get("/operations/listWidgets/order"),
        Some(&Author::Agent)
    );
    assert_eq!(
        eff.authors.get("/operations/getWidget/not_found"),
        Some(&Author::User)
    );
}

/// [`V1`] with a second route that matches the same requests as
/// `/widgets/{widgetId}`, so it is skipped and reported as a gap.
fn with_skipped_route() -> String {
    V1.replace(
        "components:\n",
        r#"  /widgets/{other}:
    get:
      operationId: getWidgetAgain
      parameters: [{name: other, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok}
components:
"#,
    )
}

/// A GET answering with any status: the status, content type and JSON body.
async fn get_any(app: &Router, uri: &str) -> (u16, String, Value) {
    let req = Request::builder()
        .uri(uri)
        .header(header::AUTHORIZATION, "Bearer editor")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status().as_u16();
    let ctype = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, ctype, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn get_world_returns_tier0_and_gaps() {
    let (_st, app) = admin().await;
    let src = with_skipped_route();
    let uploaded = send(
        &app,
        Method::POST,
        &format!("{BASE}/widgets/contracts"),
        "application/yaml",
        src.clone().into_bytes(),
    )
    .await;
    let version = uploaded["version"].as_i64().unwrap();
    let bundle = compile(&src);

    let view = send(
        &app,
        Method::GET,
        &format!("{BASE}/widgets/contracts/{version}/world"),
        "application/json",
        Vec::new(),
    )
    .await;
    assert_eq!(view["revision"], 0, "{view}");
    assert_eq!(view["version"], version, "{view}");
    assert_eq!(view["world"], serde_json::to_value(tier0(&bundle)).unwrap());
    assert_eq!(view["authors"], json!({}));
    assert_eq!(view["refused"], json!([]));
    let gaps = view["gaps"].as_array().unwrap();
    assert!(gaps.iter().any(|g| g["code"] == "skipped_route"), "{view}");

    // `active` names the active version.
    let active = send(
        &app,
        Method::GET,
        &format!("{BASE}/widgets/contracts/active/world"),
        "application/json",
        Vec::new(),
    )
    .await;
    assert_eq!(active, view);

    // A missing version or mock is a 404 problem.
    for uri in [
        format!("{BASE}/widgets/contracts/99/world"),
        format!("{BASE}/nope/contracts/1/world"),
        format!("{BASE}/nope/contracts/active/world"),
    ] {
        let (status, ctype, body) = get_any(&app, &uri).await;
        assert_eq!(status, 404, "{uri}: {body}");
        assert_eq!(ctype, "application/problem+json", "{uri}");
    }
    // A version that is neither a number nor `active` is a 400 problem.
    let (status, ctype, body) =
        get_any(&app, &format!("{BASE}/widgets/contracts/latest/world")).await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(ctype, "application/problem+json");
}

#[tokio::test]
async fn get_world_on_a_mock_without_an_active_version_is_404() {
    let (_st, app) = admin().await;
    let (status, ctype, body) =
        get_any(&app, &format!("{BASE}/widgets/contracts/active/world")).await;
    assert_eq!(status, 404, "{body}");
    assert_eq!(ctype, "application/problem+json");
}

#[tokio::test]
async fn get_world_on_pre_existing_version_is_revision_0() {
    // A version stored before world descriptions existed has no revision.
    let (st, app) = admin().await;
    let mock = widgets_mock(&st).await;
    let (version, bundle) = add_version(st.store.as_ref(), mock, V1).await;
    let uri = format!("{BASE}/widgets/contracts/{version}/world");
    let view = send(&app, Method::GET, &uri, "application/json", Vec::new()).await;
    assert_eq!(view["revision"], 0, "{view}");
    assert_eq!(view["world"], serde_json::to_value(tier0(&bundle)).unwrap());
    assert_eq!(view["refused"], json!([]));

    // The first write is revision 1.
    let first = st
        .store
        .add_world_revision(
            mock,
            version,
            NewWorldRevision {
                author: Author::User,
                document: json!({"version": 1, "operations": {"getWidget": {"not_found": 404}}}),
                created_by: None,
                expect: Some(0),
            },
        )
        .await
        .unwrap();
    assert_eq!(first.revision, 1);
    let view = send(&app, Method::GET, &uri, "application/json", Vec::new()).await;
    assert_eq!(view["revision"], 1, "{view}");
    assert_eq!(view["world"]["operations"]["getWidget"]["not_found"], 404);
    assert_eq!(
        view["authors"],
        json!({"/operations/getWidget/not_found": "user"})
    );
}

/// Any request: the status, content type and JSON body (`null` when empty).
async fn call(
    app: &Router,
    method: Method,
    uri: &str,
    ctype: &str,
    headers: &[(&str, &str)],
    body: Vec<u8>,
) -> (u16, String, Value) {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, "Bearer editor")
        .header(header::CONTENT_TYPE, ctype);
    for (name, value) in headers {
        req = req.header(*name, *value);
    }
    let resp = app
        .clone()
        .oneshot(req.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let ctype = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|e| panic!("{status}: {e}: {}", String::from_utf8_lossy(&bytes)))
    };
    (status, ctype, body)
}

/// An admin with `widgets` at contract version 1 ([`V1`]): the router, the
/// world URI of version 1, and the widget's resource type.
async fn widgets_v1() -> (AdminState, Router, String, String) {
    let (st, app) = admin().await;
    let uploaded = send(
        &app,
        Method::POST,
        &format!("{BASE}/widgets/contracts"),
        "application/yaml",
        V1.as_bytes().to_vec(),
    )
    .await;
    let version = uploaded["version"].as_i64().unwrap();
    let t = widget_type(&compile(V1));
    (
        st,
        app,
        format!("{BASE}/widgets/contracts/{version}/world"),
        t,
    )
}

async fn put_world(app: &Router, uri: &str, doc: &Value) -> Value {
    let (status, _, body) = call(
        app,
        Method::PUT,
        uri,
        "application/json",
        &[],
        doc.to_string().into_bytes(),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    body
}

async fn patch_world(
    app: &Router,
    uri: &str,
    headers: &[(&str, &str)],
    patch: &Value,
) -> (u16, String, Value) {
    call(
        app,
        Method::PATCH,
        uri,
        "application/merge-patch+json",
        headers,
        patch.to_string().into_bytes(),
    )
    .await
}

/// The root pointer `""` says a create's answer is itself the record: the
/// write stores it, refusing nothing, and the effective view shows it.
#[tokio::test]
async fn put_world_accepts_a_root_answer_record() {
    let (_st, app, uri, _t) = widgets_v1().await;
    let doc = json!({"operations": {"createWidget": {"answer": {"record": ""}}}});
    let result = put_world(&app, &uri, &doc).await;
    assert_eq!(result["refused"], json!([]), "{result}");
    assert_eq!(
        result["applied"]["operations"]["createWidget"]["answer"]["record"], "",
        "{result}"
    );
    let (status, _, view) =
        call(&app, Method::GET, &uri, "application/json", &[], Vec::new()).await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["refused"], json!([]), "{view}");
    assert_eq!(
        view["world"]["operations"]["createWidget"]["answer"]["record"], "",
        "{view}"
    );
    // The same pointer on a list is no record pointer: still refused.
    let (status, ctype, body) = call(
        &app,
        Method::PUT,
        &uri,
        "application/json",
        &[],
        json!({"operations": {"createWidget": {"answer": {"records": ""}}}})
            .to_string()
            .into_bytes(),
    )
    .await;
    assert_eq!(status, 422, "{body}");
    assert_eq!(ctype, "application/problem+json");
    assert_eq!(
        body["path"], "/operations/createWidget/answer/records",
        "{body}"
    );
}

#[tokio::test]
async fn put_world_stores_applied_only_and_reports_refused() {
    let (_st, app, uri, t) = widgets_v1().await;
    let result = put_world(
        &app,
        &uri,
        &json!({
            "version": 1,
            "types": {t.clone(): {"identity": {"member": "nope"}}},
            "operations": {
                "getWidget": {"not_found": 404, "bogus": true},
                "createWidget": {"answer": {"record": "/../"}},
                "Nope": {"kind": "read"},
            },
        }),
    )
    .await;
    assert_eq!(result["revision"], 1, "{result}");
    assert_eq!(
        result["applied"],
        json!({"version": 1, "operations": {"getWidget": {"not_found": 404}}}),
        "{result}"
    );
    let refused: BTreeMap<String, String> = result["refused"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["path"].as_str().unwrap().to_string(),
                r["code"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    let escaped = t.replace('~', "~0").replace('/', "~1");
    assert_eq!(
        refused
            .get(&format!("/types/{escaped}/identity/member"))
            .map(String::as_str),
        Some("unresolved"),
        "{refused:?}"
    );
    assert_eq!(
        refused
            .get("/operations/getWidget/bogus")
            .map(String::as_str),
        Some("unknown_key"),
        "{refused:?}"
    );
    assert_eq!(
        refused
            .get("/operations/createWidget/answer/record")
            .map(String::as_str),
        Some("unresolved"),
        "{refused:?}"
    );
    assert_eq!(
        refused.get("/operations/Nope").map(String::as_str),
        Some("unresolved"),
        "{refused:?}"
    );

    // Only the applied entries are stored.
    let (status, _, stored) = call(
        &app,
        Method::GET,
        &format!("{uri}/revisions/1"),
        "application/json",
        &[],
        Vec::new(),
    )
    .await;
    assert_eq!(status, 200, "{stored}");
    assert_eq!(stored["document"], result["applied"]);
    assert_eq!(stored["author"], "user");
    assert_eq!(stored["revision"], 1);

    let view = send(&app, Method::GET, &uri, "application/json", Vec::new()).await;
    assert_eq!(view["revision"], 1);
    assert_eq!(view["refused"], json!([]), "{view}");
    assert_eq!(
        view["authors"],
        json!({"/operations/getWidget/not_found": "user"})
    );

    // A PUT replaces the overlay whole, and `author` names who wrote it.
    let (status, _, result) = call(
        &app,
        Method::PUT,
        &format!("{uri}?author=agent"),
        "application/json",
        &[],
        json!({"operations": {"listWidgets": {"order": {"member": "name"}}}})
            .to_string()
            .into_bytes(),
    )
    .await;
    assert_eq!(status, 201, "{result}");
    assert_eq!(result["revision"], 2);
    let view = send(&app, Method::GET, &uri, "application/json", Vec::new()).await;
    assert_eq!(
        view["authors"],
        json!({"/operations/listWidgets/order": "agent"}),
        "{view}"
    );
    assert!(
        view["world"]["operations"]["getWidget"]
            .get("not_found")
            .is_none(),
        "{view}"
    );

    // A body that is not JSON, or not sent as JSON, changes nothing.
    let (status, ctype, body) = call(
        &app,
        Method::PUT,
        &uri,
        "application/json",
        &[],
        b"{".to_vec(),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(ctype, "application/problem+json");
    let (status, ctype, _) = call(&app, Method::PUT, &uri, "text/plain", &[], b"{}".to_vec()).await;
    assert_eq!(status, 415);
    assert_eq!(ctype, "application/problem+json");
    let (status, _, list) = call(
        &app,
        Method::GET,
        &format!("{uri}/revisions"),
        "application/json",
        &[],
        Vec::new(),
    )
    .await;
    assert_eq!(status, 200, "{list}");
    let revisions: Vec<i64> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["revision"].as_i64().unwrap())
        .collect();
    assert_eq!(revisions, vec![2, 1], "newest first: {list}");
    assert_eq!(list[0]["author"], "agent");
    assert!(list[0].get("document").is_none(), "{list}");
}

#[tokio::test]
async fn patch_world_merges_and_bumps_revision() {
    let (_st, app, uri, _t) = widgets_v1().await;
    // A patch on a version with no revision merges into an empty overlay.
    let (status, _, first) = patch_world(
        &app,
        &uri,
        &[],
        &json!({"operations": {"getWidget": {"not_found": 404}}}),
    )
    .await;
    assert_eq!(status, 201, "{first}");
    assert_eq!(first["revision"], 1);
    let (status, _, second) = patch_world(
        &app,
        &format!("{uri}?author=agent"),
        &[],
        &json!({"operations": {"listWidgets": {"order": {"member": "name"}}}}),
    )
    .await;
    assert_eq!(status, 201, "{second}");
    assert_eq!(second["revision"], 2);
    assert_eq!(
        second["applied"]["operations"],
        json!({"getWidget": {"not_found": 404}, "listWidgets": {"order": {"member": "name"}}}),
        "{second}"
    );
    let view = send(&app, Method::GET, &uri, "application/json", Vec::new()).await;
    assert_eq!(
        view["authors"],
        json!({"/operations/getWidget/not_found": "user", "/operations/listWidgets/order": "agent"}),
        "{view}"
    );

    // `null` removes an entry, as JSON merge patch does.
    let (status, _, third) =
        patch_world(&app, &uri, &[], &json!({"operations": {"getWidget": null}})).await;
    assert_eq!(status, 201, "{third}");
    assert_eq!(third["revision"], 3);
    assert_eq!(
        third["applied"]["operations"],
        json!({"listWidgets": {"order": {"member": "name"}}})
    );

    // `application/json` is taken as a merge patch too; other types are not.
    let (status, _, fourth) = call(
        &app,
        Method::PATCH,
        &uri,
        "application/json",
        &[],
        json!({"operations": {"getWidget": {"not_found": 404}}})
            .to_string()
            .into_bytes(),
    )
    .await;
    assert_eq!(status, 201, "{fourth}");
    assert_eq!(fourth["revision"], 4);
    let (status, ctype, _) =
        call(&app, Method::PATCH, &uri, "text/plain", &[], b"{}".to_vec()).await;
    assert_eq!(status, 415);
    assert_eq!(ctype, "application/problem+json");
}

#[tokio::test]
async fn stale_if_match_is_412() {
    let (_st, app, uri, _t) = widgets_v1().await;
    for _ in 0..3 {
        put_world(
            &app,
            &uri,
            &json!({"operations": {"getWidget": {"not_found": 404}}}),
        )
        .await;
    }
    // An assistant and a person both patch from revision 3.
    let (status, _, agent) = patch_world(
        &app,
        &format!("{uri}?author=agent"),
        &[("if-match", "3")],
        &json!({"operations": {"listWidgets": {"order": {"member": "name"}}}}),
    )
    .await;
    assert_eq!(status, 201, "{agent}");
    assert_eq!(agent["revision"], 4);
    let late = json!({"operations": {"listWidgets": {"order": {"member": "id"}}}});
    let (status, ctype, problem) = patch_world(&app, &uri, &[("if-match", "\"3\"")], &late).await;
    assert_eq!(status, 412, "{problem}");
    assert_eq!(ctype, "application/problem+json");
    assert_eq!(problem["code"], "revision_conflict", "{problem}");
    assert_eq!(problem["status"], 412);
    // A PUT with a stale If-Match is refused the same way.
    let (status, _, problem) = call(
        &app,
        Method::PUT,
        &uri,
        "application/json",
        &[("if-match", "3")],
        late.to_string().into_bytes(),
    )
    .await;
    assert_eq!(status, 412, "{problem}");
    assert_eq!(problem["code"], "revision_conflict");
    // Without If-Match, the patch applies on top of revision 4.
    let (status, _, person) = patch_world(&app, &uri, &[], &json!({"operations": {"getWidget": {"not_found": null}, "listWidgets": {"order": {"descending": true}}}})).await;
    assert_eq!(status, 201, "{person}");
    assert_eq!(person["revision"], 5);
    assert_eq!(
        person["applied"]["operations"],
        json!({"listWidgets": {"order": {"member": "name", "descending": true}}}),
        "{person}"
    );
    // A current If-Match applies; one that is not a revision number is a 400.
    let (status, _, body) = patch_world(&app, &uri, &[("if-match", "5")], &json!({})).await;
    assert_eq!(status, 201, "{body}");
    assert_eq!(body["revision"], 6);
    let (status, ctype, body) =
        patch_world(&app, &uri, &[("if-match", "latest")], &json!({})).await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(ctype, "application/problem+json");
    // If-Match 0 means "no revision yet".
    let (_st, app, uri, _t) = widgets_v1().await;
    let (status, _, body) = patch_world(&app, &uri, &[("if-match", "0")], &json!({})).await;
    assert_eq!(status, 201, "{body}");
    let (status, _, body) = patch_world(&app, &uri, &[("if-match", "0")], &json!({})).await;
    assert_eq!(status, 412, "{body}");
}

#[tokio::test]
async fn put_over_256k_is_413() {
    let (_st, app, uri, _t) = widgets_v1().await;
    let padded = |len: usize| {
        let mut doc = String::from(r#"{"version":1,"pad":""#);
        let fill = len - doc.len() - 2;
        doc.push_str(&"a".repeat(fill));
        doc.push_str("\"}");
        assert_eq!(doc.len(), len);
        doc.into_bytes()
    };
    let max = mb_mcm::world::MAX_BYTES;
    for len in [max + 1, 5 * 1024 * 1024] {
        let (status, ctype, problem) = call(
            &app,
            Method::PUT,
            &uri,
            "application/json",
            &[],
            padded(len),
        )
        .await;
        assert_eq!(status, 413, "{len}: {problem}");
        assert_eq!(ctype, "application/problem+json");
        assert_eq!(problem["code"], "too_large", "{problem}");
        let (status, _, problem) = patch_world(
            &app,
            &uri,
            &[],
            &serde_json::from_slice(&padded(len)).unwrap(),
        )
        .await;
        assert_eq!(status, 413, "{len}: {problem}");
    }
    // At the limit it is read (and the unknown key refused).
    let (status, _, result) = call(
        &app,
        Method::PUT,
        &uri,
        "application/json",
        &[],
        padded(max),
    )
    .await;
    assert_eq!(status, 201, "{result}");
    assert_eq!(result["refused"][0]["path"], "/pad");
    assert_eq!(result["refused"][0]["code"], "unknown_key");

    // Too deep is a 422 naming the path.
    let mut deep = json!(1);
    for _ in 0..33 {
        deep = json!([deep]);
    }
    let (status, ctype, problem) = call(
        &app,
        Method::PUT,
        &uri,
        "application/json",
        &[],
        json!({"x": deep}).to_string().into_bytes(),
    )
    .await;
    assert_eq!(status, 422, "{problem}");
    assert_eq!(ctype, "application/problem+json");
    assert_eq!(problem["code"], "invalid");
    assert!(
        problem["path"].as_str().unwrap().starts_with("/x/0/0"),
        "{problem}"
    );

    // A pointer that does not parse is a 422 naming it.
    let (status, _, problem) = call(
        &app,
        Method::PUT,
        &uri,
        "application/json",
        &[],
        json!({"operations": {"createWidget": {"answer": {"record": "result"}}}})
            .to_string()
            .into_bytes(),
    )
    .await;
    assert_eq!(status, 422, "{problem}");
    assert_eq!(
        problem["path"], "/operations/createWidget/answer/record",
        "{problem}"
    );

    // Entries that are not of the format's shape are a 422 naming them.
    for (doc, path) in [
        (
            json!({"operations": {"createWidget": {"changes": {"name": {"from_request": "/name", "bogus": 1}}}}}),
            "/operations/createWidget/changes/name/bogus",
        ),
        (
            json!({"operations": {"listWidgets": {"kind": "fetch"}}}),
            "/operations/listWidgets/kind",
        ),
        (
            json!({"operations": {"listWidgets": {"filters": [{"param": "q", "kind": "fuzzy"}]}}}),
            "/operations/listWidgets/filters/0",
        ),
        (
            json!({"types": {"widget": {"derived": {"total": {"kind": "avg", "of": "widget"}}}}}),
            "/types/widget/derived/total",
        ),
        (json!({"operations": []}), "/operations"),
        (json!({"version": "one"}), "/version"),
    ] {
        let (status, ctype, problem) = call(
            &app,
            Method::PUT,
            &uri,
            "application/json",
            &[],
            doc.to_string().into_bytes(),
        )
        .await;
        assert_eq!(status, 422, "{doc}: {problem}");
        assert_eq!(ctype, "application/problem+json");
        assert_eq!(problem["code"], "invalid", "{problem}");
        assert_eq!(problem["path"], path, "{doc}: {problem}");
    }
    // None of it was stored.
    let view = send(&app, Method::GET, &uri, "application/json", Vec::new()).await;
    assert_eq!(view["revision"], 1, "{view}");
}

#[tokio::test]
async fn restore_adds_a_new_revision() {
    let (_st, app, uri, _t) = widgets_v1().await;
    let (_, _, first) = patch_world(
        &app,
        &format!("{uri}?author=agent"),
        &[],
        &json!({"operations": {"getWidget": {"not_found": 404}}}),
    )
    .await;
    assert_eq!(first["revision"], 1);
    put_world(
        &app,
        &uri,
        &json!({"operations": {"listWidgets": {"order": {"member": "name"}}}}),
    )
    .await;

    let restore = format!("{uri}/revisions/1/restore");
    let (status, _, restored) = call(
        &app,
        Method::POST,
        &restore,
        "application/json",
        &[],
        Vec::new(),
    )
    .await;
    assert_eq!(status, 201, "{restored}");
    assert_eq!(restored["revision"], 3);
    assert_eq!(restored["applied"], first["applied"]);
    let (_, _, third) = call(
        &app,
        Method::GET,
        &format!("{uri}/revisions/3"),
        "application/json",
        &[],
        Vec::new(),
    )
    .await;
    assert_eq!(third["document"], first["applied"]);
    // It keeps the restored revision's author unless told otherwise.
    assert_eq!(third["author"], "agent");
    let view = send(&app, Method::GET, &uri, "application/json", Vec::new()).await;
    assert_eq!(view["world"]["operations"]["getWidget"]["not_found"], 404);
    assert!(
        view["world"]["operations"]["listWidgets"]
            .get("order")
            .is_none(),
        "{view}"
    );

    let (status, _, restored) = call(
        &app,
        Method::POST,
        &format!("{restore}?author=user"),
        "application/json",
        &[("if-match", "3")],
        Vec::new(),
    )
    .await;
    assert_eq!(status, 201, "{restored}");
    let (_, _, fourth) = call(
        &app,
        Method::GET,
        &format!("{uri}/revisions/4"),
        "application/json",
        &[],
        Vec::new(),
    )
    .await;
    assert_eq!(fourth["author"], "user");
    let (status, _, problem) = call(
        &app,
        Method::POST,
        &restore,
        "application/json",
        &[("if-match", "3")],
        Vec::new(),
    )
    .await;
    assert_eq!(status, 412, "{problem}");
    assert_eq!(problem["code"], "revision_conflict");

    // A missing revision is a 404; restoring needs a JSON content type.
    for path in ["revisions/9/restore", "revisions/9"] {
        let method = if path.ends_with("restore") {
            Method::POST
        } else {
            Method::GET
        };
        let (status, ctype, problem) = call(
            &app,
            method,
            &format!("{uri}/{path}"),
            "application/json",
            &[],
            Vec::new(),
        )
        .await;
        assert_eq!(status, 404, "{path}: {problem}");
        assert_eq!(ctype, "application/problem+json");
    }
    let (status, _, _) = call(&app, Method::POST, &restore, "text/plain", &[], Vec::new()).await;
    assert_eq!(status, 415);
}

#[tokio::test]
async fn diff_lists_changed_pointers() {
    let (_st, app, uri, _t) = widgets_v1().await;
    put_world(
        &app,
        &uri,
        &json!({"operations": {
            "getWidget": {"not_found": 404},
            "listWidgets": {"order": {"member": "name"}},
        }}),
    )
    .await;
    put_world(
        &app,
        &uri,
        &json!({"operations": {
            "listWidgets": {"order": {"member": "id"}, "paging": {"style": "page"}},
        }}),
    )
    .await;
    let (status, _, diff) = call(
        &app,
        Method::GET,
        &format!("{uri}/diff?from=1&to=2"),
        "application/json",
        &[],
        Vec::new(),
    )
    .await;
    assert_eq!(status, 200, "{diff}");
    assert_eq!(
        diff["added"],
        json!(["/operations/listWidgets/paging/style"]),
        "{diff}"
    );
    assert_eq!(
        diff["removed"],
        json!(["/operations/getWidget/not_found"]),
        "{diff}"
    );
    assert_eq!(
        diff["changed"],
        json!(["/operations/listWidgets/order/member"]),
        "{diff}"
    );
    // Revision 0 is the empty overlay; `to` defaults to the newest and
    // `from` to the one before it.
    let (_, _, from0) = call(
        &app,
        Method::GET,
        &format!("{uri}/diff?from=0&to=1"),
        "application/json",
        &[],
        Vec::new(),
    )
    .await;
    assert_eq!(
        from0["added"],
        json!([
            "/operations/getWidget/not_found",
            "/operations/listWidgets/order/member"
        ]),
        "{from0}"
    );
    assert_eq!(from0["removed"], json!([]));
    let (_, _, default) = call(
        &app,
        Method::GET,
        &format!("{uri}/diff"),
        "application/json",
        &[],
        Vec::new(),
    )
    .await;
    assert_eq!(default["added"], diff["added"], "{default}");
    assert_eq!(default["from"], 1);
    assert_eq!(default["to"], 2);
    let (status, ctype, problem) = call(
        &app,
        Method::GET,
        &format!("{uri}/diff?from=1&to=7"),
        "application/json",
        &[],
        Vec::new(),
    )
    .await;
    assert_eq!(status, 404, "{problem}");
    assert_eq!(ctype, "application/problem+json");
}

#[tokio::test]
async fn heuristic_author_is_refused() {
    let (_st, app, uri, _t) = widgets_v1().await;
    let doc = json!({"operations": {"getWidget": {"not_found": 404}}})
        .to_string()
        .into_bytes();
    for author in ["heuristic", "robot"] {
        let (status, ctype, problem) = call(
            &app,
            Method::PUT,
            &format!("{uri}?author={author}"),
            "application/json",
            &[],
            doc.clone(),
        )
        .await;
        assert_eq!(status, 400, "{author}: {problem}");
        assert_eq!(ctype, "application/problem+json");
        let (status, _, _) =
            patch_world(&app, &format!("{uri}?author={author}"), &[], &json!({})).await;
        assert_eq!(status, 400, "{author}");
    }
    let view = send(&app, Method::GET, &uri, "application/json", Vec::new()).await;
    assert_eq!(view["revision"], 0);
    for author in ["agent", "llm", "user"] {
        let (status, _, body) = call(
            &app,
            Method::PUT,
            &format!("{uri}?author={author}"),
            "application/json",
            &[],
            doc.clone(),
        )
        .await;
        assert_eq!(status, 201, "{author}: {body}");
    }
}

#[tokio::test]
async fn world_writes_need_credentials() {
    let (_st, app, uri, _t) = widgets_v1().await;
    let req = Request::builder()
        .method(Method::PUT)
        .uri(&uri)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status().as_u16(), 401);
}

#[tokio::test]
async fn near_limit_hostile_document_is_422_quickly() {
    let (_st, app, uri, _t) = widgets_v1().await;
    // Thousands of valid filters and a last one that is not of the shape,
    // just under the size limit: the 422 names it without trying each.
    let entry = json!({"param": "q", "kind": "eq"});
    let each = entry.to_string().len() + 1;
    let mut filters = vec![entry; (mb_mcm::world::MAX_BYTES - 256) / each];
    filters.push(json!({"param": "q", "kind": "bogus"}));
    let last = filters.len() - 1;
    let doc = json!({"operations": {"listWidgets": {"filters": filters}}});
    assert!(doc.to_string().len() <= mb_mcm::world::MAX_BYTES);
    assert!(last > 5_000, "{last}");
    let started = std::time::Instant::now();
    let (status, ctype, problem) = call(
        &app,
        Method::PUT,
        &uri,
        "application/json",
        &[],
        doc.to_string().into_bytes(),
    )
    .await;
    let took = started.elapsed();
    assert_eq!(status, 422, "{problem}");
    assert_eq!(ctype, "application/problem+json");
    assert_eq!(problem["code"], "invalid");
    assert_eq!(
        problem["path"],
        format!("/operations/listWidgets/filters/{last}"),
        "{problem}"
    );
    assert!(took < Duration::from_secs(10), "took {took:?}");
}

#[tokio::test]
async fn restore_of_a_heuristic_revision_writes_as_user() {
    let (st, app, uri, _t) = widgets_v1().await;
    let mock = widgets_mock(&st).await;
    let version: i64 = uri
        .trim_end_matches("/world")
        .rsplit('/')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    revise(
        &*st.store,
        mock,
        version,
        Author::Heuristic,
        json!({"version": 1, "operations": {"getWidget": {"not_found": 404}}}),
    )
    .await;
    let (status, _, restored) = call(
        &app,
        Method::POST,
        &format!("{uri}/revisions/1/restore"),
        "application/json",
        &[],
        Vec::new(),
    )
    .await;
    assert_eq!(status, 201, "{restored}");
    let (_, _, second) = call(
        &app,
        Method::GET,
        &format!("{uri}/revisions/2"),
        "application/json",
        &[],
        Vec::new(),
    )
    .await;
    assert_eq!(second["author"], "user", "{second}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_patches_without_if_match_both_apply() {
    for _ in 0..8 {
        let (_st, app, uri, _t) = widgets_v1().await;
        let patches = [
            json!({"operations": {"listWidgets": {"order": {"member": "name"}}}}),
            json!({"operations": {"getWidget": {"not_found": 404}}}),
        ];
        let tasks: Vec<_> = patches
            .into_iter()
            .map(|p| {
                let app = app.clone();
                let uri = uri.clone();
                tokio::spawn(async move { patch_world(&app, &uri, &[], &p).await })
            })
            .collect();
        let mut revisions = Vec::new();
        for t in tasks {
            let (status, _, body) = t.await.unwrap();
            assert_eq!(status, 201, "{body}");
            revisions.push(body["revision"].as_i64().unwrap());
        }
        revisions.sort_unstable();
        assert_eq!(revisions, [1, 2]);
        let (_, _, newest) = call(
            &app,
            Method::GET,
            &format!("{uri}/revisions/2"),
            "application/json",
            &[],
            Vec::new(),
        )
        .await;
        let ops = &newest["document"]["operations"];
        assert_eq!(ops["listWidgets"]["order"]["member"], "name", "{newest}");
        assert_eq!(ops["getWidget"]["not_found"], 404, "{newest}");
    }
}
