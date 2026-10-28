# @mockingbird-cli/mockingbird

[Mockingbird](https://github.com/RJ-s-Org/Mockingbird) serves LLM-backed mocks of the APIs your app calls (OpenAPI, GraphQL, gRPC, SOAP, AsyncAPI), with state, scenarios and a zero-edit HTTPS proxy. This package runs it from npm: no Rust toolchain, no separate install.

Run your tests against mocks, with no install at all:

```sh
npx @mockingbird-cli/mockingbird run --offline -- npm test
```

Or add it to a project and call `mockingbird` from npm scripts:

```sh
npm i -D @mockingbird-cli/mockingbird
```

```json
{
  "scripts": {
    "test:mocked": "mockingbird run --offline -- npm test"
  }
}
```

`mockingbird run` reads `mockingbird.yaml`, starts the mocks and the proxy, runs the command with the proxy settings and exits with the command's code. See the [quickstart](https://github.com/RJ-s-Org/Mockingbird/blob/main/docs/quickstart.md) and [Proxy and `mockingbird run`](https://github.com/RJ-s-Org/Mockingbird/blob/main/docs/proxy.md).

## How it installs

npm installs one platform package next to this one (`@mockingbird-cli/<platform>-<arch>`), holding the release binary for macOS (arm64, x64), Linux (x64, arm64; static, so glibc and musl alike) or Windows (x64; Arm64 through emulation). If that package is missing, because optional dependencies were skipped (`--omit=optional`) or the lockfile came from another platform, the launcher downloads the same binary from the GitHub release once, checks it against the sha256 published in this package, and caches it.

| Variable | Effect |
| --- | --- |
| `MOCKINGBIRD_BINARY` | Run this binary instead |
| `MOCKINGBIRD_NO_DOWNLOAD` | Fail instead of downloading when the platform package is missing |
| `MOCKINGBIRD_DOWNLOAD_URL` | Base URL of a mirror of the GitHub releases (`<url>/v<version>/<asset>`) |
| `MOCKINGBIRD_CACHE_DIR` | Where downloads are cached (default: the user cache directory) |

Licensed under MIT or Apache-2.0.
