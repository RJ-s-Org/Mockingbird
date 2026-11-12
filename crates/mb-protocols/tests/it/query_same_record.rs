//! A single-record read names its record by its path, whatever its query: the first
//! read stores the record under the path's id, and every later read of that id is
//! answered from it. The query only selects how the record is shown: its language (a
//! variant of the stored record), the members it embeds (built once, kept with the
//! record), or a projection that changes nothing about which record it is. A
//! collection's query still filters it.
#![cfg(feature = "http-rest")]

use serde_json::{Value, json};

use crate::support::{self, HarnessOptions};

/// Shaped as TMDB's `/tv/{tv_id}`: a language, `append_to_response` naming optional
/// members of the answer, and `include_image_language`.
const CONTRACT: &str = r##"
openapi: 3.0.0
info: {title: Shows, version: "1"}
paths:
  /tv:
    get:
      operationId: listShows
      parameters:
        - {name: status, in: query, schema: {type: string}}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                properties:
                  results: {type: array, items: {$ref: "#/components/schemas/ShowSummary"}}
  /search/tv:
    get:
      operationId: searchShows
      parameters:
        - {name: query, in: query, required: true, schema: {type: string}}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                properties:
                  results: {type: array, items: {$ref: "#/components/schemas/ShowSummary"}}
  /lookup:
    get:
      operationId: lookupShow
      parameters:
        - {name: id, in: query, required: true, schema: {type: integer}}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema: {$ref: "#/components/schemas/ShowSummary"}
  /tv/{tv_id}:
    get:
      operationId: getShow
      parameters:
        - {name: tv_id, in: path, required: true, schema: {type: integer}}
        - {name: language, in: query, schema: {type: string, pattern: "^[a-z]{2}(-[A-Z]{2})?$"}}
        - name: append_to_response
          in: query
          schema: {type: string}
          description: "Comma-separated: credits, videos, images"
        - {name: include_image_language, in: query, schema: {type: string}}
        - {name: sort_by, in: query, schema: {type: string, enum: [overview, first_air_date]}}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                allOf:
                  - $ref: "#/components/schemas/Show"
                  - $ref: "#/components/schemas/Appended"
components:
  schemas:
    ShowSummary:
      type: object
      required: [id, name]
      properties:
        id: {type: integer}
        name: {type: string}
        status: {type: string}
    Show:
      type: object
      required: [id, name]
      properties:
        id: {type: integer}
        name: {type: string}
        overview: {type: string}
        first_air_date: {type: string, format: date}
        number_of_seasons: {type: integer}
        status: {type: string, enum: [Ended, Returning Series]}
    Appended:
      type: object
      properties:
        credits:
          type: object
          properties:
            cast:
              type: array
              items:
                type: object
                properties:
                  id: {type: integer}
                  name: {type: string}
        videos:
          type: object
          properties:
            results:
              type: array
              items:
                type: object
                properties:
                  key: {type: string}
                  site: {type: string}
        images:
          type: object
          properties:
            posters: {type: array, items: {type: object, properties: {file_path: {type: string}}}}
"##;

fn options() -> HarnessOptions {
    HarnessOptions::default()
}

/// The LLM calls made so far.
fn calls(h: &support::DescribedHarness) -> usize {
    h.provider.requests().len()
}

/// What does not change with the language: the show's identity and facts.
fn facts(show: &Value) -> Value {
    json!({
        "id": show["id"],
        "first_air_date": show["first_air_date"],
        "number_of_seasons": show["number_of_seasons"],
        "status": show["status"],
    })
}

