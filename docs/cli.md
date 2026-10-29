# CLI client

The same binary drives the Admin API. It prints JSON on stdout and problem details on stderr, and exits 1 on any non-2xx response. The target is `--admin-url`, then `MB_ADMIN_URL`, then `http://127.0.0.1:9090`.

```sh
mockingbird mocks list
mockingbird mocks get orders
mockingbird mocks create billing --prefix /bill --persona "Formal" --profile openrouter
mockingbird mocks delete billing

# Upload a contract as a new version (creating the mock if missing).
mockingbird import orders.yaml --as orders --activate

mockingbird sessions list orders
mockingbird sessions reset orders alice

# Save a session's state as fixtures, and load it back (into any session).
mockingbird export-fixtures --mock orders --session alice > fixtures.json
mockingbird seed-fixtures --mock orders --session ci fixtures.json
```

## Archive uploads

A contract that spans several files (`.proto` files importing each other, a WSDL with its XSDs, a GraphQL schema split into files) is uploaded as one zip or tar.gz archive. `mockingbird import` packs a directory for you:

```sh
mockingbird import --as orders protos/          # packs protos/ as tar.gz
mockingbird import --as calc calculator.zip
```

An archive is at most 20 MiB, holds at most 500 files and unpacks to at most 64 MiB. It is unpacked in memory only: absolute paths, `..`, links and devices are rejected, and when every file shares one top-level directory, that directory is stripped. `__MACOSX/` entries and hidden files and directories (`.DS_Store`, `.git/`) are dropped, as packing a directory skips them. Beside one contract, documentation (`.md`, `.txt`, `.html`, …), images and `README`/`LICENSE` files are ignored; several contracts in one archive are refused with an error naming them. Nested archives are not supported. The CLI picks the upload's media type from the file extension (`.graphql`, `.proto`, `.pb`/`.desc`, `.wsdl`/`.xsd`/`.xml`, `.zip`, `.tgz`/`.tar.gz`, `.json`, `.yaml`).

## Reference

`mockingbird serve --help` lists the flags, and the [configuration reference](configuration.md#environment-variables-and-flags) maps every flag and `MB_*` variable to its key. The CLI client reads `MB_ADMIN_URL` (default `http://127.0.0.1:9090`), `MB_ADMIN_TOKEN` and `MB_WORKSPACE`.
