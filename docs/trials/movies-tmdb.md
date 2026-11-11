# Trial: nuxt/movies (Nuxt, TMDB v3), holdout run (2026-11-11)

[nuxt/movies](https://github.com/nuxt/movies) at `215904c`: a Nuxt 3 browser for movies, TV shows and people. It reaches TMDB through a hosted proxy, `movies-proxy.vercel.app`: `/tmdb/<path>` forwards to `api.themoviedb.org/3/<path>`, and `/ipx/...` resizes images. This is a **holdout** run after the cart fix wave (#402, #403, #407, #418, #424), on `main` at `d0aeacd7`. It checks whether those fixes generalize. Nothing was fixed during the run.

| | |
| --- | --- |
| API mocked | TMDB v3, at the app's proxy host: `movies-proxy.vercel.app/tmdb/...` |
| Contract | The `tmdb` example (9 operations; TMDB's OpenAPI file can't be redistributed, so there is no `--full`), extended by the agent to the 24 operations the app calls |
| Model | OpenRouter `deepseek/deepseek-v4.1-flash-1017` (`api_key: env:OPENROUTER_API_KEY`) |
| App edits | None. Settings go in `mb-holdout/`; the browser tests use `playwright.mock.config.ts`, which extends the app's config with Mockingbird's proxy (docs/proxy.md#browsers). |

## How the mock was built (as the user's agent)

1. Ran `mockingbird examples init tmdb` and `mockingbird agent install claude-code`.
2. Extended `tmdb.openapi.yaml` with the operations the app calls that the example lacks:
   - movie lists: `top_rated`, `upcoming`, `now_playing`
   - TV lists: `popular`, `top_rated`, `airing_today`, `on_the_air`
   - `{movie,tv}/{id}/recommendations`, `tv/{id}/season/{n}`, `trending/{type}/{window}`
   - `discover/tv`, `genre/tv/list`, `person/{id}/combined_credits`, `search/multi`
   - the `append_to_response` members (`credits`, `videos`, `images`, `external_ids`, `release_dates`, `combined_credits`) on the details answers
3. Config: `route_prefix: /tmdb`, `path_rewrite: {add_prefix: /3}`, `hosts: [movies-proxy.vercel.app]` (the case `docs/configuration.md` describes), and `proxy.images: [image.tmdb.org, movies-proxy.vercel.app/ipx]`.
4. Ran `world show` (19 gaps), then wrote `tmdb.world.json`:
   - the 13 lists are lists of `movie` or `tv` (the heuristics typed them `popular`, `trending`, `recommendations`), paged by `page`, ignoring `language`
   - targets for `movie-credits` and `person-combined-credits`
   - a `query` text filter on `search-movie`

   4 gaps remain: sort and year filters, and `search-multi`'s `query`.
5. Relaxed the example's `language` pattern (see H6), then served everything with `mockingbird run`.

## What works

- **Pages.** Through `mockingbird run -- node .output/server/index.mjs`, every page answers 200 with plausible real data:
  - `/` (Toy Story hero, popular movies and TV)
  - `/movie`, `/tv`
  - `/movie/550` (Fight Club, with trailer, photos and storyline)
  - `/tv/1399` (Game of Thrones with cast and "More like this")
  - `/person/287` (Brad Pitt)
  - `/search?s=matrix` (the four Matrix films)
  - `/genre/28/movie`, `/movie/category/popular`
  - `tv/1399/season/1` answers "Winter Is Coming" first.
- **TV list → detail is consistent.** `/tv/1396` is Breaking Bad, as listed.
- **The app's Playwright suite (Chrome), browser through the mock: 3 of 4 pass.** The home sections, the link to the movie catalogue and the mobile layout pass.
- 0 fallbacks. Proxy mode needed no app edits; the hosted proxy's host and path are mapped by config alone.

## Failures

| # | Class | Failure | Repro: request → expected → got |
| --- | --- | --- | --- |
| H1 | Engine bug | The background record pool gives `movie.id` the **genre** id enum | On a fresh store, `GET /tmdb/movie/popular` → TMDB movie ids (Toy Story 862) → ids 80, 28, 27, 18, 10402 (`state`, 21 ms). The server log says `patched the answer locally … /records/0/id: enum: 603 is not one of 12, 14 or 17 other candidates` (`mb_engine::pool`). |
| H2 | Engine bug | Genre objects are stored as `movie` records, so a listed movie's detail page is another record | Home lists "Toy Story" at `/movie/80` → `/movie/80` shows Toy Story → it shows a movie titled "Comedy"; `/movie/28` shows "Crime". Probably the same type confusion as H1 (`genre-movie-list` and the `genres` items typed as `movie`). |
| H3 | Engine bug or inexpressible | A read in another `language` is served from state in the stored language | `GET /tmdb/tv/549?language=fr-FR` after an `en` read → a French record (generated or translated) → the English record, from `state` in 0 ms. The app's e2e test "switches the UI and TMDB content language" depends on this. |
| H4 | Model answer | `tv/549` is "Mr. Robot" | The same test expects TMDB's 549 (Law & Order, "New York Police Judiciaire" in French). Real-world id fidelity, but the app's own test depends on it. |
| H5 | Engine bug | An unknown id gets an invented 200, under another id | `GET /tmdb/person/99999999` → the declared `404` status body → `200`, `repaired`, an empty person with `"id": 5000000` (the schema's maximum, not the path's id). `movie/1300000` also answers 200 (`llm`). |
| H6 | Skills/MCP gap (example contract) | The `tmdb` example rejects the app's requests and covers 9 of the 24 operations it calls | `language: "^[a-z]{2}-[A-Z]{2}$"`, so the app's `language=en` (which TMDB accepts) → a 400 on every call until relaxed. The 15 missing operations had to be written by hand; there is no `--full` for TMDB. |
| H7 | Skills/MCP gap | List typing and filters take hand work | The heuristics type 13 lists as `popular`, `trending`, `recommendations`… instead of `movie`/`tv`. A `with_genres` filter on `genre_ids` is refused (`unresolved`) because list items carry members the detail record lacks (`genre_ids` vs `genres`). |
| H8 | Skills/MCP gap (minor) | A browser suite run under `mockingbird run` alone silently tests the real API | Before adding Playwright's `proxy` (documented in proxy.md and the testing skill), all 4 e2e tests "passed": the browser's client-side calls went to the real hosted proxy, and nothing warned. |

## Scorecard (measures 1–7)

| # | Measure | Target | Result |
| --- | --- | --- | --- |
| 1 | App's tests and main flows pass | 100% | ✗. All pages render. e2e 3/4 through the mock (H3/H4). Clicking a listed movie opens the wrong record (H2). |
| 2 | Consistency the flows rely on | 100% | ✗. TV list → detail holds; movie list → detail does not (H1/H2); language variants share one record (H3). |
| 3 | Agent build ≤ 15 min, skills/MCP/CLI only | ≤ 15 min | ✗, about 40 min: the 15 missing operations, the example's `language` pattern, list retyping, and the Playwright proxy (H6–H8). No Mockingbird internals were edited. |
| 4 | State/pool latency | p50 < 50 ms | ✓ p50 1 ms, p95 5 ms (n = 25) |
| 5 | Model latency | p50 < 2 s, p95 < 5 s | ✗ p50 3.6 s, p95 9.2 s, max 11.0 s (n = 16; mostly `tv/*` lists, details and recommendations) |
| 6 | Model spend per session | < $0.05 | ✓ ≈ $0.01: 74k prompt / 15k completion tokens on served calls at the listed `-1017` price, plus a few background pool calls the request log doesn't count |
| 7 | Errors: unknown ids, invalid input | declared status and body | ✗ An unknown id gets 200 (H5). A non-integer id gets a generic `400` problem+json, not TMDB's declared `status` body. |

**Do the fixes generalize?** The cart-wave fixes don't touch this app's paths (REST reads, no writes), so the holdout mostly shows new classes:

- the record pool's type confusion (H1/H2)
- per-language reads (H3)
- unknown-id refusals (H5)
- example-contract coverage (H6)

The proxy and path mapping, and state-first reads (p50 1 ms), carried over cleanly.
