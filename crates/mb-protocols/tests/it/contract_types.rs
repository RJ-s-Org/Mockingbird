//! A record's identity and type where the contract's schemas give them: a path
//! parameter naming an item's member, a one-record envelope, a list typed by its
//! items, a single answer with its own identity, and a sub-route reading a member of
//! its parent. Driven over HTTP with a scripted LLM, asserting what is served, what is
//! stored and how many calls it took.
#![cfg(test)]
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use mb_engine::{Engine, EngineConfig};
use mb_import::compile;
use mb_llm::{Provider, ScriptedProvider};
use mb_protocols::{MockRuntime, UnknownIds, http_rest};
use mb_state::{CachingStateStore, MemoryStore, SessionKey, StateStore, StoredResource};
use serde_json::{Value, json};
use tower::ServiceExt;

struct Harness {
    router: axum::Router,
    provider: Arc<ScriptedProvider>,
    store: Arc<MemoryStore>,
}

fn harness_with(
    contract: &str,
    script: Vec<Value>,
    change: impl FnOnce(&mut MockRuntime),
) -> Harness {
    let script = script.into_iter().map(|v| Ok(v.to_string())).collect();
    let provider = Arc::new(ScriptedProvider::new(script));
    let store = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    let mut rt = MockRuntime::new(
        "m",
        Arc::new(compile(contract.as_bytes()).unwrap()),
        Arc::new(Engine::new(
            Some(Arc::clone(&provider) as Arc<dyn Provider>),
            EngineConfig::default(),
        )),
        CachingStateStore::wrap(
            Arc::clone(&store) as Arc<dyn StateStore>,
            Duration::from_secs(3600),
            mb_state::DEFAULT_CACHE_ENTRIES,
        ),
    );
    change(&mut rt);
    Harness {
        router: http_rest::router(Arc::new(rt)),
        provider,
        store,
    }
}

fn harness(contract: &str, script: Vec<Value>) -> Harness {
    harness_with(contract, script, |_| {})
}

fn session() -> SessionKey {
    SessionKey {
        workspace: "default".into(),
        mock: "m".into(),
        session: "default".into(),
    }
}

