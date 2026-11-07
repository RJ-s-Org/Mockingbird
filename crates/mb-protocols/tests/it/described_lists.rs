//! Paging, filters, scope and order from a world description (filled by the world-runtime lists task).
//!
//! Each described key replaces only its own heuristic: described paging the page
//! parameter, size and token place; described filters the name-matched ones; a scope
//! is checked as well; an order sorts what a list keeps. The parameter names here are
//! ones the heuristics do not find, so a test shows the description at work.
//!
//! A list the description classifies is answered from the session's records by the
//! state-first executor; a test of the model's own answer sends a parameter the
//! description does not name (`trace`), which sends the list to the model.
#![cfg(test)]
#![cfg(feature = "http-rest")]

use axum::http::StatusCode;
use serde_json::{Value, json};

use crate::support::{self, DescribedHarness, HarnessOptions};

const CONTACTS: &str = r##"
openapi: 3.0.0
info: {title: Contacts, version: "1"}
paths:
  /contacts:
    get:
      operationId: listContacts
      parameters:
        - {name: sheet, in: query, schema: {type: integer, minimum: 1}}
        - {name: rows, in: query, schema: {type: integer}}
        - {name: limit, in: query, schema: {type: integer}}
        - {name: from, in: query, schema: {type: string}}
        - {name: cursor, in: query, schema: {type: string}}
        - {name: email, in: query, schema: {type: string, format: email}}
        - {name: tag, in: query, schema: {type: string}}
        - {name: folder, in: query, schema: {type: string}}
        - {name: minAmount, in: query, schema: {type: string}}
        - {name: atLeast, in: query, schema: {type: integer}}
      responses:
        "200":
          description: ok
          headers:
            X-Next-Cursor: {schema: {type: string}}
          content:
            application/json:
              schema:
                type: object
                required: [contacts]
                properties:
                  contacts: {type: array, items: {$ref: "#/components/schemas/Contact"}}
                  continuation: {type: string}
    post:
      operationId: createContact
      requestBody: {content: {application/json: {schema: {$ref: "#/components/schemas/Contact"}}}}
      responses: {"201": {description: created, content: {application/json: {schema: {$ref: "#/components/schemas/Contact"}}}}}
  /contacts/{contactId}:
    get:
      operationId: getContact
      parameters: [{name: contactId, in: path, required: true, schema: {type: string}}]
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Contact"}}}}}
components:
  schemas:
    Contact:
      type: object
      required: [id]
      properties:
        id: {type: string}
        email: {type: string, format: email}
        billing_address:
          type: object
          properties: {email: {type: string, format: email}}
        tags:
          type: array
          items: {type: object, properties: {tag: {type: string}}}
        parent: {type: string}
        amount: {type: integer}
        createdAt: {type: string, format: date-time}
"##;

/// Lists of the same records whose schema refuses what a described narrowing or token
/// makes: a page of at least one item, a token of at most four characters.
const NARROW_SCHEMAS: &str = r##"
openapi: 3.0.0
info: {title: Narrow, version: "1"}
paths:
  /pinned:
    get:
      operationId: listPinned
      parameters: [{name: email, in: query, schema: {type: string, format: email}}]
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                required: [contacts]
                properties:
                  contacts: {type: array, minItems: 1, items: {$ref: "#/components/schemas/Contact"}}
  /short:
    get:
      operationId: listShort
      parameters:
        - {name: rows, in: query, schema: {type: integer}}
        - {name: from, in: query, schema: {type: string}}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                required: [contacts]
                properties:
                  contacts: {type: array, items: {$ref: "#/components/schemas/Contact"}}
                  continuation: {type: string, maxLength: 4}
components:
  schemas:
    Contact:
      type: object
      required: [id]
      properties:
        id: {type: string}
        email: {type: string, format: email}
        billing_address:
          type: object
          properties: {email: {type: string, format: email}}
        tags:
          type: array
          items: {type: object, properties: {tag: {type: string}}}
        parent: {type: string}
        amount: {type: integer}
        createdAt: {type: string, format: date-time}
"##;

/// A contact `id`, its top-level email and its billing email.
fn contact(id: &str, email: &str, billing: &str) -> Value {
    json!({"id": id, "email": email, "billing_address": {"email": billing}})
}

