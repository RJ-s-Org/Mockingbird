# Movie Details

Get the top level details of a movie by ID.

Operation id: `movie-details`

```
GET https://api.themoviedb.org/3/movie/{movie_id}
```

## Path parameters

| Name | Type | Description |
|------|------|-------------|
| movie_id | integer | Required. The TMDB id of the movie. |

## Response properties

| Name | Type | Description |
|------|------|-------------|
| id | integer | The movie id. |
| title | string | The title. |
| tagline | string | The tagline; empty when there is none. |
| homepage | string | The movie's website. |
| belongs_to_collection | object | The collection the movie is part of. May be null. |

## Examples

A movie with a homepage and no tagline:

```json
{
  "id": 550,
  "tagline": null,
  "homepage": "https://www.foxmovies.com/movies/fight-club"
}
```

A movie without a homepage:

```json
{
  "id": 551,
  "tagline": "Hell, upside down"
}
```