impl Harness {
    async fn call(
        &self,
        method: &str,
        uri: &str,
        body: Option<Value>,
    ) -> (StatusCode, String, Value) {
        let mut req = Request::builder().method(method).uri(uri);
        if body.is_some() {
            req = req.header("content-type", "application/json");
        }
        let req = req
            .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
            .unwrap();
        let resp = self.router.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let source = resp
            .headers()
            .get("x-mockingbird-source")
            .map(|v| v.to_str().unwrap().to_owned())
            .unwrap_or_default();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            source,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    fn calls(&self) -> usize {
        self.provider.requests().len()
    }

    /// The prompt of LLM call `n`, as text.
    fn prompt(&self, n: usize) -> String {
        format!("{:?}", self.provider.requests()[n])
    }

    /// What the session holds, as an export lists it.
    async fn stored(&self) -> Vec<StoredResource> {
        self.store.export(&session()).await.unwrap()
    }

    /// What an export of the session lists, as (type, id).
    async fn exported(&self) -> Vec<(String, String)> {
        let mut found: Vec<_> = self
            .stored()
            .await
            .into_iter()
            .map(|r| (r.resource_type, r.id))
            .collect();
        found.sort();
        found
    }
}

/// SendGrid's shape: `/bounces/{email}` answers an array of the bounce that address
/// has, if any.
const BOUNCES: &str = r##"
openapi: 3.0.0
info: {title: T, version: "1"}
paths:
  /bounces:
    get:
      operationId: listBounces
      parameters: [{name: email, in: query, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/Bounce"}}}}}}
  /bounces/{email}:
    parameters: [{name: email, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getBounce
      responses: {"200": {description: ok, content: {application/json: {schema: {type: array, maxItems: 1, items: {$ref: "#/components/schemas/Bounce"}}}}}}
    delete:
      operationId: deleteBounce
      responses: {"204": {description: gone}}
components:
  schemas:
    Bounce:
      type: object
      required: [email, reason]
      properties:
        created: {type: integer}
        email: {type: string}
        reason: {type: string}
"##;

fn bounce(email: &str) -> Value {
    json!({"created": 1_790_000_000, "email": email, "reason": "550 no such user"})
}

/// A bounce listed is read back by its address, as a one-item array, and once
/// deleted its address answers `[]`, all with no further call.
#[tokio::test]
async fn a_bounce_is_read_by_its_address() {
    let h = harness(BOUNCES, vec![json!([bounce("ann@example.com")])]);
    let (status, _, listed) = h.call("GET", "/bounces", None).await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    assert_eq!(
        h.exported().await,
        [("bounces".to_owned(), "ann@example.com".to_owned())]
    );

    let (status, source, read) = h.call("GET", "/bounces/ann@example.com", None).await;
    assert_eq!(
        (status, source.as_str()),
        (StatusCode::OK, "state"),
        "{read}"
    );
    assert_eq!(read, json!([bounce("ann@example.com")]));
    assert_eq!(h.calls(), 1);

    let (status, _, _) = h.call("DELETE", "/bounces/ann@example.com", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, source, gone) = h.call("GET", "/bounces/ann@example.com", None).await;
    assert_eq!(
        (status, source.as_str()),
        (StatusCode::OK, "state"),
        "{gone}"
    );
    assert_eq!(gone, json!([]));
    assert_eq!(h.calls(), 1);
    assert!(h.exported().await.is_empty());
}

/// A generated bounce for an address is that address's record from then on.
#[tokio::test]
async fn a_generated_bounce_is_stored_under_its_address() {
    let h = harness(BOUNCES, vec![json!([bounce("bob@example.com")])]);
    let (status, source, read) = h.call("GET", "/bounces/bob@example.com", None).await;
    assert_eq!((status, source.as_str()), (StatusCode::OK, "llm"), "{read}");
    assert_eq!(
        h.exported().await,
        [("bounces".to_owned(), "bob@example.com".to_owned())]
    );
    let stored = h.stored().await;
    assert_eq!(stored[0].data, bounce("bob@example.com"));
    let (_, source, again) = h.call("GET", "/bounces/bob@example.com", None).await;
    assert_eq!(source, "state", "{again}");
    assert_eq!(again, json!([bounce("bob@example.com")]));
    assert_eq!(h.calls(), 1);
}

/// Where an unknown id answers 404, an address with no bounce answers `[]`.
#[tokio::test]
async fn an_unknown_address_has_no_bounce() {
    let h = harness_with(BOUNCES, vec![], |rt| rt.unknown_ids = UnknownIds::NotFound);
    let (status, _, read) = h.call("GET", "/bounces/zed@example.com", None).await;
    assert_eq!(status, StatusCode::OK, "{read}");
    assert_eq!(read, json!([]));
    assert_eq!(h.calls(), 0);
}

/// Square's shape: every route answers the payment in a one-member envelope.
const PAYMENTS: &str = r##"
openapi: 3.0.0
info: {title: T, version: "1"}
paths:
  /payments:
    get:
      operationId: listPayments
      responses: {"200": {description: ok, content: {application/json: {schema: {type: object, properties: {payments: {type: array, items: {$ref: "#/components/schemas/Payment"}}, cursor: {type: string}}}}}}}
    post:
      operationId: createPayment
      requestBody: {content: {application/json: {schema: {type: object, properties: {amount: {type: integer}}}}}}
      responses: {"200": {description: ok, content: {application/json: {schema: {type: object, additionalProperties: false, properties: {payment: {$ref: "#/components/schemas/Payment"}}}}}}}
  /payments/{payment_id}:
    parameters: [{name: payment_id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getPayment
      responses: {"200": {description: ok, content: {application/json: {schema: {type: object, properties: {payment: {$ref: "#/components/schemas/Payment"}}}}}}}
  /payments/{payment_id}/complete:
    parameters: [{name: payment_id, in: path, required: true, schema: {type: string}}]
    post:
      operationId: completePayment
      responses: {"200": {description: ok, content: {application/json: {schema: {type: object, additionalProperties: false, properties: {payment: {$ref: "#/components/schemas/Payment"}}}}}}}
components:
  schemas:
    Payment:
      type: object
      required: [id, status, amount]
      properties:
        id: {type: string}
        status: {type: string, enum: [APPROVED, COMPLETED]}
        amount: {type: integer}
"##;

fn payment(status: &str) -> Value {
    json!({"id": "p1", "status": status, "amount": 500})
}

/// A payment in its envelope is stored as the payment, read back in its envelope,
/// and its completion is what the next read answers.
#[tokio::test]
async fn a_payment_in_an_envelope_is_one_record() {
    let h = harness(
        PAYMENTS,
        vec![
            json!({"payment": payment("APPROVED")}),
            json!({"payment": payment("COMPLETED")}),
        ],
    );
    let (status, _, created) = h
        .call("POST", "/payments", Some(json!({"amount": 500})))
        .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    assert_eq!(
        h.exported().await,
        [("payments".to_owned(), "p1".to_owned())]
    );
    assert_eq!(h.stored().await[0].data, payment("APPROVED"));

    let (status, source, read) = h.call("GET", "/payments/p1", None).await;
    assert_eq!(
        (status, source.as_str()),
        (StatusCode::OK, "state"),
        "{read}"
    );
    assert_eq!(read, json!({"payment": payment("APPROVED")}));
    assert_eq!(h.calls(), 1);

    let (status, _, completed) = h.call("POST", "/payments/p1/complete", None).await;
    assert_eq!(status, StatusCode::OK, "{completed}");
    assert_eq!(completed, json!({"payment": payment("COMPLETED")}));
    let (_, source, read) = h.call("GET", "/payments/p1", None).await;
    assert_eq!(source, "state", "{read}");
    assert_eq!(read["payment"]["status"], "COMPLETED");
    assert_eq!(h.calls(), 2);
    assert_eq!(
        h.exported().await,
        [("payments".to_owned(), "p1".to_owned())]
    );
}

/// A generated read of an envelope keeps the path's id inside it, and is stored.
#[tokio::test]
async fn a_generated_envelope_read_takes_the_paths_id() {
    let h = harness(
        PAYMENTS,
        vec![json!({"payment": {"id": "other", "status": "APPROVED", "amount": 7}})],
    );
    let (status, _, read) = h.call("GET", "/payments/p9", None).await;
    assert_eq!(status, StatusCode::OK, "{read}");
    assert_eq!(read["payment"]["id"], "p9");
    assert!(read.get("id").is_none(), "{read}");
    assert_eq!(
        h.exported().await,
        [("payments".to_owned(), "p9".to_owned())]
    );
    let (_, source, _) = h.call("GET", "/payments/p9", None).await;
    assert_eq!(source, "state");
    assert_eq!(h.calls(), 1);
}

/// Zendesk's shape: a read answers `{ticket}`, a write `{ticket, audit}`.
const TICKETS: &str = r##"
openapi: 3.0.0
info: {title: T, version: "1"}
paths:
  /tickets:
    post:
      operationId: createTicket
      requestBody: {content: {application/json: {schema: {type: object, properties: {ticket: {type: object}}}}}}
      responses: {"201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/TicketResponse"}}}}}
  /tickets/{ticket_id}:
    parameters: [{name: ticket_id, in: path, required: true, schema: {type: integer}}]
    get:
      operationId: showTicket
      responses: {"200": {description: ok, content: {application/json: {schema: {type: object, properties: {ticket: {$ref: "#/components/schemas/Ticket"}}}}}}}
    put:
      operationId: updateTicket
      requestBody: {content: {application/json: {schema: {type: object, properties: {ticket: {type: object}}}}}}
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/TicketResponse"}}}}}
components:
  schemas:
    Ticket:
      type: object
      required: [id, subject]
      properties:
        id: {type: integer}
        subject: {type: string}
    TicketResponse:
      type: object
      required: [ticket, audit]
      properties:
        ticket: {$ref: "#/components/schemas/Ticket"}
        audit:
          type: object
          properties:
            id: {type: integer}
            ticket_id: {type: integer}
"##;

/// A ticket written beside its audit is the ticket a read answers.
#[tokio::test]
async fn a_ticket_beside_its_audit_is_one_record() {
    let h = harness(
        TICKETS,
        vec![
            json!({"ticket": {"id": 35, "subject": "Printer"}, "audit": {"id": 1, "ticket_id": 35}}),
            json!({"ticket": {"id": 35, "subject": "Printer on fire"}, "audit": {"id": 2, "ticket_id": 35}}),
        ],
    );
    let (status, _, created) = h
        .call(
            "POST",
            "/tickets",
            Some(json!({"ticket": {"subject": "Printer"}})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(
        h.exported().await,
        [("tickets".to_owned(), "35".to_owned())]
    );
    let (_, source, read) = h.call("GET", "/tickets/35", None).await;
    assert_eq!(source, "state", "{read}");
    assert_eq!(read, json!({"ticket": {"id": 35, "subject": "Printer"}}));

    let (status, _, updated) = h
        .call(
            "PUT",
            "/tickets/35",
            Some(json!({"ticket": {"subject": "Printer on fire"}})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["audit"]["id"], 2);
    let (_, source, read) = h.call("GET", "/tickets/35", None).await;
    assert_eq!(source, "state", "{read}");
    assert_eq!(read["ticket"]["subject"], "Printer on fire");
    assert_eq!(h.calls(), 2);
    assert_eq!(
        h.exported().await,
        [("tickets".to_owned(), "35".to_owned())]
    );
}

/// Jira's shape: a search lists issues, and a later read of one is answered by
/// what the search listed.
const ISSUES: &str = r##"
openapi: 3.0.0
info: {title: T, version: "1"}
paths:
  /search/jql:
    get:
      operationId: searchGet
      parameters: [{name: jql, in: query, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Results"}}}}}
    post:
      operationId: searchPost
      requestBody: {content: {application/json: {schema: {type: object, properties: {jql: {type: string}}}}}}
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Results"}}}}}
  /issues/{issueId}:
    parameters: [{name: issueId, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getIssue
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Issue"}}}}}
components:
  schemas:
    Results:
      type: object
      properties:
        issues: {type: array, items: {$ref: "#/components/schemas/Issue"}}
        isLast: {type: boolean}
    Issue:
      type: object
      properties:
        id: {type: string}
        key: {type: string}
        summary: {type: string}
"##;

#[tokio::test]
async fn a_search_lists_the_issues_a_read_finds() {
    let issue = json!({"id": "10002", "key": "PROJ-2", "summary": "Broken login"});
    let h = harness(
        ISSUES,
        vec![json!({"issues": [issue.clone()], "isLast": true})],
    );
    let (status, _, found) = h.call("GET", "/search/jql?jql=project%3DPROJ", None).await;
    assert_eq!(status, StatusCode::OK, "{found}");
    assert_eq!(
        h.exported().await,
        [("issues".to_owned(), "10002".to_owned())]
    );
    let (_, source, read) = h.call("GET", "/issues/10002", None).await;
    assert_eq!(source, "state", "{read}");
    assert_eq!(read, issue);
    assert_eq!(h.calls(), 1);
}

/// Notion's shape: a query under a data source lists pages, and is shown the pages
/// the session holds.
const PAGES: &str = r##"
openapi: 3.0.0
info: {title: T, version: "1"}
paths:
  /pages:
    post:
      operationId: createPage
      requestBody: {content: {application/json: {schema: {type: object, properties: {title: {type: string}}}}}}
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Page"}}}}}
  /pages/{page_id}:
    parameters: [{name: page_id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getPage
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Page"}}}}}
  /data_sources/{data_source_id}:
    parameters: [{name: data_source_id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getSource
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Source"}}}}}
  /data_sources/{data_source_id}/query:
    parameters: [{name: data_source_id, in: path, required: true, schema: {type: string}}]
    post:
      operationId: querySource
      requestBody: {content: {application/json: {schema: {type: object}}}}
      responses: {"200": {description: ok, content: {application/json: {schema: {type: object, properties: {results: {type: array, items: {$ref: "#/components/schemas/Page"}}, has_more: {type: boolean}}}}}}}
components:
  schemas:
    Page:
      type: object
      properties:
        id: {type: string}
        title: {type: string}
    Source:
      type: object
      properties:
        id: {type: string}
        name: {type: string}
"##;

#[tokio::test]
async fn a_query_lists_pages_and_is_shown_them() {
    let page = |id: &str, title: &str| json!({"id": id, "title": title});
    let h = harness(
        PAGES,
        vec![
            page("pg-1", "Quarterly plan"),
            json!({"results": [page("pg-1", "Quarterly plan"), page("pg-2", "Retro notes")], "has_more": false}),
        ],
    );
    h.call("POST", "/pages", Some(json!({"title": "Quarterly plan"})))
        .await;
    let (status, _, found) = h
        .call("POST", "/data_sources/ds-1/query", Some(json!({})))
        .await;
    assert_eq!(status, StatusCode::OK, "{found}");
    assert!(h.prompt(1).contains("Quarterly plan"), "{}", h.prompt(1));
    assert_eq!(
        h.exported().await,
        [
            ("pages".to_owned(), "pg-1".to_owned()),
            ("pages".to_owned(), "pg-2".to_owned())
        ]
    );
    let (_, source, _) = h.call("GET", "/pages/pg-2", None).await;
    assert_eq!(source, "state");
    assert_eq!(h.calls(), 2);
}

/// OpenWeatherMap's shape: a reading with its own `id` holds a list of conditions.
const WEATHER: &str = r##"
openapi: 3.0.0
info: {title: T, version: "1"}
paths:
  /weather:
    get:
      operationId: getWeather
      parameters: [{name: q, in: query, schema: {type: string}}, {name: id, in: query, schema: {type: integer}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Current"}}}}}
components:
  schemas:
    Current:
      type: object
      required: [id, name, weather]
      properties:
        id: {type: integer}
        name: {type: string}
        weather: {type: array, items: {$ref: "#/components/schemas/Condition"}}
    Condition:
      type: object
      properties:
        id: {type: integer}
        main: {type: string}
"##;

/// The reading is stored as itself, not as the conditions it lists.
#[tokio::test]
async fn a_reading_is_stored_as_itself() {
    let reading =
        json!({"id": 2_950_159, "name": "Berlin", "weather": [{"id": 803, "main": "Clouds"}]});
    let h = harness(WEATHER, vec![reading.clone()]);
    let (status, _, read) = h.call("GET", "/weather?q=Berlin", None).await;
    assert_eq!(status, StatusCode::OK, "{read}");
    assert_eq!(
        h.exported().await,
        [("weather".to_owned(), "2950159".to_owned())]
    );
    assert_eq!(h.stored().await[0].data, reading);
}

/// Spotify's shape: a playlist holds its tracks, which `/playlists/{id}/items` reads.
const PLAYLISTS: &str = r##"
openapi: 3.0.0
info: {title: T, version: "1"}
paths:
  /playlists/{playlist_id}:
    parameters: [{name: playlist_id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getPlaylist
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Playlist"}}}}}
  /playlists/{playlist_id}/items:
    parameters: [{name: playlist_id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getItems
      parameters: [{name: limit, in: query, schema: {type: integer}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/PagingTracks"}}}}}
    post:
      operationId: addItems
      requestBody: {content: {application/json: {schema: {type: object, properties: {uris: {type: array, items: {type: string}}}}}}}
      responses: {"201": {description: ok, content: {application/json: {schema: {type: object, additionalProperties: false, required: [snapshot_id], properties: {snapshot_id: {type: string}}}}}}}
components:
  schemas:
    Playlist:
      type: object
      required: [id, name, tracks]
      properties:
        id: {type: string}
        name: {type: string}
        tracks: {$ref: "#/components/schemas/PagingTracks"}
    PagingTracks:
      type: object
      required: [items, total]
      properties:
        total: {type: integer}
        items: {type: array, items: {$ref: "#/components/schemas/PlaylistTrack"}}
    PlaylistTrack:
      type: object
      required: [added_at, track]
      properties:
        added_at: {type: string}
        track:
          type: object
          properties:
            id: {type: string}
            name: {type: string}
"##;

fn tracks(names: &[&str]) -> Value {
    let items: Vec<Value> = names
        .iter()
        .enumerate()
        .map(|(i, name)| {
            json!({"added_at": "2026-01-01T00:00:00Z", "track": {"id": format!("t{i}"), "name": name}})
        })
        .collect();
    json!({"total": items.len(), "items": items})
}

/// The tracks a stored playlist holds are what its items route answers, and a write
/// there is told to the next read of it, which then generates.
#[tokio::test]
async fn a_playlists_items_are_its_tracks() {
    let playlist = json!({"id": "pl1", "name": "Road trip", "tracks": tracks(&["Drive", "Roam"])});
    let h = harness(
        PLAYLISTS,
        vec![
            playlist.clone(),
            json!({"snapshot_id": "s2"}),
            tracks(&["Drive", "Roam", "Home"]),
        ],
    );
    let (status, _, read) = h.call("GET", "/playlists/pl1", None).await;
    assert_eq!(status, StatusCode::OK, "{read}");
    let (status, source, items) = h.call("GET", "/playlists/pl1/items", None).await;
    assert_eq!(
        (status, source.as_str()),
        (StatusCode::OK, "state"),
        "{items}"
    );
    assert_eq!(items, playlist["tracks"]);
    assert_eq!(h.calls(), 1);

    let (status, _, _) = h
        .call(
            "POST",
            "/playlists/pl1/items",
            Some(json!({"uris": ["spotify:track:home"]})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (_, source, items) = h.call("GET", "/playlists/pl1/items", None).await;
    assert_eq!(source, "llm", "{items}");
    assert!(
        h.prompt(2).contains("spotify:track:home"),
        "{}",
        h.prompt(2)
    );
    assert_eq!(h.calls(), 3);
}

/// With no playlist stored, the items route generates as it did.
#[tokio::test]
async fn an_unknown_playlists_items_are_generated() {
    let h = harness(PLAYLISTS, vec![tracks(&["Drive"])]);
    let (status, source, items) = h.call("GET", "/playlists/pl9/items", None).await;
    assert_eq!(
        (status, source.as_str()),
        (StatusCode::OK, "llm"),
        "{items}"
    );
    assert_eq!(h.calls(), 1);
}

/// A contract whose `id` already works answers as it did, with the same calls. The
/// lines an order holds are no orders: main stored `l1` as one.
#[tokio::test]
async fn a_contract_with_an_id_answers_as_it_did() {
    let contract = r##"
openapi: 3.0.0
info: {title: T, version: "1"}
paths:
  /orders:
    get:
      operationId: listOrders
      responses: {"200": {description: ok, content: {application/json: {schema: {type: object, properties: {data: {type: array, items: {$ref: "#/components/schemas/Order"}}}}}}}}
    post:
      operationId: createOrder
      requestBody: {content: {application/json: {schema: {type: object}}}}
      responses: {"201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Order"}}}}}
  /orders/{id}:
    parameters: [{name: id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getOrder
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Order"}}}}}
  /orders/{id}/events:
    parameters: [{name: id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getEvents
      responses: {"200": {description: ok, content: {application/json: {schema: {type: array, items: {$ref: "#/components/schemas/Event"}}}}}}
components:
  schemas:
    Order:
      type: object
      properties:
        id: {type: string}
        lines: {type: array, items: {$ref: "#/components/schemas/Line"}}
    Line:
      type: object
      properties:
        id: {type: string}
        sku: {type: string}
    Event:
      type: object
      properties:
        id: {type: string}
        kind: {type: string}
"##;
    let order = json!({"id": "o1", "lines": [{"id": "l1", "sku": "A"}]});
    let h = harness(
        contract,
        vec![order.clone(), json!([{"id": "e1", "kind": "paid"}])],
    );
    h.call("POST", "/orders", Some(json!({}))).await;
    let (_, source, read) = h.call("GET", "/orders/o1", None).await;
    assert_eq!((source.as_str(), &read), ("state", &order));
    // A list that is no member's schema is generated, and goes under its own type.
    let (_, source, _) = h.call("GET", "/orders/o1/events", None).await;
    assert_eq!(source, "llm");
    assert_eq!(h.calls(), 2);
    assert_eq!(
        h.exported().await,
        [
            ("events".to_owned(), "e1".to_owned()),
            ("lines".to_owned(), "l1".to_owned()),
            ("orders".to_owned(), "o1".to_owned())
        ]
    );
}
