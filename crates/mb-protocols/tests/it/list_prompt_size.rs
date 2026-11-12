//! A list prompt stays bounded however many records the session holds: the held
//! records it shows are those that can be on the page, as the list's items declare
//! them, without the parts a detail read embedded, and the answer still keeps their
//! stored values. Driven over the TMDB example's contract with a scripted LLM.
#![cfg(feature = "http-rest")]

use serde_json::{Value, json};

use crate::support::{self, HarnessOptions};

const TMDB: &str = include_str!("../../../../examples/apis/tmdb/tmdb.openapi.yaml");

/// A TMDB-shaped image path: "/" + 27 alphanumerics + ".jpg".
fn image(seed: u64, n: u64) -> String {
    let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut x = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(n);
    let mut s = String::from("/");
    for _ in 0..27 {
        x = x
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        s.push(char::from(
            alphabet[usize::try_from((x >> 33) % 62).unwrap()],
        ));
    }
    s.push_str(".jpg");
    s
}

fn credit(id: u64, n: u64) -> String {
    format!("{:024x}", id * 1000 + n)
}

/// Movie `id`'s details in `language`, with every part `append_to_response` can ask for.
pub(crate) fn movie_details(id: u64, language: &str) -> Value {
    let title = format!("Movie {id} ({language})");
    let overview = format!(
        "{title}: a long overview that goes on about the plot, the cast and the making of \
the film, as TMDB's overviews do. {}",
        "It keeps going for a while. ".repeat(12)
    );
    let cast: Vec<Value> = (0..10)
        .map(|n| {
            json!({"adult": false, "gender": 2, "id": id * 100 + n,
                   "known_for_department": "Acting", "name": format!("Actor {n}"),
                   "original_name": format!("Actor {n}"), "popularity": 12.5,
                   "profile_path": image(id, n), "cast_id": n, "character": format!("Role {n}"),
                   "credit_id": credit(id, n), "order": n})
        })
        .collect();
    let crew: Vec<Value> = (0..6)
        .map(|n| {
            json!({"adult": false, "gender": 1, "id": id * 100 + 50 + n,
                   "known_for_department": "Directing", "name": format!("Crew {n}"),
                   "original_name": format!("Crew {n}"), "popularity": 3.5,
                   "profile_path": image(id, 50 + n), "credit_id": credit(id, 50 + n),
                   "department": "Directing", "job": "Director"})
        })
        .collect();
    let images: Vec<Value> = (0..6)
        .map(|n| {
            json!({"aspect_ratio": 1.778, "file_path": image(id, 80 + n),
                   "height": 1080, "width": 1920, "vote_average": 5.3})
        })
        .collect();
    json!({
        "adult": false, "backdrop_path": image(id, 1),
        "belongs_to_collection": null, "budget": 63_000_000,
        "genres": [{"id": 18, "name": "Drama"}, {"id": 53, "name": "Thriller"}],
        "homepage": "https://example.com/movie", "id": id, "imdb_id": "tt0137523",
        "origin_country": ["US"], "original_language": "en",
        "original_title": format!("Movie {id}"), "overview": overview, "popularity": 61.4,
        "poster_path": image(id, 2),
        "production_companies": [{"id": 508, "logo_path": null, "name": "Regency",
                                  "origin_country": "US"}],
        "release_date": "1999-10-15", "revenue": 100_853_753, "runtime": 139,
        "spoken_languages": [{"english_name": "English", "iso_639_1": "en", "name": "English"}],
        "status": "Released", "tagline": "Mischief. Mayhem. Soap.", "title": title,
        "video": false, "vote_average": 8.4, "vote_count": 30_000,
        "credits": {"cast": cast, "crew": crew},
        "videos": {"results": [{"id": "5c9294240e0a267cd516835f", "key": "BdJKm16Co6M",
                                "name": "Trailer", "site": "YouTube", "type": "Trailer",
                                "official": true}]},
        "images": {"backdrops": images, "posters": images},
        "external_ids": {"imdb_id": "tt0137523", "facebook_id": null,
                         "instagram_id": null, "twitter_id": null},
        "release_dates": {"results": [{"iso_3166_1": "US",
                                       "release_dates": [{"certification": "R",
                                                          "release_date": "1999-10-15"}]}]},
    })
}

