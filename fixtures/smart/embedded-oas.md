# Pets API

## List pets

GET /pets

Returns all pets as a JSON array.

## Create a pet

The definition of the create call:

```yaml
openapi: 3.0.0
info:
  title: Pets
  version: "1"
paths:
  /pets:
    post:
      operationId: createPet
      requestBody:
        content:
          application/json:
            schema:
              type: object
              properties:
                name:
                  type: string
      responses:
        "201":
          description: Created
          content:
            application/json:
              schema:
                type: object
                properties:
                  id:
                    type: integer
                  name:
                    type: string
```
