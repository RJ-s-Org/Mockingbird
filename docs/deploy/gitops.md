# GitOps and preview environments

A preview environment per pull request usually stops at the third-party APIs: the payment provider has no sandbox per PR, the CRM's sandbox is shared, and a broken upstream fails every preview at once. This page puts a Mockingbird next to each preview, with the mocks declared in the application's own repository, so a PR that changes a contract or a scenario changes its own mocks.

The pieces:

| Piece | Where | Use it for |
| --- | --- | --- |
| Helm chart with `mocks:` in the values | [`deploy/helm/mockingbird`](../../deploy/helm/mockingbird/README.md#declaring-mocks) | any cluster; Argo CD and Flux render it |
| Kustomize base and overlays | [`deploy/kustomize`](../../deploy/kustomize) | teams that do not use Helm |
| Argo CD `Application` and PR `ApplicationSet` | [`examples/gitops/argocd`](../../examples/gitops/argocd) | long-lived and per-PR mock environments |
| Flux `GitRepository` and `HelmRelease` | [`examples/gitops/flux`](../../examples/gitops/flux) | the same with Flux |
| Argo Workflows `WorkflowTemplate`, Tekton `Task` | [`examples/gitops/argo-workflows`](../../examples/gitops/argo-workflows), [`examples/gitops/tekton`](../../examples/gitops/tekton) | an integration test run against throwaway mocks |
| Testcontainers modules | [`integrations/testcontainers`](../../integrations/testcontainers) | the same inside a unit-test process (Node, Python, Java, Go) |

All of them run offline (faker answers, no LLM key) unless you give them a key, and none of them puts a secret in Git: keys are Kubernetes Secrets the manifests reference.

## 1. Declare the mocks next to the app

Keep a values file in the application's repository, say `deploy/preview/mocks-values.yaml` ([a complete one](../../examples/gitops/mocks-values.yaml)):

```yaml
config:
  server:
    insecure_no_auth: true          # cluster-internal; keep adminIngress off or set config.auth
reload: watch                       # pushes to the PR reconcile in place, sessions survive
mocks:
  payments:
    contract:
      configMap: {name: payments-openapi, key: openapi.yaml}   # or inline: |, or url: + sha256:
    route_prefix: /payments
    persona: A European card processor; amounts in cents.
    scenarios:
      card-declined: {status: 402, operations: [createCharge]}
tests:
  requests:                         # what `helm test` checks
    - {path: /payments/v1/charges, method: POST, status: 201, body: {amount: 1999, currency: eur}}
ingress:
  enabled: true
  className: nginx
  hosts:
    - host: "{{ .Release.Name }}.preview.example.com"   # one host per release
      paths: [{path: /, pathType: Prefix}]
```

Point the app's preview at `http://<release>-mockingbird.<namespace>:8080/payments` instead of the provider. Apps that cannot change their base URL can keep the real host name: list it in `hosts:` and route the app through [`mockingbird proxy`](../proxy.md).

To give the mocks an LLM (richer, persona-driven answers), create the Secret in the preview namespace, or replicate one into it with your secret operator, and reference it:

```yaml
llm:
  existingSecret: mockingbird-llm   # key api-key
  baseUrl: https://api.deepseek.com
  model: deepseek-chat
```

Each mock can cap its spend with `limits.daily_token_budget`; once it is spent, the mock falls back to the faker.

## 2. One mock environment per pull request

### Argo CD

[`argocd/applicationset-pr.yaml`](../../examples/gitops/argocd/applicationset-pr.yaml) uses the pull-request generator: for every open PR labelled `preview` it creates the Application `mocks-pr-<n>`, which installs the chart as release `mocks-pr-<n>` in namespace `preview-pr-<n>` with the PR head's `deploy/preview/mocks-values.yaml` (a second source with `ref: app`). Closing or merging the PR deletes the Application, and its finalizer prunes the namespace's mocks.

```sh
kubectl -n argocd create secret generic github-token --from-literal=token="$GITHUB_TOKEN"
kubectl apply -f examples/gitops/argocd/applicationset-pr.yaml
```

Notes:

- Register `ghcr.io/rj-s-org/charts` as an OCI Helm repository in Argo CD (a repository Secret with `enableOCI: "true"`), or point `repoURL` at the Git repository and `path: deploy/helm/mockingbird`.
- Argo CD renders the chart without asking the cluster for its version; `helm.kubeVersion` keeps the 1.30+ `preStop` sleep.
- The ApplicationSet's own templates use `goTemplate`; the values file's `{{ .Release.Name }}` is rendered by Helm, not by the ApplicationSet, because it lives in the values file and not in the ApplicationSet.
- Your app's own ApplicationSet (same generator) points its third-party base URLs at `http://mocks-pr-{{ .number }}-mockingbird.preview-pr-{{ .number }}:8080/<mock>`.

[`argocd/application.yaml`](../../examples/gitops/argocd/application.yaml) is the long-lived variant: one `Application` from the chart with the values from Git, and one from the Kustomize staging overlay.

### Flux

[`flux/mocks.yaml`](../../examples/gitops/flux/mocks.yaml) has a `GitRepository` for the chart and one for the app, a `Kustomization` that turns the app's values file into a ConfigMap, and a `HelmRelease` that reads its values from it and runs `helm test` after each install and upgrade (`spec.test.enable`). An `OCIRepository` with cosign verification is included for the published chart. For per-PR environments with Flux, generate one `HelmRelease` per PR from your CI (or with the Flux Operator's `ResourceSet` and its GitHub pull-request input provider) using the same release-name pattern.

