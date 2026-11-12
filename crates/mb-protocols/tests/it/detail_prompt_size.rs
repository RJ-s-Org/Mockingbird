//! A single-record read's prompt stays bounded however much the session holds: a read
//! in another language is shown its record's text without the parts it does not ask to
//! be embedded, a read generating parts it asks to be embedded is shown its record
//! without the parts it does not ask for, nested values projected, and a first read is
//! shown the records that refer to it without the parts their reads embedded. The
//! answer keeps the stored values. Driven over the TMDB example's contract with a
//! scripted LLM.
#![cfg(feature = "http-rest")]

use serde_json::{Value, json};

use crate::list_prompt_size::{
    block, last_prompt, last_prompt_bytes, movie_details, movie_ids, related, tv_details, tv_ids,
};
use crate::support::{self, HarnessOptions};

const TMDB: &str = include_str!("../../../../examples/apis/tmdb/tmdb.openapi.yaml");

/// The parts the held movies were read with.
const HELD_PARTS: &str = "append_to_response=credits,videos,images";

/// Every part a movie's read can ask for.
const ALL_PARTS: &str = "append_to_response=credits,videos,images,external_ids,release_dates";

/// The most bytes a single-record read's prompt takes with 30 held movies: about what
/// it took before every detail read stored its record whole (the system message, with
/// the response schema, is some 8 KB of it), with room.
const MAX_DETAIL_PROMPT_BYTES: usize = 12 * 1024;

/// Movie `id`'s details in `language` with only the parts of [`HELD_PARTS`].
fn held_movie(id: u64, language: &str) -> Value {
    let mut movie = movie_details(id, language);
    if let Value::Object(members) = &mut movie {
        members.remove("external_ids");
        members.remove("release_dates");
    }
    movie
}

/// The `<existing_resource>` of `prompt`.
fn existing(prompt: &str) -> Value {
    serde_json::from_str(block(prompt, "existing_resource").unwrap_or("null")).unwrap()
}

/// Reads 30 movies with three parts appended, ten of them in French too (a variant
/// each), and ten shows with every part; the script answers `then` after that.
async fn held(then: Vec<Value>) -> support::DescribedHarness {
    let ids = movie_ids();
    let french: Vec<u64> = ids.iter().copied().take(10).collect();
    let mut script: Vec<Value> = ids.iter().map(|id| held_movie(*id, "en-US")).collect();
    script.extend(french.iter().map(|id| held_movie(*id, "fr-FR")));
    script.extend(tv_ids().iter().map(|id| tv_details(*id, "en-US")));
    script.extend(then);
    let h = support::harness(TMDB, script, &HarnessOptions::default(), |rt| rt);
    for id in &ids {
        let uri = format!("/3/movie/{id}?language=en-US&{HELD_PARTS}");
        let (status, _, body) = h.send("GET", &uri, None, &[]).await;
        assert!(status.is_success(), "{body}");
    }
    for id in &french {
        let uri = format!("/3/movie/{id}?language=fr-FR&{HELD_PARTS}");
        let (status, _, body) = h.send("GET", &uri, None, &[]).await;
        assert!(status.is_success(), "{body}");
        assert_eq!(body["title"], format!("Movie {id} (fr-FR)"));
    }
    for id in tv_ids() {
        let uri = format!(
            "/3/tv/{id}?language=en-US&append_to_response=credits,videos,images,external_ids,content_ratings"
        );
        let (status, _, body) = h.send("GET", &uri, None, &[]).await;
        assert!(status.is_success(), "{body}");
    }
    assert_eq!(
        h.provider.requests().len(),
        ids.len() + french.len() + tv_ids().len()
    );
    h
}

/// Sends `uri`, which must generate once, and answers its body.
async fn generated(h: &support::DescribedHarness, uri: &str) -> Value {
    let before = h.provider.requests().len();
    let (status, _, body) = h.send("GET", uri, None, &[]).await;
    assert!(status.is_success(), "{uri}: {body}");
    assert_eq!(h.provider.requests().len(), before + 1, "{uri}");
    body
}

