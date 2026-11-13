//! A parent's embedded child list and its child records are one set of facts: a TMDB
//! show's `seasons` are the seasons `/tv/{id}/season/{n}` reads, and a Stripe
//! subscription's `items` list object holds the items its list route answers.
#![cfg(feature = "http-rest")]

use axum::http::StatusCode;
use serde_json::{Value, json};

use crate::support::{self, HarnessOptions};

const TMDB: &str = r##"
openapi: 3.0.3
info: {title: Shows, version: "1"}
paths:
  /3/tv/{series_id}:
    parameters:
      - {name: series_id, in: path, required: true, schema: {type: integer}}
    get:
      operationId: tv-series-details
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/tv-series-details"}}}}
        "404": {description: missing}
  /3/tv/{series_id}/season/{season_number}:
    parameters:
      - {name: series_id, in: path, required: true, schema: {type: integer}}
      - {name: season_number, in: path, required: true, schema: {type: integer}}
    get:
      operationId: tv-season-details
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/tv-season"}}}}
        "404": {description: missing}
  /3/tv/{series_id}/season/{season_number}/episode/{episode_number}:
    parameters:
      - {name: series_id, in: path, required: true, schema: {type: integer}}
      - {name: season_number, in: path, required: true, schema: {type: integer}}
      - {name: episode_number, in: path, required: true, schema: {type: integer}}
    get:
      operationId: tv-episode-details
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                required: [id, episode_number, name, runtime]
                properties:
                  id: {type: integer}
                  episode_number: {type: integer}
                  name: {type: string}
                  runtime: {type: integer}
        "404": {description: missing}
components:
  schemas:
    tv-season:
      type: object
      required: [_id, air_date, episodes, id, name, season_number]
      properties:
        _id: {type: string}
        air_date: {type: string, format: date}
        id: {type: integer}
        name: {type: string}
        season_number: {type: integer, minimum: 0}
        episodes:
          type: array
          items:
            type: object
            required: [id, episode_number, name]
            properties:
              id: {type: integer}
              episode_number: {type: integer}
              name: {type: string}
    tv-series-details:
      type: object
      required: [id, name, seasons]
      properties:
        id: {type: integer}
        name: {type: string}
        number_of_seasons: {type: integer}
        seasons:
          type: array
          maxItems: 6
          items:
            type: object
            required: [air_date, episode_count, id, name, season_number]
            properties:
              air_date: {type: [string, "null"], format: date}
              episode_count: {type: integer}
              id: {type: integer}
              name: {type: string}
              season_number: {type: integer, minimum: 0}
"##;

