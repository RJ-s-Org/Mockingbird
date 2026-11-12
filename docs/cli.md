# CLI client

The same binary drives the Admin API. It prints JSON on stdout and problem details on stderr, and exits 1 on any non-2xx response. The target is `--admin-url`, then `MB_ADMIN_URL`, then this project's running background server as its state directory (`--state-dir` of `doctor` and `verify`, else `MB_STATE_DIR`, else `./.mockingbird`) records it for that config (wherever it listens, as long as its `/readyz` does not name another config), then the admin port of `doctor -c` / `proxy -c` or else of `./mockingbird.yaml` when it exists (`server.admin_port` on `server.host` with `MB_ADMIN_PORT` and `MB_HOST` applied over them as `server start` does, loopback when the host is `0.0.0.0` or `::`, `https` with `server.tls`), then `http://127.0.0.1:9090`. So a server started with `mockingbird server start -c mockingbird.yaml` is found from that directory without either, also when it was started on other ports than the config says.

```sh
mockingbird mocks list
mockingbird mocks get orders
mockingbird mocks create billing --prefix /bill --persona "Formal" --profile openrouter
mockingbird mocks update billing --persona "Terse" --patch chaos.json   # flags win over --patch keys
mockingbird mocks update billing --faker-seed 42   # --faker-seed none clears it
mockingbird mocks delete billing

# Upload a contract as a new version (creating the mock if missing).
mockingbird import orders.yaml --as orders --activate

# Contract versions: list them, serve another one (roll forward or back).
mockingbird contracts list orders
mockingbird contracts activate orders 2

# A contract version's world description: gaps first, then each entry, as
# text (--json prints the Admin API answer); and the description format.
mockingbird world show orders --version 2
# Only some operations or types (repeatable), with their gaps; an unknown
# name prints a warning.
mockingbird world show orders --operation getOrder --type order
# Only what the request log names (any session): the operations the app
# called, the types they touch, and their gaps. Run the app's flows first.
mockingbird world show orders --called
mockingbird world schema
# Write it as revisions: put replaces the overlay, patch merges a JSON merge
# patch (null removes an entry); each answers what applied and what was
# refused. --expect-revision N refuses the write (412 revision_conflict)
# unless N is the newest revision; --author is agent, llm or user (default).
mockingbird world patch orders patch.json --author agent --expect-revision 3
mockingbird world put orders world.json
mockingbird world revisions orders
mockingbird world revision orders 3       # one revision, with its overlay
mockingbird world diff orders --from 3 --to 4
mockingbird world restore orders 3

# A documentation, HAR or Postman import is a draft; approve it after review
# (the newest draft without a version; --no-activate approves without serving).
mockingbird import petstore.har --as pets
mockingbird drafts approve pets

mockingbird sessions list orders
mockingbird sessions reset orders alice

# Save a session's state as fixtures, and load it back (into any session).
mockingbird export-fixtures --mock orders --session alice > fixtures.json
mockingbird seed-fixtures --mock orders --session ci fixtures.json

# Anything else in the Admin API: any method and path under /api/v1.
mockingbird api GET /workspaces/default/api-keys
mockingbird api POST /workspaces --data '{"name": "team-a"}'
mockingbird api PUT /workspaces/default/mocks/orders/scenarios/slow --data @slow.json
echo '{"persona": "Formal"}' | mockingbird api PATCH /workspaces/default/mocks/orders --data @-
```

`api` takes the path under `/api/v1` (a leading `/api/v1` is accepted) with any query string, and a JSON body from `--data` (`-d`): inline, `@FILE` or `@-` for stdin. `POST`, `PUT` and `PATCH` without one send `{}`. It prints a JSON answer as is, any other answer of up to 1 MiB as `{content_type, text}` (or `{content_type, bytes, base64}` when it is not UTF-8), and nothing for an empty one; a non-2xx answer prints its problem details on stderr and exits 1, like every other client command. The MCP `admin_api` tool does the same.

## Archive uploads

A contract that spans several files (`.proto` files importing each other, a WSDL with its XSDs or the WSDLs it pulls in with `wsdl:import`, a GraphQL schema split into files, an OpenAPI, Swagger or AsyncAPI document whose `$ref`s name other files) is uploaded as one zip or tar.gz archive. `mockingbird import` and `mockingbird validate` pack a directory for you:

```sh
mockingbird import --as orders protos/          # packs protos/ as tar.gz
mockingbird import --as calc calculator.zip
```

An archive is at most 20 MiB, holds at most 500 files and unpacks to at most 64 MiB. It is unpacked in memory only: absolute paths, `..`, links and devices are rejected, and when every file shares one top-level directory, that directory is stripped. `__MACOSX/` entries and hidden files and directories (`.DS_Store`, `.git/`) are dropped, as packing a directory skips them. Beside one contract, documentation (`.md`, `.txt`, `.html`, …), images and `README`/`LICENSE` files are ignored; several contracts in one archive are refused with an error naming them. Nested archives are not supported. The CLI picks the upload's media type from the file extension (`.graphql`, `.proto`, `.pb`/`.desc`, `.wsdl`/`.xsd`/`.xml`, `.zip`, `.tgz`/`.tar.gz`, `.json`, `.yaml`).

For an OpenAPI, Swagger 2.0 or AsyncAPI contract, the root document is the one contract no other contract in the archive references (so a shared `common.yaml` with its own `openapi:` header does not count twice). A `$ref` with a path (`./schemas/pet.yaml`, `../common.yaml#/components/schemas/Error`, `#/Pet` inside a referenced file) is resolved relative to the file that holds it and bundled into the one stored contract: a schema becomes a named definition (`pet` for `./schemas/pet.yaml`, `Error` for `#/.../Error`; a name already taken gets `_2`, and `Owner: {$ref: ./owner.yaml}` keeps the name `Owner`), recursive schemas included, and other objects (path items, parameters, responses) are copied in place. A ref that climbs above the archive root, an absolute path, a file that is not in the archive, a missing pointer and a cycle outside schemas are refused with an error naming the ref. Refs to URLs in an upload are never fetched (`mockingbird import https://…/openapi.yaml` imports a contract from its URL as a draft, and follows its refs through the URL-import guards; see [`$ref`s of a fetched contract](import.md#refs-of-a-fetched-contract)). Bundling stops at 32 MiB of content and 64 nested refs. Pointing the CLI at one file of a split spec uploads that file only, and its first external ref fails with a message saying to upload the directory or bundle the spec (for example with `redocly bundle`).

## Reference

`mockingbird serve --help` lists the flags, and the [configuration reference](configuration.md#environment-variables-and-flags) maps every flag and `MB_*` variable to its key. The CLI client reads `MB_ADMIN_URL` (default: this project's background server recorded in `.mockingbird/`, else from `./mockingbird.yaml`, else `http://127.0.0.1:9090`; see above), `MB_ADMIN_TOKEN` and `MB_WORKSPACE`.