/// A read in another language is shown its record's text, without the parts it does
/// not ask to be embedded or the record's other languages, under a byte bound; the
/// answer keeps the stored values, and those parts keep theirs when a later read in
/// that language asks for them.
#[tokio::test]
async fn a_variant_read_s_prompt_stays_bounded() {
    // The model restates the movie in German, and makes up values the prompt left out.
    let mut german = movie_details(1001, "de-DE");
    german["budget"] = json!(1);
    german["poster_path"] = json!("/madeup.jpg");
    german["credits"]["cast"][0]["character"] = json!("Rolle 0");
    let h = held(vec![german]).await;
    let body = generated(&h, "/3/movie/1001?language=de-DE").await;
    let prompt = last_prompt(&h.provider);
    let bytes = last_prompt_bytes(&h.provider);
    assert!(bytes < MAX_DETAIL_PROMPT_BYTES, "{bytes} bytes: {prompt}");
    let shown = existing(&prompt);
    assert_eq!(shown["id"], 1001, "{shown}");
    assert_eq!(shown["title"], "Movie 1001 (en-US)", "{shown}");
    assert!(shown["overview"].as_str().is_some(), "{shown}");
    for left_out in ["credits", "videos", "images", "genre_ids"] {
        assert!(shown.get(left_out).is_none(), "{left_out}: {shown}");
    }
    assert!(!prompt.contains("fr-FR)"), "{prompt}");

    // Its text is the answer's, every other value as stored.
    let stored = held_movie(1001, "en-US");
    assert_eq!(body["title"], "Movie 1001 (de-DE)", "{body}");
    assert_eq!(body["budget"], stored["budget"], "{body}");
    assert_eq!(body["poster_path"], stored["poster_path"], "{body}");
    assert!(body.get("credits").is_none(), "{body}");

    // The German read asking for the credits is answered from what is kept, with the
    // stored credits, not those the model made up for a part it was not shown.
    let before = h.provider.requests().len();
    let (status, _, again) = h
        .send(
            "GET",
            &format!("/3/movie/1001?language=de-DE&{HELD_PARTS}"),
            None,
            &[],
        )
        .await;
    assert!(status.is_success(), "{again}");
    assert_eq!(h.provider.requests().len(), before, "{again}");
    assert_eq!(again["title"], "Movie 1001 (de-DE)", "{again}");
    assert_eq!(again["credits"], stored["credits"], "{again}");
}

/// A model answering a read in another language in the shape of what it was shown (its
/// text and the values beside it) gives a valid answer at once: no repair is asked for,
/// and the answer is its text over the stored values.
#[tokio::test]
async fn a_variant_answered_as_shown_needs_no_repair() {
    let mut german = held_movie(1004, "de-DE");
    if let Value::Object(members) = &mut german {
        for part in ["credits", "videos", "images", "origin_country"] {
            members.remove(part);
        }
    }
    let h = held(vec![german]).await;
    let body = generated(&h, "/3/movie/1004?language=de-DE").await;
    let stored = held_movie(1004, "en-US");
    assert_eq!(body["title"], "Movie 1004 (de-DE)", "{body}");
    assert_eq!(body["origin_country"], stored["origin_country"], "{body}");
    assert_eq!(body["budget"], stored["budget"], "{body}");
}

