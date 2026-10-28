# 0.1.0 launch checklist

Every step left between `main` and a public 0.1.0, in order, with who does it and the exact commands. **Owner** is either *agent* (a coding session may do it, through a reviewed PR) or *you* (the maintainer: anything that publishes, tags, spends, or changes repository settings).

The repository itself is already public (`gh repo view RJ-s-Org/Mockingbird --json visibility`); "going public" here means the release, the install channels, the docs site and the announcement.

## 1. Land the launch workstreams (agent)

- [ ] Merge the L1–L7 pull requests (engine speed, proxy and `mockingbird run`, CI, IaC, GitOps, examples, docs).
- [ ] Reconcile the docs once L3–L6 are on `main`:
  - in [`docs/SUMMARY.md`](../SUMMARY.md), replace the four draft entries under *Automation* with the real pages:
    ```markdown
    - [CI](ci.md)
    - [Infrastructure as code](iac.md)
    - [Kubernetes GitOps](deploy/gitops.md)
    - [Examples catalog](examples.md)
    ```
  - in `README.md` and [`docs/integrations.md`](../integrations.md), turn the `https://github.com/RJ-s-Org/Mockingbird/blob/main/…` links to `docs/ci.md`, `docs/iac.md`, `docs/deploy/gitops.md`, `docs/examples.md` and `integrations/github-action` into relative links;
  - merge the one-row additions other workstreams made to README's *Documentation* table into the table as it is now;
  - check that the README quickstart matches `mockingbird run --help` (flag names, `--offline`).
- [ ] Re-record the terminal demo with `mockingbird run` (needs `vhs`: `brew install vhs`):
  ```sh
  cargo build --release -p mockingbird-cli
  PATH="$PWD/target/release:$PATH" vhs docs/assets/demo.tape
  ```
- [ ] Extend the summary at the top of `CHANGELOG.md`'s `[Unreleased]` section with what L1–L6 added.
- [ ] Run the gates on `main`: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, `cargo test --workspace --all-features`, the console's `npm run typecheck && npm run lint && npm test`, and `scripts/docs-check.sh`.

## 2. Decide (you)

- [x] **The crates.io name: `mockingbird-cli`** (`mockingbird` belongs to another project). The package is `mockingbird-cli`, the command stays `mockingbird`, and the `publish-cli` job of `release.yml` publishes it with the internal crates it depends on, as `mockingbird-mcm`, `-llm`, `-engine`, `-state`, `-store`, `-import`, `-protocols`, `-admin`, `-auth` and `-console` (their libraries keep the `mb_*` names; descriptions say they are internal, without API stability), pinned to each other's exact version, after `publish-crate` publishes `mb-ext`, on final releases only. A dry run packages and verifies all of them.
- [ ] **A crates.io token**, if `CARGO_REGISTRY_TOKEN` is not set yet: create one at <https://crates.io/settings/tokens> with the `publish-new` and `publish-update` scopes (the first release creates 12 new crates, so `publish-new` is required, and the token must not be limited to existing crate names), then `gh secret set CARGO_REGISTRY_TOKEN --repo RJ-s-Org/Mockingbird`. Without it `publish-crate` and `publish-cli` fail; nothing else does.
- [x] **The npm scope.** The unscoped `mockingbird` and `mockingbird-cli` on npm belong to other projects, and `@mockingbird` was taken, so the packages live in the npm organization `mockingbird-cli`: `@mockingbird-cli/mockingbird` (the launcher; its command is `mockingbird`) and `@mockingbird-cli/<platform>-<arch>` beside it. The scope is only `name` in [`npm/cli/package.json`](../../npm/cli/package.json); the platform package names derive from it.
- [ ] **The comparison table** in the README describes WireMock, Prism, Mockoon and Microcks from their docs as of October 2026. Read it once more before posting.

## 3. Cut 0.1.0 (you, with an agent preparing the PR)

- [ ] Bump the version everywhere `scripts/check-version.sh` checks (agent, in one PR):
  ```sh
  sed -i.bak -e 's/^version = "0.1.0-rc.1"/version = "0.1.0"/' \
    -e 's/version = "=0.1.0-rc.1"/version = "=0.1.0"/' Cargo.toml && rm Cargo.toml.bak
  sed -i.bak 's/"version": "0.1.0-rc.1"/"version": "0.1.0"/' \
    plugins/mockingbird/.claude-plugin/plugin.json plugins/mockingbird/plugin.json \
    plugins/mockingbird/.cursor-plugin/plugin.json gemini-extension.json
  find plugins gemini-extension.json -name '*.bak' -delete
  # deploy/helm/mockingbird/Chart.yaml: version and appVersion "0.1.0"
  cargo update --workspace
  ```
  Rename `## [Unreleased]` to `## [0.1.0] - <date>` in `CHANGELOG.md`, add a new empty `## [Unreleased]` above it, and update the link references at the bottom:
  ```markdown
  [Unreleased]: https://github.com/rj-s-org/mockingbird/compare/v0.1.0...HEAD
  [0.1.0]: https://github.com/rj-s-org/mockingbird/compare/v0.1.0-rc.1...v0.1.0
  ```
  Then `scripts/check-version.sh v0.1.0` must pass.
