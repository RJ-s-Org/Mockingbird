# Spotify

A mock of the Spotify Web API for "Plattenkiste", a Hamburg record-shop app: search,
tracks, artists and their top tracks, the current user and what they are playing,
playlists and adding items to them. A second mock answers the token endpoint on
`accounts.spotify.com`, so an SDK's client-credentials or authorization-code flow
works end to end.

| | |
| --- | --- |
| Contracts | [`spotify.openapi.yaml`](spotify.openapi.yaml) (`api.spotify.com`) and [`spotify-accounts.openapi.yaml`](spotify-accounts.openapi.yaml) (`accounts.spotify.com`), hand-written subsets that keep Spotify's operation ids |
| Upstream | None pinned: Spotify's [OpenAPI file](https://developer.spotify.com/reference/web-api/open-api-schema.yaml) carries no licence (see `upstream.notes` in [`example.json`](example.json)) |
| Hosts | `api.spotify.com`, `accounts.spotify.com`; album art on `i.scdn.co` answers placeholder PNGs |
| Scenarios | `spotify`: `token-expired` (401), `nothing-playing` (204), `not-found` (404), `rate-limited` (429), `outage` (503); `spotify-accounts`: `invalid-client` (401) |
| Webhooks | None (Spotify sends none) |

## Quickstart

```bash
mockingbird examples init spotify --dir spotify && cd spotify   # or: cd examples/apis/spotify
mockingbird serve -c mockingbird.yaml --offline
```

In another shell:

```bash
curl -s 'localhost:8080/v1/search?q=nordlicht&type=track&limit=2' -H 'Host: api.spotify.com' \
  -H 'Authorization: Bearer BQtest'
```

The [`spotipy`](https://pypi.org/project/spotipy/) client, unmodified, through the proxy
(Spotify has no official Python SDK; spotipy is the one its docs point to). It fetches a
token from `accounts.spotify.com/api/token` first, which the second mock answers:

```bash
eval "$(mockingbird proxy env)"
pip install spotipy
python - <<'PY'
import spotipy
from spotipy.oauth2 import SpotifyClientCredentials
sp = spotipy.Spotify(auth_manager=SpotifyClientCredentials(
    client_id="spotify_test_id", client_secret="spotify_test_secret"))
for t in sp.search(q="nordlicht", type="track", limit=3)["tracks"]["items"]:
    print(t["uri"], t["name"], "by", t["artists"][0]["name"])
print(sp.playlist_add_items("3cEYpjA9oz9GiPac4AsH4n", ["4uLU6hMCjMI75M1A2tKUQC"]))
PY
```

Drop `--offline` (and configure an LLM, see [the configuration reference](../../../docs/configuration.md#llm))
for answers that follow the persona: German indie artists matching `q`, one id shared by
`id`, `uri`, `href` and `external_urls`, and Mira Okafor's profile on `/v1/me`.

## Expected output

`--offline` answers come from the schema faker, so values differ run to run and the id
inside `uri` is not the `id`; the shape does not:

```json
{
  "tracks": {
    "href": "https://api.spotify.com/v1/search?offset=0&limit=20",
    "limit": 11,
    "next": "https://api.spotify.com/v1/search?offset=20&limit=20",
    "offset": 0,
    "previous": null,
    "total": 90,
    "items": [
      {
        "id": "ySsfknM6y2FLBHiQLDCo8Z",
        "name": "iris",
        "type": "track",
        "uri": "spotify:track:XbParKYNXzoIfetp7ed7w8",
        "duration_ms": 120058,
        "artists": [{"id": "n1yyHxt2WTEc61zCXGkJC8", "name": "lumen", "type": "artist", ...}],
        "album": {"album_type": "single", "images": [{"url": "https://i.scdn.co/image/ab67616d0000b273...", "height": 640, "width": 640}], ...},
        ...
      }
    ]
  }
}
```

The Python snippet prints lines like:

```
spotify:track:banq1KaYEly8cAc1TK2rdi fjord by raven
{'snapshot_id': 'bMP/p3hgVeXNvepkgCmATQL/3I5lQvpG'}
```

## Scenarios

```bash
curl -s localhost:8080/v1/tracks/4uLU6hMCjMI75M1A2tKUQC -H 'Host: api.spotify.com' \
  -H 'Authorization: Bearer BQtest' -H 'X-Mockingbird-Scenario: token-expired'
# HTTP 401 {"error":{"message":"The access token expired","status":401}}
```

spotipy raises `spotipy.SpotifyException` with `http_status` 401 for it, and 404 for
`not-found` (`{"error":{"status":404,"message":"Resource not found"}}`).
`nothing-playing` makes `GET /v1/me/player/currently-playing` answer 204 with no body,
as Spotify does when no device is playing; `sp.current_user_playing_track()` returns `None`.

`rate-limited` answers 429 with Spotify's error body but without a `Retry-After` header,
so spotipy retries with its own backoff and then raises. For a 429 that carries
`Retry-After`, set [`limits.rate_limit`](../../../docs/configuration.md#mocksnamelimits)
on the mock instead.

The token mock has its own scenario: send `X-Mockingbird-Scenario: invalid-client` to
`POST /api/token` on `accounts.spotify.com` for `{"error":"invalid_client","error_description":"Invalid client secret"}`.
