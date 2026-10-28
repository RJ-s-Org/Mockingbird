# Mockingbird with Pulumi

Pulumi uses the [Mockingbird Terraform provider](../terraform-provider/README.md) through its [Terraform bridge](https://www.pulumi.com/registry/packages/terraform-provider/): `pulumi package add terraform-provider …` (or `pulumi install` in these projects) generates a typed `@pulumi/mockingbird` / `pulumi_mockingbird` SDK into `sdks/`. Both programs build the [mocked staging environment](../../docs/iac.md#a-mocked-staging-environment-in-20-lines): a Stripe mock with a `card-declined` scenario and a GitHub mock, exporting the URLs the staging app points at.

| Directory | Language |
| --- | --- |
| [`typescript/`](typescript) | TypeScript (`index.ts`) |
| [`python/`](python) | Python (`__main__.py`) |

## Run

Until the provider is published, the projects take a locally built binary (the `parameters` in `Pulumi.yaml`):

```sh
(cd ../terraform-provider && go build -o bin/terraform-provider-mockingbird .)
mockingbird serve --offline &        # or point MB_ADMIN_URL at a real server
export MB_ADMIN_URL=http://127.0.0.1:9090 MB_DATA_PLANE_URL=http://127.0.0.1:8080
cd typescript                        # or python
pulumi login --local                 # any backend works
pulumi install                       # generates sdks/mockingbird and installs it
pulumi stack init dev
pulumi preview
pulumi up
```

The contracts default to the Stripe and GitHub subsets of the examples catalog (`examples/apis` in the repository root); `pulumi config set stripeContract path/to/openapi.yaml` (and `githubContract`) picks others.

Once the provider is in the OpenTofu Registry, replace the local path in `Pulumi.yaml` with the registry reference, or start a new project with:

```sh
pulumi package add terraform-provider RJ-s-Org/mockingbird 0.1.0
```

## What was verified

On macOS with Pulumi 3.270 (bridge `terraform-provider` 1.4.0), Node 26 and Python 3.15, against `mockingbird serve --offline`, for both projects: `pulumi install` generated and installed the SDK from the local binary; `pulumi preview` planned the 3 resources; `pulumi up` created them and exported the mock URLs; a call with `X-Mockingbird-Scenario: card-declined` answered `402`; a second `pulumi preview --expect-no-changes` passed; `pulumi destroy` removed everything. Pulumi is not part of CI; the provider underneath is tested there with Terraform and OpenTofu.