- [ ] Dry-run the release from `main` (builds, scans and packages everything, publishes nothing):
  ```sh
  gh workflow run release.yml --repo RJ-s-Org/Mockingbird --ref main -f dry_run=true
  gh run watch --repo RJ-s-Org/Mockingbird
  ```
- [ ] Tag and push (**you**; this publishes images, binaries, the chart, `mb-ext` and the GitHub release):
  ```sh
  git checkout main && git pull
  git tag -s v0.1.0 -m "Mockingbird 0.1.0"
  git push origin v0.1.0
  ```
- [ ] Verify the published artifacts:
  ```sh
  curl -fsSL https://raw.githubusercontent.com/rj-s-org/mockingbird/main/install.sh | sh && mockingbird --version
  docker run --rm ghcr.io/rj-s-org/mockingbird:0.1.0 --version
  helm pull oci://ghcr.io/rj-s-org/charts/mockingbird --version 0.1.0
  cargo binstall mockingbird-cli && cargo install mockingbird-cli --version 0.1.0 --root /tmp/mb-cargo
  ```
  Check the signatures and provenance with the commands in [Verifying images and binaries](../deploy/kubernetes.md#verifying-images-and-binaries). On Windows: `irm https://raw.githubusercontent.com/rj-s-org/mockingbird/main/install.ps1 | iex`.

## 4. Install channels (you)

The files are ready in [`packaging/`](https://github.com/RJ-s-Org/Mockingbird/tree/main/packaging), rendered from a release's `SHA256SUMS` by `scripts/render-packaging.sh` (the committed copies are for 0.1.0-rc.1).

- [ ] **npm** (`npx @mockingbird-cli/mockingbird`). The `npm-pack` job of `release.yml` builds the launcher and the five platform packages from the release archives on every run, dry runs included; `npm-publish` publishes them, with provenance, after the GitHub release, but only once the `NPM_PUBLISH` repository variable is `true`. npm attaches a trusted publisher to a package that already exists, so the first version goes out with a short-lived token:
  1. ~~Create the organization~~ `mockingbird-cli`: done.
  2. Create a granular access token: **Access Tokens → Generate New Token → Granular Access Token**, expiry 7 days, **Packages and scopes: Read and write** on the `@mockingbird-cli` scope, **Bypass two-factor authentication** checked (CI cannot answer a 2FA prompt). Then give it to the workflow and turn publishing on:
     ```sh
     gh secret set NPM_TOKEN --repo RJ-s-Org/Mockingbird
     gh variable set NPM_PUBLISH --repo RJ-s-Org/Mockingbird --body true
     ```
  3. Publish 0.1.0. If these are set before the tag is pushed (section 3), the release run publishes it. If the tag went out first, `npm-publish` was skipped with a notice; re-run that job of the release run (the packed tarballs are kept as the `npm-packages` artifact):
     ```sh
     run=$(gh run list --repo RJ-s-Org/Mockingbird --workflow release.yml --branch v0.1.0 --limit 1 --json databaseId -q '.[0].databaseId')
     job=$(gh run view "$run" --repo RJ-s-Org/Mockingbird --json jobs -q '.jobs[] | select(.name == "npm-publish") | .databaseId')
     gh run rerun "$run" --repo RJ-s-Org/Mockingbird --job "$job"
     ```
  4. Switch to trusted publishing. For each of `@mockingbird-cli/mockingbird`, `@mockingbird-cli/darwin-arm64`, `darwin-x64`, `linux-x64`, `linux-arm64` and `win32-x64`: the package page → **Settings → Trusted Publisher → GitHub Actions**, organization `RJ-s-Org`, repository `Mockingbird`, workflow `release.yml`, no environment. On the same page set **Publishing access** to *Require two-factor authentication and disallow tokens*. Then remove the token: `gh secret delete NPM_TOKEN --repo RJ-s-Org/Mockingbird`, and revoke it under **Access Tokens**. Later releases publish through OIDC alone.
  5. Verify:
     ```sh
     npm view @mockingbird-cli/mockingbird version dist-tags optionalDependencies
     npx --yes @mockingbird-cli/mockingbird@0.1.0 --version
     cd "$(mktemp -d)" && npm init -y > /dev/null && npm i -D @mockingbird-cli/mockingbird && npm audit signatures
     # the download fallback, without the platform package:
     cd "$(mktemp -d)" && npm init -y > /dev/null && npm i -D --omit=optional @mockingbird-cli/mockingbird && npx mockingbird --version
     ```
- [ ] **Homebrew tap.** Create the tap, give the release workflow a token for it, and seed it:
  ```sh
  gh repo create RJ-s-Org/homebrew-tap --public --description "Homebrew formulae for Mockingbird"
  # A fine-grained token with Contents: read and write on RJ-s-Org/homebrew-tap only:
  gh secret set HOMEBREW_TAP_TOKEN --repo RJ-s-Org/Mockingbird
  gh release download v0.1.0 --repo RJ-s-Org/Mockingbird --pattern SHA256SUMS --dir /tmp/mb
  scripts/render-packaging.sh 0.1.0 /tmp/mb/SHA256SUMS /tmp/mb/packaging
  git clone https://github.com/RJ-s-Org/homebrew-tap /tmp/tap && mkdir -p /tmp/tap/Formula
  cp /tmp/mb/packaging/homebrew/mockingbird.rb /tmp/tap/Formula/ && cd /tmp/tap
  git add Formula && git commit -m "mockingbird 0.1.0" && git push
  brew install rj-s-org/tap/mockingbird && mockingbird --version
  ```
  From then on the `homebrew` job of `release.yml` updates the formula on every final release. Add `brew install rj-s-org/tap/mockingbird` to the README's quickstart and to [Quickstart](../quickstart.md).
- [ ] **Scoop.** Re-render the manifest for 0.1.0 (the command above writes `packaging/scoop/mockingbird.json`) and commit it to `packaging/scoop/` (agent PR). Users install it with:
  ```powershell
  scoop install https://raw.githubusercontent.com/RJ-s-Org/Mockingbird/main/packaging/scoop/mockingbird.json
  ```
  For `scoop bucket add`, create `RJ-s-Org/scoop-bucket` with the manifest in `bucket/`; its `checkver` and `autoupdate` let Scoop's tooling follow new releases.
- [ ] **GitHub Action, Terraform provider, Helm/Argo/Testcontainers.** Publish as their guides say once L3–L5 have merged (the GitHub Marketplace listing and the Terraform Registry both need you to sign in and accept terms).

## 5. Docs site (you, then automatic)

- [ ] Enable GitHub Pages with the Actions source: **Settings → Pages → Build and deployment → Source: GitHub Actions**. That is the only switch; the `deploy` job of the Docs workflow checks for it and is skipped until then.
- [ ] Publish the current `main` and point the repository at the site:
  ```sh
  gh workflow run docs.yml --repo RJ-s-Org/Mockingbird --ref main
  gh repo edit RJ-s-Org/Mockingbird --homepage https://rj-s-org.github.io/Mockingbird/
  ```
- [ ] Open `https://rj-s-org.github.io/Mockingbird/` and `…/api/` (the Admin API reference), and run the external link check once by hand: `lychee --no-progress README.md docs`.

## 6. Repository polish (you)

- [ ] Description and topics:
  ```sh
  gh repo edit RJ-s-Org/Mockingbird \
    --description "LLM-backed API mocks: every protocol, stateful, zero-edit HTTPS proxy, agent-native (MCP)" \
    --add-topic api-mocking --add-topic mock-server --add-topic openapi --add-topic graphql --add-topic grpc \
    --add-topic llm --add-topic mcp --add-topic testing --add-topic rust
  ```
- [ ] Social preview image (Settings → General → Social preview): a frame of `docs/assets/demo.gif` works.
- [ ] Optional: enable Discussions (`gh repo edit RJ-s-Org/Mockingbird --enable-discussions`) and add a contact link for questions in `.github/ISSUE_TEMPLATE/config.yml`.
- [ ] Create the labels the issue templates use, if missing: `gh label create contracts` and `gh label create docs`.

## 7. Announce (you)

- [ ] Post [the announcement](announcement.md): Show HN (title and first paragraph are written for it), then a blog post, r/rust, r/programming, X / Bluesky / Mastodon, and the MCP server directories.
- [ ] Submit the Claude Code plugin marketplace (`rj-s-org/mockingbird`) and the Gemini CLI extension to their community lists.
- [ ] For the first days: answer issues, label real-world contract failures `contracts`, and move confirmed gaps into [`docs/improvements.md`](../improvements.md).