/// Contacts `c1`..`cn`, each made at a later hour.
fn numbered(n: usize) -> Vec<Value> {
    (1..=n)
        .map(|i| {
            json!({
                "id": format!("c{i}"),
                "email": "a@x.io",
                "createdAt": format!("2024-01-01T{i:02}:00:00Z"),
            })
        })
        .collect()
}

fn list(items: Vec<Value>) -> Value {
    json!({"contacts": items})
}

fn ids(body: &Value) -> Vec<&str> {
    body["contacts"]
        .as_array()
        .map(|items| items.iter().filter_map(|i| i["id"].as_str()).collect())
        .unwrap_or_default()
}

fn world(list_op: &str, desc: Value) -> Value {
    json!({"operations": {list_op: desc}})
}

fn harness(desc: Value, script: Vec<Value>) -> DescribedHarness {
    support::described(CONTACTS, world("listContacts", desc), script)
}

/// A harness over `desc` whose session holds `contacts`, which earlier writes left.
async fn holding(desc: Value, contacts: Vec<Value>) -> DescribedHarness {
    let h = harness(desc, Vec::new());
    h.hold("contacts", &contacts).await;
    h
}

async fn get(h: &DescribedHarness, uri: &str) -> Value {
    let (status, _, body) = h.send("GET", uri, None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{uri}: {body}");
    body
}

fn page_paging() -> Value {
    json!({"paging": {"style": "page", "request": "sheet", "size": "rows"}})
}

fn cursor_paging() -> Value {
    json!({"paging": {"style": "cursor", "request": "from", "response": "continuation", "size": "rows"}})
}

#[tokio::test]
async fn a_described_page_number_reaches_the_second_page() {
    let h = holding(page_paging(), numbered(5)).await;
    let first = get(&h, "/contacts?sheet=1&rows=2").await;
    assert_eq!(ids(&first), ["c1", "c2"]);
    let second = get(&h, "/contacts?sheet=2&rows=2").await;
    assert_eq!(ids(&second), ["c3", "c4"]);
    assert_eq!(h.provider.requests().len(), 0);
}

#[tokio::test]
async fn a_described_cursor_pages_through_and_ends() {
    let h = holding(cursor_paging(), numbered(5)).await;
    let first = get(&h, "/contacts?rows=2").await;
    assert_eq!(ids(&first), ["c1", "c2"]);
    let token = first["continuation"].as_str().unwrap().to_string();
    let second = get(&h, &format!("/contacts?rows=2&from={token}")).await;
    assert_eq!(ids(&second), ["c3", "c4"]);
    let token = second["continuation"].as_str().unwrap().to_string();
    let last = get(&h, &format!("/contacts?rows=2&from={token}")).await;
    assert_eq!(ids(&last), ["c5"]);
    assert!(
        last.get("continuation")
            .is_none_or(|t| t.is_null() || t == ""),
        "{last}"
    );
    assert_eq!(h.provider.requests().len(), 0);
}

#[tokio::test]
async fn a_header_cursor_is_served_in_its_header() {
    let h = holding(
        json!({"paging": {"style": "header_cursor", "request": "cursor", "response_header": "X-Next-Cursor", "size": "rows"}}),
        numbered(5),
    )
    .await;
    let (_, headers, first) = h.send("GET", "/contacts?rows=2", None, &[]).await;
    assert_eq!(ids(&first), ["c1", "c2"]);
    let token = headers["x-next-cursor"].to_str().unwrap().to_string();
    assert_eq!(token.len(), 40, "{token}");
    let (_, headers, second) = h
        .send(
            "GET",
            &format!("/contacts?rows=2&cursor={token}"),
            None,
            &[],
        )
        .await;
    assert_eq!(ids(&second), ["c3", "c4"]);
    let next = headers["x-next-cursor"].to_str().unwrap().to_string();
    assert_ne!(next, token);
    let (_, headers, last) = h
        .send("GET", &format!("/contacts?rows=2&cursor={next}"), None, &[])
        .await;
    assert_eq!(ids(&last), ["c5"]);
    // The last page carries no next token.
    assert!(
        headers.get("x-next-cursor").is_none_or(|v| v.is_empty()),
        "{headers:?}"
    );
    assert_eq!(h.provider.requests().len(), 0);
}

#[tokio::test]
async fn a_header_cursor_list_on_one_page_carries_no_next() {
    let h = holding(
        json!({"paging": {"style": "header_cursor", "request": "cursor", "response_header": "X-Next-Cursor", "size": "rows"}}),
        numbered(2),
    )
    .await;
    let (_, headers, only) = h.send("GET", "/contacts?rows=5", None, &[]).await;
    assert_eq!(ids(&only), ["c1", "c2"]);
    assert!(
        headers.get("x-next-cursor").is_none_or(|v| v.is_empty()),
        "{headers:?}"
    );
}

#[tokio::test]
async fn a_described_page_follows_a_numeric_filter() {
    let amounts = (1..=6)
        .map(|i| json!({"id": format!("c{i}"), "amount": i}))
        .collect();
    let desc = json!({
        "filters": [{"param": "atLeast", "member": "amount", "kind": "gte"}],
        "paging": {"style": "page", "request": "sheet", "size": "rows"},
    });
    let h = holding(desc, amounts).await;
    let first = get(&h, "/contacts?atLeast=3&rows=2").await;
    assert_eq!(ids(&first), ["c3", "c4"]);
    let second = get(&h, "/contacts?atLeast=3&rows=2&sheet=2").await;
    assert_eq!(ids(&second), ["c5", "c6"]);
    assert_eq!(h.provider.requests().len(), 0);
}

#[tokio::test]
async fn a_described_cut_with_no_token_place_keeps_its_filters() {
    // A cursor paging that names no answer member nor header has no described place for
    // a token: the page is narrowed as described and cut to the size asked, as an
    // undescribed one is.
    let amounts = (1..=5)
        .map(|i| json!({"id": format!("c{i}"), "amount": i}))
        .collect();
    let desc = json!({
        "filters": [{"param": "atLeast", "member": "amount", "kind": "gte"}],
        "paging": {"style": "cursor", "request": "from", "size": "rows"},
    });
    let h = holding(desc, amounts).await;
    let body = get(&h, "/contacts?atLeast=2&rows=2").await;
    assert_eq!(ids(&body), ["c2", "c3"]);
    assert_eq!(h.provider.requests().len(), 0);
}

#[tokio::test]
async fn a_nested_member_filter_narrows() {
    let h = holding(
        json!({"filters": [{"param": "email", "member": "billing_address.email", "kind": "eq"}]}),
        vec![
            contact("c1", "z@x.io", "a@x.io"),
            contact("c2", "a@x.io", "b@x.io"),
            contact("c3", "y@x.io", "a@x.io"),
        ],
    )
    .await;
    let body = get(&h, "/contacts?email=a@x.io").await;
    assert_eq!(ids(&body), ["c1", "c3"]);
}

/// Over the model's answer, an item that does not hold a described filter's member (or
/// holds `null` there) says nothing about it and is kept; one holding another value is
/// left out.
#[tokio::test]
async fn a_described_filter_keeps_an_answered_item_lacking_the_member() {
    let h = harness(
        json!({"filters": [{"param": "email", "member": "billing_address.email", "kind": "eq"}]}),
        vec![list(vec![
            contact("c1", "z@x.io", "a@x.io"),
            json!({"id": "c2", "email": "a@x.io"}),
            contact("c3", "a@x.io", "b@x.io"),
            json!({"id": "c4", "billing_address": {"email": null}}),
        ])],
    );
    let body = get(&h, "/contacts?email=a@x.io&trace=1").await;
    assert_eq!(ids(&body), ["c1", "c2", "c4"]);
}

/// A numeric bound over the model's answer keeps an item with no amount too.
#[tokio::test]
async fn a_described_bound_keeps_an_answered_item_lacking_the_member() {
    let h = harness(
        json!({"filters": [{"param": "atLeast", "member": "amount", "kind": "gte"}]}),
        vec![list(vec![
            json!({"id": "c1", "amount": 1}),
            json!({"id": "c2"}),
            json!({"id": "c3", "amount": 5}),
        ])],
    );
    let body = get(&h, "/contacts?atLeast=3&trace=1").await;
    assert_eq!(ids(&body), ["c2", "c3"]);
}

#[tokio::test]
async fn a_contains_filter_matches_an_array_of_objects() {
    let h = holding(
        json!({"filters": [{"param": "tag", "member": "tags", "kind": "contains"}]}),
        vec![
            json!({"id": "c1", "tags": [{"tag": "urgent"}]}),
            json!({"id": "c2", "tags": [{"tag": "later"}]}),
            json!({"id": "c3", "tags": [{"tag": "later"}, {"tag": "urgent"}]}),
        ],
    )
    .await;
    let body = get(&h, "/contacts?tag=urgent").await;
    assert_eq!(ids(&body), ["c1", "c3"]);
}

#[tokio::test]
async fn scope_keeps_only_the_parents_records() {
    let h = holding(
        json!({"scope": {"param": "folder", "member": "parent"}}),
        vec![
            json!({"id": "c1", "parent": "f1"}),
            json!({"id": "c2", "parent": "f2"}),
            json!({"id": "c3", "parent": "f1"}),
        ],
    )
    .await;
    let body = get(&h, "/contacts?folder=f1").await;
    assert_eq!(ids(&body), ["c1", "c3"]);
}

#[tokio::test]
async fn order_descending_sorts_newest_first() {
    let h = holding(
        json!({"order": {"member": "createdAt", "descending": true}}),
        numbered(3),
    )
    .await;
    let body = get(&h, "/contacts").await;
    assert_eq!(ids(&body), ["c3", "c2", "c1"]);
}

#[tokio::test]
async fn an_unknown_page_token_falls_back_to_generation() {
    let h = harness(cursor_paging(), vec![list(numbered(5))]);
    let body = get(&h, "/contacts?rows=2&from=never-minted").await;
    assert_eq!(ids(&body), ["c1", "c2", "c3", "c4", "c5"]);
    assert!(body.get("continuation").is_none(), "{body}");
    assert_eq!(h.provider.requests().len(), 1);
}

#[tokio::test]
async fn a_mistyped_filter_value_narrows_nothing() {
    let h = holding(
        json!({"filters": [{"param": "minAmount", "member": "amount", "kind": "gte"}]}),
        vec![
            json!({"id": "c1", "amount": 3}),
            json!({"id": "c2", "amount": 7}),
        ],
    )
    .await;
    assert_eq!(ids(&get(&h, "/contacts?minAmount=abc").await), ["c1", "c2"]);
    assert_eq!(ids(&get(&h, "/contacts?minAmount=5").await), ["c2"]);
}

#[tokio::test]
async fn a_page_past_the_end_is_empty_with_no_next() {
    let h = holding(page_paging(), numbered(3)).await;
    assert_eq!(
        ids(&get(&h, "/contacts?sheet=1&rows=2").await),
        ["c1", "c2"]
    );
    assert_eq!(ids(&get(&h, "/contacts?sheet=2&rows=2").await), ["c3"]);
    let past = get(&h, "/contacts?sheet=3&rows=2").await;
    assert_eq!(past["contacts"], json!([]));
    assert!(past.get("continuation").is_none(), "{past}");
    assert_eq!(h.provider.requests().len(), 0);
}

/// Four contacts, three of them `a@x.io` at the top level, made one hour apart.
fn mixed_contacts() -> Vec<Value> {
    let mut items = numbered(4);
    items[1]["email"] = json!("b@x.io");
    items
}

/// [`mixed_contacts`] as the model lists them.
fn mixed() -> Value {
    list(mixed_contacts())
}

#[tokio::test]
async fn a_described_paging_alone_keeps_name_matched_filters() {
    let plain = support::harness(CONTACTS, vec![mixed()], &HarnessOptions::default(), |rt| rt);
    let body = get(&plain, "/contacts?email=a@x.io&rows=2").await;
    // No world: the name-matched `email` filter narrows, and `rows` is no page size.
    assert_eq!(ids(&body), ["c1", "c3", "c4"]);
    let h = holding(page_paging(), mixed_contacts()).await;
    let body = get(&h, "/contacts?email=a@x.io&rows=2").await;
    assert_eq!(ids(&body), ["c1", "c3"]);
    let next = get(&h, "/contacts?email=a@x.io&rows=2&sheet=2").await;
    assert_eq!(ids(&next), ["c4"]);
}

#[tokio::test]
async fn a_described_order_alone_keeps_filters_and_paging() {
    let plain = support::harness(CONTACTS, vec![mixed()], &HarnessOptions::default(), |rt| rt);
    let body = get(&plain, "/contacts?email=a@x.io&limit=2").await;
    assert_eq!(ids(&body), ["c1", "c3"]);
    let h = holding(
        json!({"order": {"member": "createdAt", "descending": true}}),
        mixed_contacts(),
    )
    .await;
    let body = get(&h, "/contacts?email=a@x.io&limit=2").await;
    // The same filter and page size, over the items newest first.
    assert_eq!(ids(&body), ["c4", "c3"]);
}

#[tokio::test]
async fn empty_described_filters_keep_name_matching() {
    let h = holding(
        json!({"filters": [], "order": {"member": "createdAt", "descending": true}}),
        mixed_contacts(),
    )
    .await;
    let body = get(&h, "/contacts?email=a@x.io").await;
    assert_eq!(ids(&body), ["c4", "c3", "c1"]);
}

#[tokio::test]
async fn held_records_a_list_adds_follow_the_described_query() {
    let held = |id: &str, billing: &str, hour: u32| {
        json!({
            "id": id,
            "email": "h@x.io",
            "billing_address": {"email": billing},
            "parent": "f1",
            "createdAt": format!("2024-02-01T{hour:02}:00:00Z"),
        })
    };
    let desc = json!({
        "filters": [{"param": "email", "member": "billing_address.email", "kind": "eq"}],
        "scope": {"param": "folder", "member": "parent"},
        "order": {"member": "createdAt"},
        "paging": {"style": "page", "request": "sheet", "size": "rows"},
    });
    let h = harness(
        desc,
        vec![
            held("h1", "a@x.io", 2),
            held("h2", "b@x.io", 1),
            held("h3", "a@x.io", 1),
        ],
    );
    for (id, billing, hour) in [
        ("h1", "a@x.io", 2),
        ("h2", "b@x.io", 1),
        ("h3", "a@x.io", 1),
    ] {
        let (status, _, _) = h
            .seeded("POST", "/contacts", Some(held(id, billing, hour)), &[])
            .await;
        assert_eq!(status, StatusCode::CREATED);
    }
    let uri = |sheet: u32| format!("/contacts?folder=f1&email=a@x.io&rows=1&sheet={sheet}");
    // The held records the query asks for, in the described order; not `h2`.
    assert_eq!(ids(&get(&h, &uri(1)).await), ["h3"]);
    assert_eq!(ids(&get(&h, &uri(2)).await), ["h1"]);
    assert_eq!(get(&h, &uri(3)).await["contacts"], json!([]));
    assert_eq!(h.provider.requests().len(), 3);
}

#[tokio::test]
async fn a_narrowed_page_the_schema_refuses_is_served_unnarrowed() {
    let h = support::described(
        NARROW_SCHEMAS,
        world(
            "listPinned",
            json!({"filters": [{"param": "email", "kind": "eq"}]}),
        ),
        vec![list(vec![
            contact("c1", "a@x.io", "a@x.io"),
            contact("c2", "b@x.io", "b@x.io"),
        ])],
    );
    let body = get(&h, "/pinned?email=none@x.io").await;
    assert_eq!(ids(&body), ["c1", "c2"]);
}

#[tokio::test]
async fn a_next_token_the_schema_refuses_is_not_written() {
    let h = support::described(
        NARROW_SCHEMAS,
        world(
            "listShort",
            json!({"paging": {"style": "cursor", "request": "from", "response": "continuation", "size": "rows"}}),
        ),
        vec![list(numbered(5))],
    );
    let body = get(&h, "/short?rows=2").await;
    assert_eq!(ids(&body), ["c1", "c2", "c3", "c4", "c5"]);
    assert!(body.get("continuation").is_none(), "{body}");
}