const STRIPE: &str = r##"
openapi: 3.0.3
info: {title: Billing, version: "1"}
paths:
  /v1/subscriptions/{subscription_exposed_id}:
    parameters:
      - {name: subscription_exposed_id, in: path, required: true, schema: {type: string, pattern: "^sub_[A-Za-z0-9]+$"}}
    get:
      operationId: GetSubscriptionsSubscriptionExposedId
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/subscription"}}}}
        "404": {description: missing}
  /v1/subscription_items:
    post:
      operationId: PostSubscriptionItems
      requestBody:
        required: true
        content: {application/json: {schema: {type: object, required: [subscription], properties: {subscription: {type: string}, quantity: {type: integer}}}}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/subscription_item"}}}}
    get:
      operationId: GetSubscriptionItems
      parameters:
        - {name: subscription, in: query, required: true, schema: {type: string}}
        - {name: limit, in: query, schema: {type: integer}}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                required: [object, data, has_more, url]
                properties:
                  object: {type: string, enum: [list]}
                  data: {type: array, items: {$ref: "#/components/schemas/subscription_item"}}
                  has_more: {type: boolean}
                  url: {type: string}
  /v1/subscription_items/{item}:
    parameters:
      - {name: item, in: path, required: true, schema: {type: string, pattern: "^si_[A-Za-z0-9]+$"}}
    get:
      operationId: GetSubscriptionItemsItem
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/subscription_item"}}}}
        "404": {description: missing}
    post:
      operationId: PostSubscriptionItemsItem
      requestBody:
        content: {application/json: {schema: {type: object, properties: {quantity: {type: integer}}}}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/subscription_item"}}}}
    delete:
      operationId: DeleteSubscriptionItemsItem
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                required: [id, deleted]
                properties:
                  id: {type: string}
                  deleted: {type: boolean}
components:
  schemas:
    subscription_item:
      type: object
      required: [id, object, quantity, subscription]
      properties:
        id: {type: string, pattern: "^si_[A-Za-z0-9]+$"}
        object: {type: string, enum: [subscription_item]}
        quantity: {type: integer}
        subscription: {type: string}
    subscription:
      type: object
      required: [id, object, items, status]
      properties:
        id: {type: string, pattern: "^sub_[A-Za-z0-9]+$"}
        object: {type: string, enum: [subscription]}
        status: {type: string}
        items:
          type: object
          required: [object, data, has_more, url]
          properties:
            object: {type: string, enum: [list]}
            data: {type: array, items: {$ref: "#/components/schemas/subscription_item"}}
            has_more: {type: boolean}
            total_count: {type: integer}
            url: {type: string}
"##;

fn tmdb(script: Vec<Value>) -> support::DescribedHarness {
    support::harness(TMDB, script, &HarnessOptions::default(), |rt| rt)
}

fn stripe(script: Vec<Value>) -> support::DescribedHarness {
    support::harness(STRIPE, script, &HarnessOptions::default(), |rt| rt)
}

fn session(h: &support::DescribedHarness) -> mb_state::SessionKey {
    mb_state::SessionKey {
        workspace: h.rt.workspace.clone(),
        mock: h.rt.name.clone(),
        session: "default".into(),
    }
}

fn source(headers: &axum::http::HeaderMap) -> &str {
    headers
        .get("x-mockingbird-source")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
}

/// The stored records of type `t`, as the store keys them, sorted.
async fn keys(h: &support::DescribedHarness, t: &str) -> Vec<String> {
    let all = h.rt.state.export(&session(h)).await.unwrap();
    let mut ids: Vec<String> = all
        .into_iter()
        .filter(|r| r.resource_type == t)
        .map(|r| r.id)
        .collect();
    ids.sort();
    ids
}

fn season_summary(id: u64, n: u64, name: &str) -> Value {
    json!({"air_date": "2008-01-20", "episode_count": 7, "id": id, "name": name, "season_number": n})
}

/// A show's seasons as (number, id).
fn seasons_of(show: &Value) -> Vec<(u64, u64)> {
    show["seasons"]
        .as_array()
        .unwrap_or(&Vec::new())
        .iter()
        .map(|s| {
            (
                s["season_number"].as_u64().unwrap_or(u64::MAX),
                s["id"].as_u64().unwrap_or(u64::MAX),
            )
        })
        .collect()
}

/// A show's details list seasons 1 and 2; `/season/1` then answers the season the show
/// listed, and the seasons are stored under the show.
#[tokio::test]
async fn a_season_read_after_the_show_is_the_season_it_listed() {
    let h = tmdb(vec![
        json!({"id": 1396, "name": "Breaking Bad", "seasons": [
            season_summary(3572, 1, "Season 1"),
            season_summary(3573, 2, "Season 2"),
        ]}),
    ]);
    let (status, _, show) = h.send("GET", "/3/tv/1396", None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{show}");
    assert_eq!(seasons_of(&show), [(1, 3572), (2, 3573)]);
    assert_eq!(
        keys(&h, "season").await,
        ["1396\u{1f}3572", "1396\u{1f}3573"]
    );
    for (n, id) in [(1, 3572), (2, 3573), (1, 3572)] {
        let (status, _, season) = h
            .send("GET", &format!("/3/tv/1396/season/{n}"), None, &[])
            .await;
        assert_eq!(status, StatusCode::OK, "{season}");
        assert_eq!(season["id"], id, "{season}");
        assert_eq!(season["season_number"], n, "{season}");
        assert!(season["episodes"].is_array(), "{season}");
    }
    assert_eq!(
        keys(&h, "season").await,
        ["1396\u{1f}3572", "1396\u{1f}3573"]
    );
    let (_, _, again) = h.send("GET", "/3/tv/1396", None, &[]).await;
    assert_eq!(seasons_of(&again), [(1, 3572), (2, 3573)], "{again}");
}

/// A season read first is listed in the show's details read after, as it was read,
/// though the show's own answer named another season 1.
#[tokio::test]
async fn a_season_read_first_is_listed_in_the_show() {
    let h = tmdb(vec![
        json!({"_id": "x", "air_date": "2008-01-20", "id": 9999, "name": "Pilot season", "season_number": 1, "episodes": []}),
        json!({"id": 1396, "name": "Breaking Bad", "seasons": [
            season_summary(3573, 2, "Season 2"),
            season_summary(3572, 1, "Season 1"),
        ]}),
    ]);
    let (status, _, season) = h.send("GET", "/3/tv/1396/season/1", None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{season}");
    assert_eq!(season["id"], 9999);
    let (status, _, show) = h.send("GET", "/3/tv/1396", None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{show}");
    assert_eq!(seasons_of(&show), [(2, 3573), (1, 9999)], "{show}");
    assert_eq!(show["seasons"][1]["name"], "Pilot season", "{show}");
    let (_, _, season) = h.send("GET", "/3/tv/1396/season/1", None, &[]).await;
    assert_eq!(season["id"], 9999, "{season}");
    let (_, headers, again) = h.send("GET", "/3/tv/1396", None, &[]).await;
    assert_eq!(source(&headers), "state", "{again}");
    assert_eq!(seasons_of(&again), [(2, 3573), (1, 9999)], "{again}");
}

/// A season generated after its show joins the show's seasons, stored and answered.
#[tokio::test]
async fn a_season_generated_after_the_show_joins_its_list() {
    let h = tmdb(vec![
        json!({"id": 1396, "name": "Breaking Bad", "seasons": [season_summary(3572, 1, "Season 1")]}),
        json!({"_id": "y", "air_date": "2009-03-08", "id": 3573, "name": "Season 2", "season_number": 2, "episodes": []}),
    ]);
    h.send("GET", "/3/tv/1396", None, &[]).await;
    let (status, _, season) = h.send("GET", "/3/tv/1396/season/2", None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{season}");
    assert_eq!(season["id"], 3573);
    let stored = h.stored("tv", "1396").await.unwrap();
    assert_eq!(seasons_of(&stored), [(1, 3572), (2, 3573)], "{stored}");
    let (_, _, show) = h.send("GET", "/3/tv/1396", None, &[]).await;
    assert_eq!(seasons_of(&show), [(1, 3572), (2, 3573)], "{show}");
}

/// Two shows keep their own seasons: each one's season 1 is the one it listed.
#[tokio::test]
async fn two_shows_keep_their_own_seasons() {
    let h = tmdb(vec![
        json!({"id": 1396, "name": "Breaking Bad", "seasons": [season_summary(3572, 1, "BB 1")]}),
        json!({"id": 549, "name": "Law & Order", "seasons": [season_summary(1946, 1, "LO 1")]}),
    ]);
    h.send("GET", "/3/tv/1396", None, &[]).await;
    h.send("GET", "/3/tv/549", None, &[]).await;
    for (show, id, name) in [
        (549, 1946, "LO 1"),
        (1396, 3572, "BB 1"),
        (549, 1946, "LO 1"),
    ] {
        let (status, _, season) = h
            .send("GET", &format!("/3/tv/{show}/season/1"), None, &[])
            .await;
        assert_eq!(status, StatusCode::OK, "{season}");
        assert_eq!(season["id"], id, "{season}");
        assert_eq!(season["name"], name, "{season}");
    }
    assert_eq!(
        keys(&h, "season").await,
        ["1396\u{1f}3572", "549\u{1f}1946"]
    );
}

/// A held season is never overwritten by the show's embed: the show lists the held one.
#[tokio::test]
async fn a_held_season_is_not_overwritten_by_the_show() {
    let h = tmdb(vec![
        json!({"id": 1396, "name": "Breaking Bad", "seasons": [
            season_summary(3572, 1, "From the show"),
        ]}),
    ]);
    let held = json!({"_id": "h", "air_date": "2008-01-20", "id": 3572, "name": "Held", "season_number": 1, "episodes": []});
    h.rt.state
        .upsert(
            &session(&h),
            mb_state::StoredResource {
                resource_type: "season".into(),
                id: "1396\u{1f}3572".into(),
                data: held.clone(),
            },
        )
        .await
        .unwrap();
    let (_, _, show) = h.send("GET", "/3/tv/1396", None, &[]).await;
    assert_eq!(show["seasons"][0]["name"], "Held", "{show}");
    assert_eq!(h.stored("season", "3572").await.unwrap(), held);
    let (_, _, season) = h.send("GET", "/3/tv/1396/season/1", None, &[]).await;
    assert_eq!(season["name"], "Held", "{season}");
}

fn item(id: &str, quantity: u64) -> Value {
    json!({"id": id, "object": "subscription_item", "quantity": quantity, "subscription": "sub_1"})
}

fn ids(list: &Value) -> Vec<String> {
    list["data"]
        .as_array()
        .unwrap_or(&Vec::new())
        .iter()
        .filter_map(|i| i["id"].as_str().map(str::to_string))
        .collect()
}

/// A subscription's `items` are the items its list route answers: those it embeds are
/// stored, and the list object names that route.
#[tokio::test]
async fn a_subscription_s_items_are_its_item_list() {
    let h = stripe(vec![
        json!({"id": "sub_1", "object": "subscription", "status": "active", "items": {
            "object": "list", "data": [item("si_a", 1), item("si_b", 2)], "has_more": false,
            "url": "/v1/subscription_items?subscription=sub_other"
        }}),
    ]);
    let (status, _, sub) = h.send("GET", "/v1/subscriptions/sub_1", None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{sub}");
    assert_eq!(ids(&sub["items"]), ["si_a", "si_b"], "{sub}");
    assert_eq!(sub["items"]["has_more"], false, "{sub}");
    assert_eq!(
        sub["items"]["url"], "/v1/subscription_items?subscription=sub_1",
        "{sub}"
    );
    assert_eq!(h.stored_ids("subscription_items").await, ["si_a", "si_b"]);
    let (status, _, item) = h
        .send("GET", "/v1/subscription_items/si_b", None, &[])
        .await;
    assert_eq!(status, StatusCode::OK, "{item}");
    assert_eq!(item["quantity"], 2, "{item}");
    let (status, _, list) = h
        .send(
            "GET",
            "/v1/subscription_items?subscription=sub_1",
            None,
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{list}");
    let mut listed = ids(&list);
    listed.sort();
    assert_eq!(listed, ["si_a", "si_b"], "{list}");
}

/// A subscription holding more items than a page shows its first page, `has_more`, and
/// its `total_count`; a held item is not overwritten by the embed.
#[tokio::test]
async fn a_subscription_s_items_keep_their_page_size() {
    let h = stripe(Vec::new());
    let mut items: Vec<Value> = (0..12).map(|n| item(&format!("si_{n:02}"), n)).collect();
    items[0]["quantity"] = json!(100);
    h.hold("subscription_items", &items).await;
    h.hold(
        "subscriptions",
        &[
            json!({"id": "sub_1", "object": "subscription", "status": "active", "items": {
                "object": "list", "data": [item("si_00", 1)], "has_more": false, "total_count": 1,
                "url": "/v1/subscription_items?subscription=sub_1"
            }}),
        ],
    )
    .await;
    let (status, _, sub) = h.send("GET", "/v1/subscriptions/sub_1", None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{sub}");
    let listed = ids(&sub["items"]);
    assert_eq!(listed.len(), 10, "{sub}");
    assert_eq!(listed[0], "si_00", "{sub}");
    assert_eq!(sub["items"]["data"][0]["quantity"], 100, "{sub}");
    assert_eq!(sub["items"]["has_more"], true, "{sub}");
    assert_eq!(sub["items"]["total_count"], 12, "{sub}");
    assert_eq!(
        sub["items"]["url"], "/v1/subscription_items?subscription=sub_1",
        "{sub}"
    );
    assert_eq!(
        h.stored("subscription_items", "si_00").await.unwrap()["quantity"],
        100
    );
}

/// The items held for `sub_1` and a subscription embedding them.
async fn hold_subscription(h: &support::DescribedHarness) {
    h.hold("subscription_items", &[item("si_a", 1), item("si_b", 2)])
        .await;
    h.hold(
        "subscriptions",
        &[
            json!({"id": "sub_1", "object": "subscription", "status": "active", "items": {
                "object": "list", "data": [item("si_a", 1), item("si_b", 2)], "has_more": false,
                "total_count": 2, "url": "/v1/subscription_items?subscription=sub_1"
            }}),
        ],
    )
    .await;
}

/// An item created, updated and deleted is in, changed in and out of its subscription's
/// `items`; the last one deleted leaves the list empty.
#[tokio::test]
async fn item_writes_show_in_the_subscription() {
    let h = stripe(Vec::new());
    hold_subscription(&h).await;
    let (status, _, created) = h
        .send(
            "POST",
            "/v1/subscription_items",
            Some(json!({"subscription": "sub_1", "quantity": 7})),
            &[],
        )
        .await;
    assert!(status.is_success(), "{status} {created}");
    let new_id = created["id"].as_str().unwrap_or_default().to_string();
    assert!(new_id.starts_with("si_"), "{created}");
    let stored = h.stored("subscriptions", "sub_1").await.unwrap();
    assert_eq!(
        ids(&stored["items"]),
        ["si_a", "si_b", new_id.as_str()],
        "{stored}"
    );
    assert_eq!(stored["items"]["total_count"], 3, "{stored}");

    let (status, _, updated) = h
        .send(
            "POST",
            "/v1/subscription_items/si_b",
            Some(json!({"quantity": 9})),
            &[],
        )
        .await;
    assert!(status.is_success(), "{status} {updated}");
    let (_, _, sub) = h.send("GET", "/v1/subscriptions/sub_1", None, &[]).await;
    assert_eq!(sub["items"]["data"][1]["quantity"], 9, "{sub}");

    for gone in ["si_a", "si_b", new_id.as_str()] {
        let (status, _, body) = h
            .send(
                "DELETE",
                &format!("/v1/subscription_items/{gone}"),
                None,
                &[],
            )
            .await;
        assert!(status.is_success(), "{status} {body}");
    }
    let stored = h.stored("subscriptions", "sub_1").await.unwrap();
    assert_eq!(ids(&stored["items"]), Vec::<String>::new(), "{stored}");
    assert_eq!(stored["items"]["total_count"], 0, "{stored}");
}

/// A list of a subscription's items answered before the subscription was read answers
/// the items the subscription embeds once it is: the cached answer does not stand.
#[tokio::test]
async fn an_item_list_read_again_after_its_subscription_shows_its_items() {
    let h = support::harness(
        STRIPE,
        vec![
            json!({"object": "list", "data": [], "has_more": false, "url": "/v1/subscription_items"}),
            json!({"id": "sub_1", "object": "subscription", "status": "active", "items": {
                "object": "list", "data": [item("si_a", 1)], "has_more": false,
                "url": "/v1/subscription_items?subscription=sub_1"
            }}),
        ],
        &HarnessOptions {
            cached: true,
            ..HarnessOptions::default()
        },
        |rt| rt,
    );
    let uri = "/v1/subscription_items?subscription=sub_1";
    let (_, _, before) = h.send("GET", uri, None, &[]).await;
    assert_eq!(ids(&before), Vec::<String>::new(), "{before}");
    h.send("GET", "/v1/subscriptions/sub_1", None, &[]).await;
    let (status, _, after) = h.send("GET", uri, None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{after}");
    assert_eq!(ids(&after), ["si_a"], "{after}");
}

/// A season that is a parent too (of its episodes) generated after its show still joins
/// the show's seasons, and its episodes are stored under it.
#[tokio::test]
async fn a_season_with_episodes_joins_its_show() {
    let h = tmdb(vec![
        json!({"id": 1396, "name": "Breaking Bad", "seasons": [season_summary(3572, 1, "Season 1")]}),
        json!({"_id": "y", "air_date": "2009-03-08", "id": 3573, "name": "Season 2", "season_number": 2,
            "episodes": [{"id": 62092, "episode_number": 1, "name": "Seven Thirty-Seven"}]}),
    ]);
    h.send("GET", "/3/tv/1396", None, &[]).await;
    let (status, _, season) = h.send("GET", "/3/tv/1396/season/2", None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{season}");
    let stored = h.stored("tv", "1396").await.unwrap();
    assert_eq!(seasons_of(&stored), [(1, 3572), (2, 3573)], "{stored}");
    assert_eq!(keys(&h, "episode").await, ["1396\u{1f}2\u{1f}62092"]);
    let (status, _, episode) = h
        .send("GET", "/3/tv/1396/season/2/episode/1", None, &[])
        .await;
    assert_eq!(status, StatusCode::OK, "{episode}");
    assert_eq!(episode["id"], 62092, "{episode}");
}

/// With no model, a show read before its seasons lists a season the faker made; the
/// seasons read after replace it: the show lists exactly them, by their ids.
#[tokio::test]
async fn seasons_read_after_the_show_replace_its_filler() {
    let h = support::harness(
        TMDB,
        Vec::new(),
        &HarnessOptions {
            offline: true,
            cached: true,
            ..HarnessOptions::default()
        },
        |rt| rt,
    );
    let (status, _, show) = h.send("GET", "/3/tv/1396", None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{show}");
    let filler = seasons_of(&show);
    assert!(!filler.is_empty(), "{show}");
    let mut read = Vec::new();
    for n in [1, 2] {
        let (status, _, season) = h
            .send("GET", &format!("/3/tv/1396/season/{n}"), None, &[])
            .await;
        assert_eq!(status, StatusCode::OK, "{season}");
        assert_eq!(season["season_number"], n, "{season}");
        read.push((n, season["id"].as_u64().unwrap()));
    }
    let (_, _, again) = h.send("GET", "/3/tv/1396", None, &[]).await;
    assert_eq!(seasons_of(&again), read, "{again}");
    for (n, id) in read {
        let (_, _, season) = h
            .send("GET", &format!("/3/tv/1396/season/{n}"), None, &[])
            .await;
        assert_eq!(season["id"], id, "{season}");
    }
}

/// With no model, a subscription read first lists items the faker made; an item created
/// for it replaces them, in its `items` and in its item list.
#[tokio::test]
async fn an_item_created_replaces_the_filler_items() {
    let h = support::harness(
        STRIPE,
        Vec::new(),
        &HarnessOptions {
            offline: true,
            ..HarnessOptions::default()
        },
        |rt| rt,
    );
    let (status, _, sub) = h.send("GET", "/v1/subscriptions/sub_1", None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{sub}");
    assert!(!ids(&sub["items"]).is_empty(), "{sub}");
    let (status, _, created) = h
        .send(
            "POST",
            "/v1/subscription_items",
            Some(json!({"subscription": "sub_1", "quantity": 7})),
            &[],
        )
        .await;
    assert!(status.is_success(), "{status} {created}");
    let new_id = created["id"].as_str().unwrap_or_default().to_string();
    let (_, _, sub) = h.send("GET", "/v1/subscriptions/sub_1", None, &[]).await;
    assert_eq!(ids(&sub["items"]), [new_id.as_str()], "{sub}");
    let (_, _, list) = h
        .send(
            "GET",
            "/v1/subscription_items?subscription=sub_1",
            None,
            &[],
        )
        .await;
    assert_eq!(ids(&list), [new_id.as_str()], "{list}");
}
