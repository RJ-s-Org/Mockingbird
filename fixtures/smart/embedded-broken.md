# Broken definitions

The definition below has one good and one broken operation.

```json
{
  "openapi": "3.0.0",
  "info": {"title": "Broken", "version": "1"},
  "paths": {
    "/a": {"get": {"operationId": "getA", "responses": {"200": {"description": "OK",
      "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Missing"}}}}}}},
    "/b": {"get": {"operationId": "getB", "responses": {"200": {"description": "OK",
      "content": {"application/json": {"schema": {"type": "object"}}}}}}}
  }
}
```

This one does not parse:

```yaml
openapi: [3
paths: {
```
