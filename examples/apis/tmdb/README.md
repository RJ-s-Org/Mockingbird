# TMDB

A mock of The Movie Database (TMDB) API v3 for "Reelhouse", a movie-night app:
movie details and credits, popular movies, search, discover, genres, TV series,
people and the configuration endpoint on `api.themoviedb.org`, with placeholder
posters for `image.tmdb.org` so an app's images load too.

| | |
| --- | --- |
| Contract | [`tmdb.openapi.yaml`](tmdb.openapi.yaml), a hand-written subset (9 operations) whose operation ids are TMDB's reference slugs (`movie-details`, `search-movie`, ...) |
| Upstream | none: TMDB's OpenAPI file is under its API Terms of Use, not an open licence (see `notes` in [`example.json`](example.json)) |
| Hosts | `api.themoviedb.org`; `image.tmdb.org` through `proxy.images` |
| Scenarios | `rate-limited` (429), `not-found` (404), `invalid-api-key` (401), `outage` (503) |
| Webhooks | none (TMDB sends none) |

## Quickstart

```bash
mockingbird examples init tmdb --dir tmdb && cd tmdb   # or: cd examples/apis/tmdb
mockingbird serve -c mockingbird.yaml --offline
```

In another shell:

```bash
curl -s 'localhost:8080/3/movie/550?api_key=tmdb_test_123' -H 'Host: api.themoviedb.org'
```

[tmdbsimple](https://github.com/celiao/tmdbsimple) (TMDB has no official SDK; this is
the Python wrapper its docs list), unmodified, through the proxy, plus a poster from
`image.tmdb.org`:

```bash
eval "$(mockingbird proxy env)"
pip install tmdbsimple requests
python - <<'PY'
import requests, tmdbsimple as tmdb
tmdb.API_KEY = "tmdb_test_123"
movie = tmdb.Movies(550).info()
print(movie["id"], movie["title"], movie["release_date"], movie["runtime"], "min")
popular = tmdb.Movies().popular(page=1)
print(len(popular["results"]), "popular, page", popular["page"], "of", popular["total_pages"])
poster = requests.get("https://image.tmdb.org/t/p/w500" + movie["poster_path"])
print(poster.status_code, poster.headers["content-type"], len(poster.content), "bytes")
PY
```

Drop `--offline` (and configure an LLM, see [the configuration reference](../../../docs/configuration.md#llm))
for answers that follow the persona: real movies under their real ids (550 is Fight
Club), search results that match the query, overviews translated for `language`.

## Expected output

`--offline` answers come from the schema faker, so titles are placeholder words; ids
from the path, image paths, `imdb_id`, genres and dates keep TMDB's formats:

```json
{
  "id": 550,
  "imdb_id": "tt77059787",
  "title": "pebble",
  "original_language": "es",
  "release_date": "2025-09-15",
  "runtime": 83,
  "genres": [{ "id": 14, "name": "Crime" }],
  "poster_path": "/siHRk9eA4wDklHbVVeAZAXyouUl.jpg",
  "vote_average": 2.17,
  "status": "Post Production",
  ...
}
```

The Python snippet prints lines like:

```
550 pebble 2025-09-15 83 min
1 popular, page 1 of 26
200 image/png 6720 bytes
```

## Scenarios

```bash
curl -s 'localhost:8080/3/movie/550?api_key=tmdb_test_123' -H 'Host: api.themoviedb.org' \
  -H 'X-Mockingbird-Scenario: not-found'
# HTTP 404 {"success":false,"status_code":34,"status_message":"The resource you requested could not be found."}
```

Each scenario answers TMDB's own error body: `rate-limited` 429 with `status_code` 25,
`invalid-api-key` 401 with 7, `outage` 503 with 9. tmdbsimple raises
`requests.HTTPError` for all of them. To send the header from tmdbsimple, give it a
session: `s = requests.Session(); s.headers["X-Mockingbird-Scenario"] = "not-found"; tmdb.REQUESTS_SESSION = s`.
`not-found` applies to the by-id reads (`movie-details`, `movie-credits`,
`tv-series-details`, `person-details`).