/// TV show `id`'s details in `language`, with every part `append_to_response` can ask for.
pub(crate) fn tv_details(id: u64, language: &str) -> Value {
    let movie = movie_details(id, language);
    let seasons: Vec<Value> = (1..=4)
        .map(|n| {
            json!({"air_date": "2004-11-16", "episode_count": 22, "id": id * 10 + n,
                   "name": format!("Season {n}"),
                   "overview": "A season overview that runs for a sentence or two.",
                   "poster_path": image(id, 90 + n), "season_number": n,
                   "vote_average": 7.9})
        })
        .collect();
    json!({
        "adult": false, "backdrop_path": image(id, 1),
        "created_by": [{"id": 1, "credit_id": credit(id, 99), "name": "Creator",
                        "profile_path": null}],
        "episode_run_time": [44], "first_air_date": "2004-11-16",
        "genres": [{"id": 18, "name": "Drama"}], "homepage": "https://example.com/tv",
        "id": id, "in_production": false, "languages": ["en"], "last_air_date": "2012-05-21",
        "name": format!("Show {id} ({language})"),
        "networks": [{"id": 19, "logo_path": null, "name": "FOX", "origin_country": "US"}],
        "number_of_episodes": 177, "number_of_seasons": 4, "origin_country": ["US"],
        "original_language": "en", "original_name": format!("Show {id}"),
        "overview": movie["overview"], "popularity": 99.1, "poster_path": image(id, 2),
        "seasons": seasons, "status": "Ended", "tagline": "Everybody lies.", "type": "Scripted",
        "vote_average": 8.6, "vote_count": 6000,
        "credits": movie["credits"], "videos": movie["videos"], "images": movie["images"],
        "external_ids": movie["external_ids"],
        "content_ratings": {"results": [{"iso_3166_1": "US", "rating": "TV-14"}]},
    })
}

/// A page of movies, as the model would answer it: `ids` with made-up values, then
/// made-up others.
fn movie_page(ids: &[u64]) -> Value {
    let results: Vec<Value> = ids
        .iter()
        .chain(&[900_001, 900_002])
        .take(20)
        .map(|id| {
            json!({"adult": false, "backdrop_path": image(*id, 7), "genre_ids": [18],
                   "id": id, "original_language": "en",
                   "original_title": "Made up", "overview": "A made-up overview.",
                   "popularity": 10.0, "poster_path": image(*id, 8),
                   "release_date": "2001-01-01", "title": "Made up", "video": false,
                   "vote_average": 6.0, "vote_count": 100})
        })
        .collect();
    json!({"page": 1, "results": results, "total_pages": 5, "total_results": 100})
}

/// A page of TV shows: `ids` first, then made-up others.
fn tv_page(ids: &[u64]) -> Value {
    let results: Vec<Value> = ids
        .iter()
        .chain(&[200_001, 200_002])
        .take(20)
        .map(|id| {
            json!({"adult": false, "backdrop_path": image(*id, 1),
                   "first_air_date": "2001-01-01", "genre_ids": [18], "id": id,
                   "name": "Made up", "origin_country": ["US"], "original_language": "en",
                   "original_name": "Made up", "overview": "A made-up overview.",
                   "popularity": 10.0, "poster_path": image(*id, 2), "vote_average": 6.0,
                   "vote_count": 100})
        })
        .collect();
    json!({"page": 1, "results": results, "total_pages": 5, "total_results": 100})
}

/// A page of a multi search.
fn multi_page(ids: &[u64]) -> Value {
    let results: Vec<Value> = ids
        .iter()
        .map(|id| {
            json!({"id": id, "media_type": "movie", "adult": false, "title": "Made up",
                   "popularity": 10.0})
        })
        .collect();
    json!({"page": 1, "results": results, "total_pages": 1, "total_results": ids.len()})
}

/// The `<tag>` block of `prompt`.
pub(crate) fn block<'a>(prompt: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let start = prompt.find(&open)? + open.len();
    let end = prompt.find(&format!("</{tag}>"))?;
    Some(&prompt[start..end])
}