/// A read generating the parts it asks to be embedded is shown its record, long text
/// cut and nested values kept, without the parts it generates, under a byte bound; the
/// answer keeps every stored value and the parts held, and takes the new parts from the
/// model.
#[tokio::test]
async fn an_embed_read_s_prompt_stays_bounded() {
    let mut answer = movie_details(1003, "en-US");
    answer["title"] = json!("Made up");
    answer["budget"] = json!(1);
    answer["credits"]["cast"][0]["character"] = json!("Made up");
    answer["external_ids"]["imdb_id"] = json!("tt7654321");
    let h = held(vec![answer]).await;
    let body = generated(&h, &format!("/3/movie/1003?language=en-US&{ALL_PARTS}")).await;
    let prompt = last_prompt(&h.provider);
    let bytes = last_prompt_bytes(&h.provider);
    assert!(bytes < MAX_DETAIL_PROMPT_BYTES, "{bytes} bytes: {prompt}");
    let shown = existing(&prompt);
    assert_eq!(shown["id"], 1003, "{shown}");
    assert_eq!(shown["title"], "Movie 1003 (en-US)", "{shown}");
    let overview = shown["overview"].as_str().unwrap();
    assert!(overview.ends_with('…'), "{overview}");
    // Its nested values and the held parts it asks for stay, long lists cut; the parts
    // it generates are not shown.
    assert_eq!(
        shown["genres"],
        held_movie(1003, "en-US")["genres"],
        "{shown}"
    );
    for held_part in ["credits", "videos", "images"] {
        assert!(shown.get(held_part).is_some(), "{held_part}: {shown}");
    }
    let cast = shown["credits"]["cast"].as_array().unwrap();
    assert!(cast.len() < 10, "{shown}");
    assert!(cast.last().is_some_and(Value::is_string), "{shown}");
    for left_out in ["external_ids", "release_dates"] {
        assert!(shown.get(left_out).is_none(), "{left_out}: {shown}");
    }

    // Stored values and held parts stand; the new parts are the model's.
    let stored = held_movie(1003, "en-US");
    for member in ["title", "overview", "budget", "genres", "credits", "images"] {
        assert_eq!(body[member], stored[member], "{member}: {body}");
    }
    assert_eq!(body["external_ids"]["imdb_id"], "tt7654321", "{body}");
    assert!(body.get("release_dates").is_some(), "{body}");
}

/// An embed read of a show is shown its nested values (seasons, genres, networks,
/// creators), so the parts it generates line up with them, but not the parts embedded
/// before that it does not ask for, under a byte bound; the answer keeps the stored
/// seasons.
#[tokio::test]
async fn an_embed_read_is_shown_its_record_s_nested_values() {
    const SHOW: u64 = 6001;
    // The show was read with three parts; a later read asks for its credits.
    let mut first = tv_details(SHOW, "en-US");
    let mut answer = first.clone();
    if let Value::Object(members) = &mut first {
        members.remove("credits");
        members.remove("content_ratings");
    }
    answer["seasons"][0]["name"] = json!("Made up");
    answer["credits"]["cast"][0]["character"] = json!("Made up role");
    let h = held(vec![first.clone(), answer]).await;
    let (status, _, body) = h
        .send(
            "GET",
            &format!("/3/tv/{SHOW}?language=en-US&append_to_response=videos,images,external_ids"),
            None,
            &[],
        )
        .await;
    assert!(status.is_success(), "{body}");
    let body = generated(
        &h,
        &format!("/3/tv/{SHOW}?language=en-US&append_to_response=credits"),
    )
    .await;
    let prompt = last_prompt(&h.provider);
    let bytes = last_prompt_bytes(&h.provider);
    assert!(bytes < MAX_DETAIL_PROMPT_BYTES, "{bytes} bytes: {prompt}");
    let shown = existing(&prompt);
    assert_eq!(shown["id"], SHOW, "{shown}");
    for member in ["seasons", "genres", "networks", "created_by"] {
        assert_eq!(shown[member], first[member], "{member}: {shown}");
    }
    for left_out in [
        "videos",
        "images",
        "external_ids",
        "credits",
        "content_ratings",
    ] {
        assert!(shown.get(left_out).is_none(), "{left_out}: {shown}");
    }

    // The stored seasons stand; the credits are the model's.
    assert_eq!(body["seasons"], first["seasons"], "{body}");
    assert!(body.get("images").is_none(), "{body}");
    assert_eq!(
        body["credits"]["cast"][0]["character"], "Made up role",
        "{body}"
    );
}

/// Posts by authors; a post's read can embed its comments.
const BLOG: &str = r##"
openapi: 3.0.0
info: {title: Blog, version: "1"}
paths:
  /posts/{post_id}:
    get:
      operationId: getPost
      parameters:
        - {name: post_id, in: path, required: true, schema: {type: string}}
        - {name: embed, in: query, schema: {type: string}, description: "Comma-separated: comments"}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema: {$ref: "#/components/schemas/Post"}
  /authors/{author_id}:
    get:
      operationId: getAuthor
      parameters:
        - {name: author_id, in: path, required: true, schema: {type: string}}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema: {$ref: "#/components/schemas/Author"}