### Plain Helm from CI

```sh
helm upgrade --install "mocks-pr-$PR" oci://ghcr.io/rj-s-org/charts/mockingbird --version 0.1.0 \
  --namespace "preview-pr-$PR" --create-namespace -f deploy/preview/mocks-values.yaml --wait
helm test "mocks-pr-$PR" --namespace "preview-pr-$PR"
helm uninstall "mocks-pr-$PR" --namespace "preview-pr-$PR"   # on PR close
```

## 3. How changes reach the mocks

| `reload` | What happens on a push to the PR |
| --- | --- |
| `rollout` (default) | the ConfigMap checksum changes, the pod rolls, and sessions and the request log start empty |
| `watch` | the ConfigMap is updated, the kubelet syncs it (usually within a minute) and `serve --watch` reconciles the mocks in place |

Contracts from `contract.configMap` are only picked up live with `watch`; with `rollout`, restart the pods after changing one. See the chart's [Reloading](../../deploy/helm/mockingbird/README.md#reloading).

## 4. Throwaway mocks for a test run

When the tests run as a job rather than against a long-lived preview:

- **Argo Workflows**: [`workflowtemplate.yaml`](../../examples/gitops/argo-workflows/workflowtemplate.yaml) starts Mockingbird as a `daemon` step from a ConfigMap ([`mocks-configmap.yaml`](../../examples/gitops/argo-workflows/mocks-configmap.yaml)), runs your test image with `PAYMENTS_BASE_URL` set, then prints the mock's request log (`GET /api/v1/mocks/<mock>/requests`), which you can keep as an artifact once an artifact repository is configured (commented in the template). Argo stops the daemon when the workflow ends, pass or fail.
  ```sh
  argo submit --from workflowtemplate/mocked-integration-test -p test-image=ghcr.io/example/shop-tests:pr-42 --watch
  ```
- **Tekton**: [`task.yaml`](../../examples/gitops/tekton/task.yaml) runs Mockingbird as a sidecar on `127.0.0.1`, waits for `/readyz`, runs the test script and prints the request log.
- **Testcontainers**: [`integrations/testcontainers`](../../integrations/testcontainers) has a small module for Node, Python, Java and Go that starts the image with a contract or a config directory, waits for `/readyz` and gives the base URL and the request log.

The request log is the useful artifact of a failed run: it holds every call the app made, with bodies, the status Mockingbird answered and whether the faker or the LLM produced it.

## 5. Kustomize instead of Helm

[`deploy/kustomize/base`](../../deploy/kustomize/base) is an all-in-one Deployment and Service with `mockingbird.yaml` and the contracts in a generated ConfigMap (its name carries a content hash, so a change rolls the Deployment). Two overlays:

| Overlay | What it adds |
| --- | --- |
| [`offline-dev`](../../deploy/kustomize/overlays/offline-dev) | namespace `mocks-dev`, faker only, unknown ids answered 404 |
| [`staging-llm`](../../deploy/kustomize/overlays/staging-llm) | namespace `mocks-staging`, the LLM key from the Secret `mockingbird-llm`, a richer persona, a daily token budget, an ingress |

```sh
kubectl apply -k deploy/kustomize/overlays/offline-dev
```

Add your contracts to the base's `configMapGenerator` (or a new overlay's, with `behavior: merge`) and name them in `mockingbird.yaml`.

## Checking it

- `scripts/check-helm.sh` lints and renders the chart (including the declarative-mock values) and validates the manifests against Kubernetes 1.30; CI runs it.
- `scripts/check-helm-kind.sh [image]` installs the declarative mocks on a kind cluster, runs `helm test` and checks that a mock added by `helm upgrade` is served without restarting the pod.
- The Argo CD, Argo Workflows and Flux examples validate with `kubeconform` against the [CRDs catalog](https://github.com/datreeio/CRDs-catalog) schemas; Tekton publishes no CRD schema, so its Task is checked by running it.