/// The bytes of every message of the last LLM request.
pub(crate) fn last_prompt_bytes(provider: &mb_llm::ScriptedProvider) -> usize {
    provider
        .requests()
        .last()
        .map(|r| r.messages.iter().map(|m| m.content.len()).sum())
        .unwrap_or_default()
}

/// The user message of the last LLM request.
pub(crate) fn last_prompt(provider: &mb_llm::ScriptedProvider) -> String {
    provider
        .requests()
        .last()
        .and_then(|r| r.messages.last())
        .map(|m| m.content.clone())
        .unwrap_or_default()
}

/// The held records `prompt` shows, one per line.
pub(crate) fn related(prompt: &str) -> Vec<Value> {
    block(prompt, "related_resources")
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

const HELD: u64 = 30;
const MOVIE_PARTS: &str = "append_to_response=credits,videos,images,external_ids,release_dates";
const TV_PARTS: &str = "append_to_response=credits,videos,images,external_ids,content_ratings";

pub(crate) fn movie_ids() -> Vec<u64> {
    (1..=HELD).map(|n| 1000 + n).collect()
}

pub(crate) fn tv_ids() -> Vec<u64> {
    (1..=10).map(|n| 5000 + n).collect()
}

/// Reads `HELD` movies' details and ten shows' with every part appended, and ten of the
/// movies in French too (a variant each); the script answers `lists` after that.
async fn held_records(lists: Vec<Value>) -> support::DescribedHarness {
    let ids = movie_ids();
    let french: Vec<u64> = ids.iter().copied().take(10).collect();
    let mut script: Vec<Value> = ids.iter().map(|id| movie_details(*id, "en-US")).collect();
    script.extend(french.iter().map(|id| movie_details(*id, "fr-FR")));
    script.extend(tv_ids().iter().map(|id| tv_details(*id, "en-US")));
    script.extend(lists);
    let h = support::harness(TMDB, script, &HarnessOptions::default(), |rt| rt);
    for id in &ids {
        let uri = format!("/3/movie/{id}?language=en-US&{MOVIE_PARTS}");
        let (status, _, body) = h.send("GET", &uri, None, &[]).await;
        assert!(status.is_success(), "{body}");
        assert_eq!(body["credits"]["cast"].as_array().map(Vec::len), Some(10));
    }
    for id in &french {
        let uri = format!("/3/movie/{id}?language=fr-FR&{MOVIE_PARTS}");
        let (status, _, body) = h.send("GET", &uri, None, &[]).await;
        assert!(status.is_success(), "{body}");
        assert_eq!(body["title"], format!("Movie {id} (fr-FR)"));
    }
    for id in tv_ids() {
        let uri = format!("/3/tv/{id}?language=en-US&{TV_PARTS}");
        let (status, _, body) = h.send("GET", &uri, None, &[]).await;
        assert!(status.is_success(), "{body}");
    }
    assert_eq!(h.stored_ids("movie").await.len(), ids.len());
    assert_eq!(
        h.provider.requests().len(),
        ids.len() + french.len() + tv_ids().len()
    );
    h
}

/// The most bytes a list prompt takes with 30 held movies: about what it took before
/// every detail read stored its record (some 3k tokens), with room.
const MAX_LIST_PROMPT_BYTES: usize = 24 * 1024;

/// With 30 held movies carrying embeds and variants, a list prompt stays under a byte
/// bound, still shows the held movies the page shows, as list items without the parts
/// a detail read embedded, and the answer keeps their stored values.
#[tokio::test]
async fn a_list_prompt_stays_bounded_with_many_held_records() {
    let ids = movie_ids();
    let h = held_records(vec![movie_page(&ids)]).await;
    let before = h.provider.requests().len();
    let (status, _, page) = h
        .send("GET", "/3/movie/popular?language=en-US&page=1", None, &[])
        .await;
    assert!(status.is_success(), "{page}");
    assert_eq!(h.provider.requests().len(), before + 1);
    let prompt = last_prompt(&h.provider);
    let bytes = last_prompt_bytes(&h.provider);
    assert!(bytes < MAX_LIST_PROMPT_BYTES, "{bytes} bytes: {prompt}");

    // As many held movies as a page shows, as list items: no embedded parts and no
    // detail-only members.
    let shown = related(&prompt);
    assert_eq!(shown.len(), 20, "{prompt}");
    for movie in &shown {
        let id = movie["id"].as_u64().unwrap();
        assert!(ids.contains(&id), "{movie}");
        assert_eq!(movie["title"], format!("Movie {id} (en-US)"));
        for detail in [
            "credits",
            "videos",
            "images",
            "release_dates",
            "budget",
            "tagline",
        ] {
            assert!(movie.get(detail).is_none(), "{detail}: {movie}");
        }
        // Its long overview is cut in the prompt.
        let overview = movie["overview"].as_str().unwrap();
        assert!(overview.ends_with('…'), "{overview}");
        assert_eq!(overview.chars().count(), 201, "{overview}");
    }

    // The answer keeps the stored values of the held movies it names, whole.
    let results = page["results"].as_array().unwrap();
    let held: Vec<&Value> = results
        .iter()
        .filter(|m| m["id"].as_u64().is_some_and(|id| ids.contains(&id)))
        .collect();
    assert_eq!(held.len(), 20, "{page}");
    for movie in held {
        let id = movie["id"].as_u64().unwrap();
        let stored = movie_details(id, "en-US");
        assert_eq!(movie["title"], stored["title"], "{movie}");
        assert_eq!(movie["overview"], stored["overview"], "{movie}");
        assert_eq!(movie["poster_path"], stored["poster_path"], "{movie}");
        assert!(movie.get("credits").is_none(), "{movie}");
    }
}

/// Each TMDB list's prompt stays under the bound with the held records (each printed).
#[tokio::test]
async fn every_tmdb_list_prompt_stays_bounded() {
    let ids = movie_ids();
    let h = held_records(vec![
        movie_page(&ids),
        tv_page(&tv_ids()),
        movie_page(&ids[..10]),
        multi_page(&ids[..5]),
    ])
    .await;
    for uri in [
        "/3/movie/popular?language=en-US&page=1",
        "/3/tv/popular?language=en-US&page=1",
        "/3/discover/movie?sort_by=popularity.desc&with_genres=18&language=en-US&page=1",
        "/3/search/multi?query=movie&language=en-US&page=1",
    ] {
        let (status, _, body) = h.send("GET", uri, None, &[]).await;
        assert!(status.is_success(), "{body}");
        let prompt = last_prompt(&h.provider);
        eprintln!(
            "{uri}: {} bytes, related {} bytes ({} records)",
            last_prompt_bytes(&h.provider),
            block(&prompt, "related_resources").map_or(0, str::len),
            related(&prompt).len()
        );
        assert!(
            last_prompt_bytes(&h.provider) < MAX_LIST_PROMPT_BYTES,
            "{uri}"
        );
    }
}

/// A list of items with a status filter and a page size.
const TASKS: &str = r##"
openapi: 3.0.0
info: {title: Tasks, version: "1"}
paths:
  /tasks:
    get:
      operationId: listTasks
      parameters:
        - {name: status, in: query, schema: {type: string, enum: [open, done]}}
        - {name: limit, in: query, schema: {type: integer, maximum: 50}}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                properties:
                  tasks: {type: array, maxItems: 50, items: {$ref: "#/components/schemas/TaskSummary"}}
  /projects/{project_id}/tasks:
    get:
      operationId: listProjectTasks
      parameters:
        - {name: project_id, in: path, required: true, schema: {type: string}}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema: {type: array, items: {$ref: "#/components/schemas/TaskSummary"}}
  /tasks/{task_id}:
    get:
      operationId: getTask
      parameters:
        - {name: task_id, in: path, required: true, schema: {type: string}}
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema: {$ref: "#/components/schemas/Task"}
components:
  schemas:
    TaskSummary:
      type: object
      required: [id, title, status]
      properties:
        id: {type: string}
        title: {type: string}
        status: {type: string, enum: [open, done]}
        project_id: {type: string}
    Task:
      type: object
      required: [id, title, status]
      properties:
        id: {type: string}
        title: {type: string}
        status: {type: string, enum: [open, done]}
        project_id: {type: string}
        notes: {type: string}
"##;

/// A held record the request's filter contradicts is not shown, a page size cuts what
/// is shown to one page, and each shown record holds only the items' members.
#[tokio::test]
async fn a_list_prompt_shows_only_what_its_page_can_hold() {
    let h = support::harness(
        TASKS,
        vec![json!({"tasks": [{"id": "t1", "title": "x", "status": "open"}]})],
        &HarnessOptions::default(),
        |rt| rt,
    );
    let tasks: Vec<Value> = (1..=12)
        .map(|n| {
            json!({"id": format!("t{n}"), "title": format!("Task {n}"),
                   "status": if n % 2 == 0 { "done" } else { "open" },
                   "notes": "long notes ".repeat(20)})
        })
        .collect();
    h.hold("tasks", &tasks).await;
    let (status, _, body) = h.send("GET", "/tasks?status=open&limit=4", None, &[]).await;
    assert!(status.is_success(), "{body}");
    let prompt = last_prompt(&h.provider);
    let shown = related(&prompt);
    assert_eq!(shown.len(), 4, "{prompt}");
    for task in &shown {
        assert_eq!(task["status"], "open", "{prompt}");
        assert!(task.get("notes").is_none(), "{task}");
    }
}

/// Twelve held tasks, odd ones open, two each of projects `p1` and `p2` in turn, each
/// with a title of `title_len` characters and long notes.
fn twelve_tasks(title_len: usize) -> Vec<Value> {
    (1..=12)
        .map(|n| {
            let title: String = format!("Task {n} ")
                .repeat(title_len)
                .chars()
                .take(title_len)
                .collect();
            json!({"id": format!("t{n}"), "title": title,
                   "status": if n % 2 == 0 { "done" } else { "open" },
                   "project_id": if n % 4 < 2 { "p1" } else { "p2" },
                   "notes": "long notes ".repeat(20)})
        })
        .collect()
}

/// A list under a parent path is not shown the held records of another parent.
#[tokio::test]
async fn a_list_prompt_shows_only_its_parent_s_records() {
    let h = support::harness(
        TASKS,
        vec![json!([{"id": "t1", "title": "x", "status": "open", "project_id": "p1"}])],
        &HarnessOptions::default(),
        |rt| rt,
    );
    let tasks = twelve_tasks(10);
    h.hold("tasks", &tasks).await;
    let (status, _, body) = h.send("GET", "/projects/p1/tasks", None, &[]).await;
    assert!(status.is_success(), "{body}");
    let prompt = last_prompt(&h.provider);
    let shown = related(&prompt);
    let of_p1 = tasks.iter().filter(|t| t["project_id"] == "p1").count();
    assert_eq!(shown.len(), of_p1, "{prompt}");
    assert!(shown.iter().all(|t| t["project_id"] == "p1"), "{prompt}");
}

/// A long text is cut in the prompt only where the answer's items are overlaid with the
/// stored records: not with consistency off.
#[tokio::test]
async fn a_long_text_is_cut_only_where_the_store_restores_it() {
    let tasks = twelve_tasks(300);
    for consistency in [true, false] {
        let h = support::harness(
            TASKS,
            vec![json!({"tasks": [{"id": "t1", "title": "x", "status": "open"}]})],
            &HarnessOptions::default(),
            |mut rt| {
                rt.consistency = consistency;
                rt
            },
        );
        h.hold("tasks", &tasks).await;
        let (status, _, body) = h.send("GET", "/tasks", None, &[]).await;
        assert!(status.is_success(), "{body}");
        let prompt = last_prompt(&h.provider);
        let shown = related(&prompt);
        assert!(!shown.is_empty(), "{prompt}");
        for task in &shown {
            let title = task["title"].as_str().unwrap();
            assert_eq!(title.ends_with('…'), consistency, "{title}");
            assert_eq!(title.chars().count() <= 201, consistency, "{title}");
        }
        if consistency {
            // The answer's item naming a held task serves its stored title whole.
            assert_eq!(body["tasks"][0]["title"], tasks[0]["title"], "{body}");
        }
    }
}