components:
  schemas:
    Author:
      type: object
      required: [id, name]
      properties:
        id: {type: string}
        name: {type: string}
        bio: {type: string}
    Comment:
      type: object
      required: [id, body]
      properties:
        id: {type: string}
        body: {type: string}
    Post:
      type: object
      required: [id, title, author_id]
      properties:
        id: {type: string}
        title: {type: string}
        body: {type: string}
        author_id: {type: string}
        tags: {type: array, items: {type: string}}
        comments: {type: array, items: {$ref: "#/components/schemas/Comment"}}
"##;

/// Post `n` by author `a1`, with twenty comments and `tags` tags.
fn post(n: u64, tags: usize) -> Value {
    let comments: Vec<Value> = (0..20)
        .map(|c| {
            json!({"id": format!("c{n}-{c}"),
                   "body": "A comment that says something. ".repeat(4)})
        })
        .collect();
    let tags: Vec<String> = (0..tags).map(|t| format!("tag-{n}-{t}")).collect();
    json!({"id": format!("p{n}"), "title": format!("Post {n}"), "author_id": "a1",
           "body": "A post's body. ".repeat(40), "tags": tags, "comments": comments})
}

/// The most bytes a first read's prompt takes with 30 held posts referring to it: the
/// related records' 24 KiB, with room.
const MAX_FIRST_READ_PROMPT_BYTES: usize = 26 * 1024;

/// Reads 30 posts of `contract` by `/posts/{id}?{query}`, each with `tags` tags, then
/// author `a1`, which the session does not hold: the related records its prompt shows.
async fn first_read(contract: &str, query: &str, tags: usize) -> Vec<Value> {
    let mut script: Vec<Value> = (1..=30).map(|n| post(n, tags)).collect();
    script.push(json!({"id": "a1", "name": "Ada", "bio": "Writes."}));
    let h = support::harness(contract, script, &HarnessOptions::default(), |rt| rt);
    for n in 1..=30 {
        let (status, _, body) = h
            .send("GET", &format!("/posts/p{n}?{query}"), None, &[])
            .await;
        assert!(status.is_success(), "{body}");
        assert_eq!(body["comments"].as_array().map(Vec::len), Some(20));
    }
    let body = generated(&h, "/authors/a1").await;
    assert_eq!(body["name"], "Ada", "{body}");
    let prompt = last_prompt(&h.provider);
    let bytes = last_prompt_bytes(&h.provider);
    assert!(
        bytes < MAX_FIRST_READ_PROMPT_BYTES,
        "{bytes} bytes: {prompt}"
    );
    let shown = related(&prompt);
    assert!(!shown.is_empty(), "{prompt}");
    for post in &shown {
        assert_eq!(post["author_id"], "a1", "{post}");
        assert!(post.get("comments").is_none(), "{post}");
    }
    shown
}

/// A first read of an author the session does not hold is shown the posts that refer to
/// it, without the comments their reads embedded (a part the embedding parameter's
/// description names), under a byte bound.
#[tokio::test]
async fn a_first_read_is_shown_its_referrers_without_their_embeds() {
    let shown = first_read(BLOG, "embed=comments", 0).await;
    assert_eq!(shown.len(), 20);
}

/// Referrers too long to be shown all are shown nearest first up to a byte bound; an
/// embedding parameter's enumerated values name the parts left out.
#[tokio::test]
async fn a_first_read_s_referrers_are_cut_at_a_byte_bound() {
    let contract = BLOG.replace(
        r#"{name: embed, in: query, schema: {type: string}, description: "Comma-separated: comments"}"#,
        "{name: include, in: query, schema: {type: string, enum: [comments]}}",
    );
    assert_ne!(contract, BLOG);
    let shown = first_read(&contract, "include=comments", 150).await;
    assert!((1..20).contains(&shown.len()), "{}", shown.len());
}
