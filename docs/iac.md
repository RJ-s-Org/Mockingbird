# Infrastructure as code: Terraform, OpenTofu and Pulumi

The Mockingbird provider manages mocks, contracts, scenarios, LLM profiles, API keys and event settings through the [Admin API](../README.md#admin-api), so a team can create a mocked lower environment from the same code that creates the rest of it. One provider serves Terraform and OpenTofu; Pulumi uses it through its Terraform bridge.

| Resource | What it manages |
| --- | --- |
| `mockingbird_workspace` | A workspace (needs a sysadmin credential) |
| `mockingbird_mock` | A mock: route prefix, hosts, persona, behaviour, chaos, limits, webhooks, data-plane auth, and its contract from inline content, a file or a URL |
| `mockingbird_contract_version` | One immutable contract version, for explicit version history |
| `mockingbird_scenario` | A named scenario (`X-Mockingbird-Scenario`) |
| `mockingbird_llm_profile` | An LLM profile; the key is a secret reference only |
| `mockingbird_api_key` | A workspace API key, its token a sensitive output |
| `mockingbird_channel` | Whether an event channel runs, and its triggers |
| `mockingbird_event_settings` | The broker a mock's channels use |

Data sources `mockingbird_mock` and `mockingbird_workspace` read existing objects, including mocks declared in the server's config file. Every resource can be imported. The full reference is generated from the provider's schema into [`integrations/terraform-provider/docs`](../integrations/terraform-provider/docs/index.md), which is what the registries show.

## A mocked staging environment in 20 lines

Staging should not charge cards or open issues. With Mockingbird running next to it (see [Kubernetes](deploy/kubernetes.md) or [Docker Compose](deploy/compose.md)), this is the whole configuration:

```hcl
terraform {
  required_providers {
    mockingbird = { source = "RJ-s-Org/mockingbird" }
  }
}

provider "mockingbird" {} # MB_ADMIN_URL, MB_ADMIN_TOKEN, MB_DATA_PLANE_URL

resource "mockingbird_mock" "stripe" {
  name         = "stripe"
  route_prefix = "/stripe"
  persona      = "Stripe in test mode for Juniper & Co, a plant shop in Portland."
  contract     = { file = "contracts/stripe.openapi.yaml" }
}

resource "mockingbird_mock" "github" {
  name         = "github"
  route_prefix = "/github"
  persona      = "GitHub for octo-org, a 40-person company with a dozen repos."
  contract     = { file = "contracts/github-rest.openapi.yaml" }
}

output "stripe_api_base" { value = mockingbird_mock.stripe.url }
output "github_api_url" { value = mockingbird_mock.github.url }
```

The two contracts are the hand-written Stripe and GitHub subsets of the [examples catalog](../examples/apis): `mockingbird examples init stripe --dir stripe` writes `stripe/stripe.openapi.yaml`, and `github-rest` likewise. Any OpenAPI, GraphQL, protobuf, WSDL or AsyncAPI document works the same way, and `contract = { url = "…" }` takes a published one.

```sh
export MB_ADMIN_URL=https://mockingbird.staging.example.com:9090
export MB_ADMIN_TOKEN=mbk_…            # an API key with the editor role
export MB_DATA_PLANE_URL=http://mockingbird.staging.svc:8080
terraform init && terraform apply
```

```text
Outputs:

github_api_url = "http://mockingbird.staging.svc:8080/github"
stripe_api_base = "http://mockingbird.staging.svc:8080/stripe"
```

Point the staging app at the outputs, for example from the module that deploys it:

```hcl
env = {
  STRIPE_API_BASE = mockingbird_mock.stripe.url # stripe.api_base / new Stripe(key, { host })
  GITHUB_API_URL  = mockingbird_mock.github.url # new Octokit({ baseUrl })
}
```

```sh
curl -s -X POST "$STRIPE_API_BASE/v1/payment_intents" -d amount=2000 -d currency=usd
# {"amount":2000,"currency":"usd","id":"pi_d3aZHHFAtiE2Xo6oBHjbppoj","livemode":false,"object":"payment_intent",…}
```

Clients that cannot change their base URL can reach the mocks by the real host names instead: set `hosts = ["api.stripe.com"]` and `route_prefix = "/"`, and route the app through `mockingbird proxy` (see [Proxy](proxy.md)).

Edit `contracts/stripe.openapi.yaml` and the next plan shows the mock updated in place; `apply` uploads the new contract version and activates it, with no restart. A scenario adds failure modes for tests:

```hcl
resource "mockingbird_scenario" "card_declined" {
  mock         = mockingbird_mock.stripe.name
  name         = "card-declined"
  instructions = "The card is declined for insufficient funds."
  status       = 402
  operations   = ["PostPaymentIntents", "PostPaymentIntentsIntentConfirm"]
}
```

A test then sends `X-Mockingbird-Scenario: card-declined` and gets a Stripe-shaped `402`.

## Provider settings

| Setting | Environment | Default | Meaning |
| --- | --- | --- | --- |
| `endpoint` | `MB_ADMIN_URL` | `http://127.0.0.1:9090` | The Admin API origin |
| `token` | `MB_ADMIN_TOKEN` | none | Bearer credential: an API key (`mbk_…`) or an OIDC access token. Not needed in `auth.mode: none` |
| `workspace` | `MB_WORKSPACE` | `default` | The workspace of every resource that does not set `workspace` |
| `data_plane_url` | `MB_DATA_PLANE_URL` | none | The data-plane origin; when set, `mockingbird_mock.url` is it plus the route prefix |

What the credential may do follows its role in the workspace: `editor` covers mocks, contracts, scenarios, channels and event settings; LLM profiles and API keys need `admin`; renaming or deleting a workspace needs `owner`. Creating a workspace, and setting an LLM profile's `secret_ref`, need a sysadmin. An API key cannot create API keys, so `mockingbird_api_key` needs a user's OIDC access token (or a server in `auth.mode: none`).

## Contracts and change detection

`mockingbird_mock.contract` takes exactly one of `content` (for example `file("openapi.yaml")` or a heredoc), `file` (a path read at plan time) or `url` (fetched at plan time, at most 20 MiB). The provider hashes the source during the plan and compares the sha256 with the `source_digest` of the version the mock serves (`contract_sha256`):

- same hash: nothing to upload, so re-applying an unchanged configuration is a no-op;
- a different hash, because the file or the document behind the URL changed, or because someone activated another version by hand: the plan shows an update, and `apply` uploads the source as a new version and activates it;
- `activate = false` uploads new versions without activating them (a mock's first version is always activated).

`active_version`, `contract_version` (the version the resource last uploaded) and `protocols` are exported. Removing `contract` from the configuration leaves the uploaded versions in place. A contract that does not compile fails the apply with the server's `422` detail; compilation warnings are shown as Terraform warnings.

For explicit history, use `mockingbird_contract_version` instead of `contract` (not both on one mock): each source change creates a new, immutable version (the resource is replaced), and `activate = true` re-activates it if another version was activated since. The Admin API keeps every version, so destroying the resource only removes it from the state.

## Secrets

Nothing secret is sent to Mockingbird or stored in the state, with one exception:

- `mockingbird_llm_profile.secret_ref` and `webhooks[*].secret` are references (`env:NAME`, `file:/path`, `vault:path#key`) that the Mockingbird server resolves. The provider rejects anything else at plan time, and the server only accepts references matching `server.api_secret_refs` / `server.api_webhook_secret_refs` (by default `env:MB_LLM_*` or `env:*_API_KEY`, and `env:*_WEBHOOK_SECRET` or `env:MB_WEBHOOK_*`).
- `mockingbird_api_key.token` is a real credential. It is marked sensitive but is stored in the state, like every generated secret, so keep the state in an encrypted backend and hand the token to a secret store rather than printing it. It is null after an import (the API never shows a token again). Every change of the key's attributes replaces it, revoking the old key.
- The provider's `token` is never stored.

## Importing

| Resource | Import ID |
| --- | --- |
| `mockingbird_workspace` | `<name>` |
| `mockingbird_mock`, `mockingbird_llm_profile` | `<workspace>/<name>` |
| `mockingbird_scenario` | `<workspace>/<mock>/<name>` |
| `mockingbird_contract_version` | `<workspace>/<mock>/<version>` |
| `mockingbird_channel` | `<workspace>/<mock>/<channel>` |
| `mockingbird_event_settings` | `<workspace>/<mock>` |
| `mockingbird_api_key` | `<workspace>/<key_id>` |

The workspace may be left out (`orders` instead of `default/orders`) to use the provider's. After importing a mock, the first plan with a `contract` uploads only if the source differs from the served version. A webhook URL's query string is never shown by the API, so an imported webhook URL with a query reads back as `…?<redacted>` until the configuration sets it.

## Limitations

- Mocks and profiles declared in the server's `mockingbird.yaml` (`source = file`) are read-only through the Admin API; read them with the `mockingbird_mock` data source.
- A mock's name, a scenario's name and a profile's name cannot be changed in place: a rename replaces the object (and a mock's session state and request log go with it).
- `default_scenario` is not managed by the provider (a scenario must exist first); set it with `PATCH /api/v1/mocks/{mock}` or the console.
- Destroying `mockingbird_channel` re-enables the channel with the contract's triggers; destroying `mockingbird_event_settings` returns to automatic broker selection.
- Changing the provider's `workspace` does not move existing resources; each keeps the workspace it was created in (set `workspace` on a resource to move it, which replaces it).

## OpenTofu

The provider is built with the Terraform plugin framework on protocol 6 and works unchanged with OpenTofu 1.6 and later; the acceptance tests run against both. With OpenTofu the same `source = "RJ-s-Org/mockingbird"` resolves from `registry.opentofu.org`. State encryption (`terraform { encryption { … } }`, OpenTofu 1.7+) is a good fit for the API key tokens.

## Pulumi

Pulumi uses the provider through its [Terraform bridge](https://www.pulumi.com/registry/packages/terraform-provider/), which generates a typed SDK in your project's language:

```sh
pulumi package add terraform-provider RJ-s-Org/mockingbird
```

```ts
import * as mockingbird from "@pulumi/mockingbird";

const stripe = new mockingbird.Mock("stripe", {
    name: "stripe",
    routePrefix: "/stripe",
    contract: { file: "contracts/stripe.openapi.yaml" },
});
export const stripeApiBase = stripe.url;
```

The provider reads the same `MB_ADMIN_URL`, `MB_ADMIN_TOKEN` and `MB_DATA_PLANE_URL` variables (or `pulumi config set mockingbird:endpoint …`). Complete TypeScript and Python programs for the staging walkthrough are in [`integrations/pulumi`](../integrations/pulumi/README.md). Until the provider is published, `pulumi package add terraform-provider ./path/to/terraform-provider-mockingbird` takes a locally built binary; both examples are set up that way.

## Using the provider before it is published

Build it and tell Terraform or OpenTofu where it is with a `dev_overrides` block (no `init` needed):

```sh
cd integrations/terraform-provider
go build -o bin/terraform-provider-mockingbird .
cat > ~/.mockingbird.tfrc <<EOF
provider_installation {
  dev_overrides { "RJ-s-Org/mockingbird" = "$PWD/bin" }
  direct {}
}
EOF
export TF_CLI_CONFIG_FILE=~/.mockingbird.tfrc
```

## Developing the provider

The provider lives in [`integrations/terraform-provider`](../integrations/terraform-provider/README.md), a Go module with no dependency on the Rust workspace. Its acceptance tests start `mockingbird serve --offline` on free ports and drive every resource through real `terraform` (or `tofu`) runs:

```sh
cargo build -p mockingbird                      # the server the tests start
cd integrations/terraform-provider
make testacc                                    # Terraform (downloaded if not installed)
make testacc-tofu                               # OpenTofu
make docs                                       # regenerate docs/ after a schema change
```

`MOCKINGBIRD_BIN` picks another binary, and `MB_ADMIN_URL` runs the tests against a server that is already running. CI runs the same in the `IaC` workflow whenever the provider, the Admin API or the store changes.

## Publishing to the registries

Both registries read releases from a public GitHub repository named `terraform-provider-mockingbird`, signed with a GPG key. Everything that repository needs is in `integrations/terraform-provider`: the Go module (already named `github.com/RJ-s-Org/terraform-provider-mockingbird`), generated `docs/`, `examples/`, `.goreleaser.yml`, `terraform-registry-manifest.json` and `.github/workflows/release.yml` (inert in this repository). The steps, once:

1. **Create the repository** `RJ-s-Org/terraform-provider-mockingbird` on GitHub, public, empty.
2. **Copy the provider into it**, with the licenses:

   ```sh
   git clone git@github.com:RJ-s-Org/terraform-provider-mockingbird.git ../terraform-provider-mockingbird
   cp -R integrations/terraform-provider/. ../terraform-provider-mockingbird/
   cp LICENSE-MIT LICENSE-APACHE ../terraform-provider-mockingbird/
   cd ../terraform-provider-mockingbird
   rm -rf bin && git add -A && git commit -m "Initial import from RJ-s-Org/Mockingbird"
   git push origin main
   ```

3. **Create the signing key.** The Terraform Registry accepts RSA and DSA keys only (not ECC):

   ```sh
   gpg --full-generate-key                 # (1) RSA and RSA, 4096 bits, no expiry or a long one
   gpg --list-secret-keys --keyid-format=long
   gpg --armor --export <KEY_ID> > mockingbird-signing.pub.asc
   gpg --armor --export-secret-keys <KEY_ID> > mockingbird-signing.sec.asc   # keep offline
   ```

4. **Add the repository secrets** (Settings → Secrets and variables → Actions): `GPG_PRIVATE_KEY` = the contents of `mockingbird-signing.sec.asc`, `PASSPHRASE` = its passphrase. Then delete the local `.sec.asc` copy.
5. **Tag the first release.** The provider version is independent of Mockingbird's, but starting at the launch version keeps them easy to match:

   ```sh
   git tag v0.1.0 && git push origin v0.1.0
   ```

   The release workflow builds every platform with goreleaser, signs the `SHA256SUMS` file and attaches the `manifest.json`. Check that the GitHub release has the zip files, `terraform-provider-mockingbird_0.1.0_SHA256SUMS`, its `.sig` and `terraform-provider-mockingbird_0.1.0_manifest.json`.
6. **Terraform Registry:** sign in to [registry.terraform.io](https://registry.terraform.io) with GitHub (as a member of `RJ-s-Org` who can grant the registry access to the organisation), add the public key under *User Settings → Signing Keys* for the `RJ-s-Org` namespace, then *Publish → Provider* and choose the repository. The registry installs a webhook; every later `v*` tag is published automatically.
7. **OpenTofu Registry:** in [github.com/opentofu/registry](https://github.com/opentofu/registry/issues/new/choose), open a *Submit new Provider* issue for `RJ-s-Org/terraform-provider-mockingbird` and a *Submit new Provider Signing Key* issue with `mockingbird-signing.pub.asc`. A bot opens the pull request; once it is merged, new tags are picked up within about an hour.
8. **Pulumi:** nothing to publish. `pulumi package add terraform-provider RJ-s-Org/mockingbird` resolves the provider from the OpenTofu Registry once step 7 is done; in the two `Pulumi.yaml` files, replace the local binary in `parameters` with `RJ-s-Org/mockingbird` followed by `0.1.0` (the provider version).

For later releases: change the provider here, let the `IaC` workflow pass, copy `integrations/terraform-provider/.` into the standalone repository again, commit and push a new `v*` tag.
