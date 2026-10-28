# Testcontainers

Small [Testcontainers](https://testcontainers.com) modules that start the Mockingbird image for a test, wait for `/readyz` and hand back the mocks' base URL and their request log. Each is one file you can vendor, with a test that runs it against [`fixtures/`](fixtures) (a payments contract and a `mockingbird.yaml` with a `card-declined` scenario).

| Language | Module | Test |
| --- | --- | --- |
| Node (20+) | [`node/mockingbird.js`](node/mockingbird.js) | `cd node && npm install && npm test` |
| Python (3.10+) | [`python/mockingbird_testcontainers.py`](python/mockingbird_testcontainers.py) | `cd python && pip install -r requirements.txt && pytest` |
| Java (17+) | [`java/src/main/java/.../MockingbirdContainer.java`](java/src/main/java/io/mockingbird/testcontainers/MockingbirdContainer.java) | `cd java && mvn test` |
| Go (1.23+) | [`go/mockingbird.go`](go/mockingbird.go) | `cd go && go test ./...` |

Two ways to serve mocks:

- **One contract, zero-config** (`withContract` / `with_contract` / `WithContract`): the mock is named after the file (`payments.yaml` is `payments`) and mounted at `/`, so the base URL is the container's root.
- **A config directory** (`withConfigDir` / `with_config_dir` / `WithConfigDir`): a directory with `mockingbird.yaml` and the contracts it names is copied to `/etc/mockingbird`; each mock is at `/<name>` (or its `route_prefix`), and scenarios, personas and webhooks apply.

```js
import { MockingbirdContainer } from "./mockingbird.js";

const mocks = await new MockingbirdContainer().withConfigDir("test/mocks").start();
process.env.PAYMENTS_BASE_URL = mocks.baseUrl("payments");
// ... run the code under test ...
const calls = await mocks.requests("payments");   // what it sent, with bodies
await mocks.stop();
```

The container runs `serve --host 0.0.0.0 --insecure-no-auth --offline`: faker answers, no network, no key. `withLlm` (`with_llm`, `WithLLM`) drops `--offline` and passes `MB_LLM_API_KEY`, `MB_LLM_BASE_URL` and `MB_LLM_MODEL`; read the key from the environment, never write it in a test.

The image is `ghcr.io/rj-s-org/mockingbird:0.1.0` unless `MOCKINGBIRD_IMAGE` names another (for example one built locally with `docker build -t mockingbird:dev .`).

Send `X-Mockingbird-Scenario: <name>` to pick a scenario per call, and `X-Mockingbird-Session: <id>` to isolate the state of parallel tests.