/// `fr-FR`, then `en-US`, then no query, then `append_to_response=credits`: one show.
/// Each language has its own text, and every read keeps the stored show's facts.
#[tokio::test]
async fn every_query_reads_the_one_record_its_path_names() {
    let h = support::harness(
        CONTRACT,
        vec![
            json!({"id": 549, "name": "Dr House", "overview": "Un médecin de génie.",
                   "first_air_date": "2004-11-16", "number_of_seasons": 8, "status": "Ended"}),
            // In English: another show's facts, which the stored show's replace.
            json!({"id": 1396, "name": "House", "overview": "A brilliant doctor.",
                   "first_air_date": "2008-01-20", "number_of_seasons": 5,
                   "status": "Returning Series"}),
            json!({"id": 549, "name": "House M.D.", "overview": "A doctor.",
                   "first_air_date": "2004-11-16", "number_of_seasons": 8, "status": "Ended"}),
            // The credits asked for, over the stored show.
            json!({"id": 4607, "name": "Law & Order", "number_of_seasons": 20,
                   "credits": {"cast": [{"id": 41419, "name": "Hugh Laurie"}]}}),
        ],
        &options(),
        |rt| rt,
    );
    let (status, _, french) = h.send("GET", "/tv/549?language=fr-FR", None, &[]).await;
    assert!(status.is_success(), "{french}");
    assert_eq!(french["name"], "Dr House");
    let stored = facts(&french);
    assert_eq!(stored["id"], 549);

    let (_, _, english) = h.send("GET", "/tv/549?language=en-US", None, &[]).await;
    assert_eq!(english["name"], "House");
    assert_eq!(english["overview"], "A brilliant doctor.");
    assert_eq!(facts(&english), stored);

    let (_, _, plain) = h.send("GET", "/tv/549", None, &[]).await;
    assert_eq!(facts(&plain), stored);

    let (_, _, appended) = h
        .send("GET", "/tv/549?append_to_response=credits", None, &[])
        .await;
    assert_eq!(facts(&appended), stored);
    // In the language it asked for (none: the API's default), as the plain read was.
    assert_eq!(appended["name"], plain["name"]);
    assert_eq!(appended["credits"]["cast"][0]["name"], "Hugh Laurie");
    assert_eq!(calls(&h), 4);

    // Each again: the same answer, with no further generation.
    let (_, _, again) = h.send("GET", "/tv/549?language=fr-FR", None, &[]).await;
    assert_eq!(again, french);
    let (_, _, again) = h.send("GET", "/tv/549?language=en-US", None, &[]).await;
    assert_eq!(again, english);
    let (_, _, again) = h.send("GET", "/tv/549", None, &[]).await;
    assert_eq!(again, plain);
    let (_, _, again) = h
        .send("GET", "/tv/549?append_to_response=credits", None, &[])
        .await;
    assert_eq!(again, appended);
    // The kept credits, in a language's variant too.
    let (_, _, english_credits) = h
        .send(
            "GET",
            "/tv/549?language=en-US&append_to_response=credits",
            None,
            &[],
        )
        .await;
    assert_eq!(english_credits["name"], "House");
    assert_eq!(english_credits["credits"], appended["credits"]);
    assert_eq!(calls(&h), 4);
}

/// What a read embeds is built once and kept with the record: a later read asking for
/// it is answered the kept part, and one that does not ask leaves it out.
#[tokio::test]
async fn an_embed_is_stored_and_reused() {
    let h = support::harness(
        CONTRACT,
        vec![
            json!({"id": 549, "name": "House", "number_of_seasons": 8,
                   "credits": {"cast": [{"id": 41419, "name": "Hugh Laurie"}]},
                   "videos": {"results": [{"key": "abc123", "site": "YouTube"}]}}),
            json!({"id": 549, "name": "a second answer"}),
        ],
        &options(),
        |rt| rt,
    );
    let (_, _, first) = h
        .send(
            "GET",
            "/tv/549?append_to_response=credits,videos",
            None,
            &[],
        )
        .await;
    assert_eq!(first["credits"]["cast"][0]["name"], "Hugh Laurie");
    assert_eq!(first["videos"]["results"][0]["key"], "abc123");

    let (_, _, bare) = h.send("GET", "/tv/549", None, &[]).await;
    assert_eq!(bare["name"], "House");
    assert!(bare.get("credits").is_none(), "{bare}");
    assert!(bare.get("videos").is_none(), "{bare}");

    let (_, _, videos) = h
        .send("GET", "/tv/549?append_to_response=videos", None, &[])
        .await;
    assert_eq!(videos["videos"], first["videos"]);
    assert!(videos.get("credits").is_none(), "{videos}");
    assert_eq!(calls(&h), 1);
}

/// A parameter that neither selects a language nor embeds anything is no other record.
#[tokio::test]
async fn a_projection_param_does_not_change_the_record() {
    let h = support::harness(
        CONTRACT,
        vec![
            json!({"id": 549, "name": "House", "number_of_seasons": 8}),
            json!({"id": 549, "name": "a second answer", "number_of_seasons": 3}),
        ],
        &options(),
        |rt| rt,
    );
    let (_, _, first) = h
        .send("GET", "/tv/549?include_image_language=en,null", None, &[])
        .await;
    assert_eq!(first["name"], "House");
    let (_, _, other) = h
        .send("GET", "/tv/549?include_image_language=fr", None, &[])
        .await;
    assert_eq!(other, first);
    // An enum naming members that are no sub-resources embeds nothing: it leaves them in.
    let (_, _, sorted) = h.send("GET", "/tv/549?sort_by=overview", None, &[]).await;
    assert_eq!(sorted, first);
    let (_, _, plain) = h.send("GET", "/tv/549", None, &[]).await;
    assert_eq!(plain, first);
    assert_eq!(calls(&h), 1);
}

