# Mockingbird

A mock-endpoint platform: import an API contract and get a running mock that answers with schema-valid, LLM-generated responses.

## Quickstart

Install from a checkout:

```sh
cargo install --path crates/mockingbird
```

Serve a contract (OpenAPI 3.x or Swagger 2.0) with an LLM behind it:

```sh
MB_LLM_API_KEY=sk-... mockingbird serve orders.yaml
```

Or serve with the built-in faker only, no LLM:

```sh
mockingbird serve --offline orders.yaml
```

Several contracts are each mounted under `/<file stem>`, for example `/a/orders` and `/b/orders`. A single contract is mounted at `/`.

```sh
# Create an order; the response is schema-valid.
curl -i -XPOST localhost:8080/orders \
  -H 'content-type: application/json' \
  -d '{"customerId":"c1","total":10}'

# State is kept per session (default session: `default`).
curl -i localhost:8080/orders/<id-from-above>
curl -i localhost:8080/orders/<id> -H 'X-Mockingbird-Session: alice'

# Force an error status, or add latency.
curl -i localhost:8080/orders/<id> -H 'X-Mockingbird-Status: 404'
curl -i localhost:8080/orders -H 'X-Mockingbird-Latency: 250ms'
```

Every response carries `X-Mockingbird-Source: llm | repaired | fallback`.

Check a contract without serving it:

```sh
mockingbird validate orders.yaml
```

### Flags

| Flag | Default | Meaning |
| --- | --- | --- |
| `--host` | `127.0.0.1` | Bind address. A non-loopback host requires `--insecure-no-auth`. |
| `--port` | `8080` | Bind port. |
| `--insecure-no-auth` | off | Permit a non-loopback bind (there is no auth yet). |
| `--offline` | off | Faker only; no LLM calls. |
| `--llm-timeout` | `3s` | Per-request LLM timeout before falling back. |

### Environment

| Variable | Default | Meaning |
| --- | --- | --- |
| `MB_LLM_API_KEY` | none | API key. Required unless `--offline`; local endpoints such as Ollama accept any dummy value. |
| `MB_LLM_BASE_URL` | `https://api.deepseek.com` | OpenAI-compatible base URL. |
| `MB_LLM_MODEL` | `deepseek-chat` | Model name. |
| `MB_LLM_JSON_SCHEMA` | `false` | `true` to request JSON Schema mode instead of JSON mode. |
| `RUST_LOG` | `info` | Log filter. |

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.
