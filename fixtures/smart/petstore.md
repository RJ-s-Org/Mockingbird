# Petstore API

A sample API that manages the pets of a pet store.
All responses are JSON. The base URL is `https://petstore.example.com/v1`.

## Pets

### List pets

GET /pets

Returns all pets, newest first.

Query parameters:

- `limit` (integer, optional): how many pets to return, at most 100.

Example response (`200 OK`):

```json
[
  {
    "id": 1,
    "name": "Rex",
    "tag": "dog"
  },
  {
    "id": 2,
    "name": "Tom",
    "tag": "cat"
  }
]
```

### Get a pet

GET /pets/{petId}

Returns the pet with the given id, or `404 Not Found` when there is none.

```json
{
  "id": 1,
  "name": "Rex",
  "tag": "dog"
}
```

### Create a pet

POST /pets

Creates a pet. The request body is JSON:

```json
{
  "name": "Rex",
  "tag": "dog"
}
```

On success the API answers `201 Created` with the new pet:

```json
{
  "id": 3,
  "name": "Rex",
  "tag": "dog"
}
```

## Pet fields

| Field | Type | Description |
|-------|------|-------------|
| id | integer | The pet id |
| name | string | The pet name |
| tag | string | An optional tag |