/// A read in a language with no variant yet that asks for a part the record lacks:
/// answered in its language, over the stored record, and the part is kept with it.
#[tokio::test]
async fn a_new_language_can_ask_for_a_new_part() {
    let h = support::harness(
        CONTRACT,
        vec![
            json!({"id": 549, "name": "Dr House", "overview": "Un médecin de génie.",
                   "first_air_date": "2004-11-16", "number_of_seasons": 8, "status": "Ended"}),
            json!({"id": 1396, "name": "House", "overview": "A brilliant doctor.",
                   "first_air_date": "2008-01-20", "number_of_seasons": 5,
                   "credits": {"cast": [{"id": 41419, "name": "Hugh Laurie"}]}}),
        ],
        &options(),
        |rt| rt,
    );
    let (_, _, french) = h.send("GET", "/tv/549?language=fr-FR", None, &[]).await;
    let stored = facts(&french);
    let (_, _, english) = h
        .send(
            "GET",
            "/tv/549?language=en-US&append_to_response=credits",
            None,
            &[],
        )
        .await;
    assert_eq!(english["name"], "House");
    assert_eq!(facts(&english), stored);
    assert_eq!(english["credits"]["cast"][0]["name"], "Hugh Laurie");
    // That language's variant, without the part it did not ask for.
    let (_, _, plain_english) = h.send("GET", "/tv/549?language=en-US", None, &[]).await;
    assert_eq!(plain_english["name"], "House");
    assert!(plain_english.get("credits").is_none(), "{plain_english}");
    // The part is the record's: the stored language has it too.
    let (_, _, french_credits) = h
        .send(
            "GET",
            "/tv/549?language=fr-FR&append_to_response=credits",
            None,
            &[],
        )
        .await;
    assert_eq!(french_credits["name"], "Dr House");
    assert_eq!(french_credits["credits"], english["credits"]);
    assert_eq!(calls(&h), 2);
}

/// A part asked of a record held in the read's own language is generated over it: the
/// stored values stand.
#[tokio::test]
async fn a_part_is_generated_over_the_held_record() {
    let h = support::harness(
        CONTRACT,
        vec![
            json!({"id": 549, "name": "House", "overview": "A doctor.",
                   "first_air_date": "2004-11-16", "number_of_seasons": 8}),
            json!({"id": 4607, "name": "Law & Order", "overview": "Detectives.",
                   "number_of_seasons": 20,
                   "credits": {"cast": [{"id": 41419, "name": "Hugh Laurie"}]}}),
        ],
        &options(),
        |rt| rt,
    );
    let (_, _, plain) = h.send("GET", "/tv/549", None, &[]).await;
    let (_, _, appended) = h
        .send("GET", "/tv/549?append_to_response=credits", None, &[])
        .await;
    assert_eq!(appended["id"], 549);
    assert_eq!(appended["name"], plain["name"]);
    assert_eq!(appended["overview"], plain["overview"]);
    assert_eq!(appended["number_of_seasons"], 8);
    assert_eq!(appended["credits"]["cast"][0]["name"], "Hugh Laurie");
    let (_, _, again) = h.send("GET", "/tv/549", None, &[]).await;
    assert_eq!(again, plain);
    assert_eq!(calls(&h), 2);
}

/// A search, and a read naming its record by a query value rather than its path, are no
/// single-record reads by path: each query is its own.
#[tokio::test]
async fn searches_and_query_named_reads_keep_their_query() {
    let h = support::harness(
        CONTRACT,
        vec![
            json!({"results": [{"id": 1, "name": "House"}]}),
            json!({"results": [{"id": 2, "name": "Law & Order"}]}),
            json!({"id": 549, "name": "House"}),
            json!({"id": 550, "name": "Fight Club"}),
        ],
        &options(),
        |rt| rt,
    );
    let (_, _, house) = h.send("GET", "/search/tv?query=house", None, &[]).await;
    let (_, _, law) = h.send("GET", "/search/tv?query=law", None, &[]).await;
    assert_eq!(house["results"][0]["name"], "House");
    assert_eq!(law["results"][0]["name"], "Law & Order");
    let (_, _, first) = h.send("GET", "/lookup?id=549", None, &[]).await;
    let (_, _, second) = h.send("GET", "/lookup?id=550", None, &[]).await;
    assert_eq!(first["name"], "House");
    assert_eq!(second["name"], "Fight Club");
    assert_eq!(calls(&h), 4);
}

/// A collection's query is its own: its filter still narrows what it lists.
#[tokio::test]
async fn a_collection_reads_filters_still_apply() {
    let h = support::harness(
        CONTRACT,
        vec![json!({"results": [
            {"id": 1, "name": "Deadwood", "status": "Ended"},
            {"id": 2, "name": "Severance", "status": "Returning Series"},
        ]})],
        &options(),
        |rt| rt,
    );
    let (status, _, page) = h.send("GET", "/tv?status=Ended", None, &[]).await;
    assert!(status.is_success(), "{page}");
    let results = page["results"].as_array().cloned().unwrap_or_default();
    assert!(!results.is_empty(), "{page}");
    assert!(
        results.iter().all(|show| show["status"] == "Ended"),
        "{page}"
    );
}
