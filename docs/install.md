# Install

Every channel installs the same `mockingbird` binary, built by the release workflow for macOS (Apple Silicon and Intel), Linux (x86_64 and arm64, static musl builds that run on any distribution) and Windows (x86_64).

| Channel | Command | Needs |
| --- | --- | --- |
| npm, no install | `npx @mockingbird-cli/mockingbird run -- npm test` | Node.js 18+ |
| npm, per project | `npm i -D @mockingbird-cli/mockingbird` | Node.js 18+ |
| [Install script](#install-script) | `install.sh` (macOS, Linux), `install.ps1` (Windows) | curl or wget; PowerShell |
| Docker | `docker run --rm ghcr.io/rj-s-org/mockingbird:latest --version` | Docker |
| From source | `cargo install --git https://github.com/RJ-s-Org/Mockingbird mockingbird` | Rust |

## npm and npx

For a JavaScript or TypeScript project, the npm package is the shortest way in: no Rust toolchain, no binary to download by hand, and the version is pinned in your lockfile like any other dev dependency.

Run your tests against mocks, with nothing installed:

```sh
npx @mockingbird-cli/mockingbird run --offline -- npm test   # the built-in faker answers; no key, no network
npx @mockingbird-cli/mockingbird run -- npm test             # an LLM writes the answers (`mockingbird init` lists providers)
```

Or add it to the project, so `mockingbird` is on the path of every npm script:

```sh
npm i -D @mockingbird-cli/mockingbird
```

```json
{
  "scripts": {
    "test": "vitest run",
    "test:mocked": "mockingbird run --offline -- npm test"
  }
}
```

`npx mockingbird …` then runs the project's copy. pnpm (`pnpm add -D @mockingbird-cli/mockingbird`, `pnpm exec mockingbird`), Yarn and Bun install it the same way.

### How the package works

`@mockingbird-cli/mockingbird` is a small Node.js launcher. It lists one package per platform as an optional dependency (`@mockingbird-cli/darwin-arm64`, `darwin-x64`, `linux-x64`, `linux-arm64`, `win32-x64`), and npm installs only the one whose `os` and `cpu` match the machine. The launcher runs that package's binary with your arguments, stdio, exit code and signals passed through, so `mockingbird run` behaves exactly as the installed binary does.

When the platform package is missing (optional dependencies skipped with `--omit=optional`, or a lockfile written on another platform), the launcher downloads the same release archive from GitHub once, checks it against the sha256 that was published inside `@mockingbird-cli/mockingbird`, and caches the binary (`~/.cache/mockingbird`, `~/Library/Caches/mockingbird` or `%LOCALAPPDATA%\mockingbird\cache`). Windows on Arm uses the x64 build through emulation, by the same download. Any other platform fails with the list of supported ones; build from source there.

| Variable | Effect |
| --- | --- |
| `MOCKINGBIRD_BINARY` | Run this binary instead of the packaged one |
| `MOCKINGBIRD_NO_DOWNLOAD` | Fail instead of downloading when the platform package is missing (air-gapped CI) |
| `MOCKINGBIRD_DOWNLOAD_URL` | Base URL of a mirror of the GitHub releases (`<url>/v<version>/<asset>`) |
| `MOCKINGBIRD_CACHE_DIR` | Where downloaded binaries are cached |

Every package is published from the release workflow with [npm provenance](https://docs.npmjs.com/generating-provenance-statements), so `npm audit signatures` checks that it was built from this repository's tagged commit.

## Install script

The install script puts the release binary for your platform in `~/.mockingbird/bin` (or `$MOCKINGBIRD_INSTALL_DIR`) after checking its sha256:

```sh
curl -fsSL https://raw.githubusercontent.com/rj-s-org/mockingbird/main/install.sh | sh
```

On Windows:

```powershell
irm https://raw.githubusercontent.com/rj-s-org/mockingbird/main/install.ps1 | iex
```

See [Coding agents](agents.md#install) for wiring it into an agent. To check the signatures and provenance of a binary or image, see [Verifying images and binaries](deploy/kubernetes.md#verifying-images-and-binaries).

## From source

```sh
cargo install --git https://github.com/RJ-s-Org/Mockingbird mockingbird
# or, from a checkout:
cargo install --path crates/mockingbird
```

Next: [Quickstart](quickstart.md).
