# Mockingbird Helm chart

Runs [Mockingbird](https://github.com/rj-s-org/mockingbird) on Kubernetes 1.27 or newer, either as one all-in-one Deployment or split into a control plane and an autoscaled data plane. There are no subcharts: Postgres, Redis and the OIDC issuer (Rauthy or any other) are external, and the chart only takes their URLs or Secrets.

```sh
helm install mockingbird deploy/helm/mockingbird -f my-values.yaml
```

## Modes

| `mode` | Deployments | Stores |
| --- | --- | --- |
| `allInOne` (default) | `<release>-all` (`--role all`) | anything Mockingbird accepts; SQLite (on the `/tmp` `emptyDir`) or `memory` only for one replica |
| `split` | `<release>-control` (`--role control`: console, Admin API, leader-elected reconcile and maintenance) and `<release>-data` (`--role data`: the mocks, scaled by an HPA) | `storage` must be Postgres (rendering fails without `storage.url` or `storage.existingSecret`); `state` Postgres or Redis; `bus` Redis or Postgres |

## Configuration

`config` is Mockingbird's `mockingbird.yaml`. It is deep-merged (`mergeOverwrite`, later wins) over [`files/default-config.yaml`](files/default-config.yaml), which only sets `server.host: 0.0.0.0`, and rendered into a ConfigMap mounted at `/etc/mockingbird/mockingbird.yaml`. Its checksum is a pod annotation, so a change rolls the pods (or, with `reload: watch`, is reconciled in place). Do not set `server.role`; the mode sets it with `--role`. The container ports follow `config.server.port`, `grpc_port` and `admin_port`.

**Authentication is not configured by default.** Set `config.auth` (OIDC, see [`examples/rauthy-values.yaml`](examples/rauthy-values.yaml)); otherwise Mockingbird refuses to listen on `0.0.0.0` and the pods do not start. `config.server.insecure_no_auth: true` lifts that for a private test install, and makes everyone who reaches the admin service a sysadmin. The install notes warn when `config.auth.mode` is unset.

Secrets never go into `config`: the stores and the LLM key are passed as environment variables, from a value or an existing Secret, and `config` refers to other secrets as `env:NAME` (with `env` below) or `file:/path`.

| Value | Default | Meaning |
| --- | --- | --- |
| `image.repository` | `ghcr.io/rj-s-org/mockingbird` | |
| `image.tag` | `""` | defaults to the chart's `appVersion` |
| `image.flavor` | `full` | `slim` appends `-slim` (no broker clients) |
| `image.pullPolicy` | `IfNotPresent` | |
| `imagePullSecrets` | `[]` | |
| `mode` | `allInOne` | `allInOne` or `split` |
| `config` | `{}` | `mockingbird.yaml`, merged over the defaults |
| `mocks` | `{}` | mocks by name, merged into `config.mocks`; see [Declaring mocks](#declaring-mocks) |
| `reload` | `rollout` | `rollout`: a config checksum rolls the pods; `watch`: `serve --watch` reconciles in place; see [Reloading](#reloading) |
| `storage.{url,existingSecret,existingSecretKey}` | `"", "", url` | `MB_STORAGE_URL`; required in `split` |
| `state.{url,existingSecret,existingSecretKey}` | `"", "", url` | `MB_STATE_URL` (defaults to the storage URL) |
| `bus.{url,existingSecret,existingSecretKey}` | `"", "", url` | `MB_BUS_URL` (defaults to Postgres `LISTEN/NOTIFY` with Postgres storage) |
| `llm.existingSecret`, `llm.apiKeyKey` | `""`, `api-key` | `MB_LLM_API_KEY` of the `default` profile; without a key the mocks answer from the faker |
| `llm.baseUrl`, `llm.model` | `""` | `MB_LLM_BASE_URL`, `MB_LLM_MODEL` of the `default` profile (empty: the built-in defaults) |
| `llm.profiles` | `{}` | more profiles, merged into `config.llm.profiles`; `apiKeySecret: {name, key}` passes the key as `MB_PROFILE_KEY_<NAME>` |
| `env`, `envFrom` | `[]` | extra variables for every container |
| `allInOne`, `control`, `data` | replicas 1, 2, 2 | `replicas`, `resources`, `nodeSelector`, `tolerations`, `affinity`, `podAnnotations`, `topologySpreadConstraints` |
| `data.autoscaling` | enabled, 2 to 10, 70 % CPU | the HPA of the data Deployment (`split` only); `data.replicas` is ignored while it is on |
| `service.type`, `service.ports` | `ClusterIP`; `http` 8080, `grpc` 50051, `admin` 9090 | |
| `ingress` | disabled | HTTP mocks to `<release>` port `http` |
| `grpcIngress` | disabled | gRPC mocks to `<release>` port `grpc`, with `nginx.ingress.kubernetes.io/backend-protocol: GRPC` |
| `adminIngress` | disabled | console and Admin API to `<release>-admin` |
| `podDisruptionBudget` | enabled, `minAvailable: 1` | one per Deployment with more than one replica (or autoscaled) |
| `networkPolicy` | disabled | `ingressFrom`: allowed peers (empty: any); `egress`: rules (empty: all egress allowed) |
| `serviceMonitor` | disabled, `30s` | Prometheus Operator scrape of `/metrics` on `admin` |
| `serviceAccount` | `create: true` | no API token is mounted |
| `tests.enabled`, `tests.requests` | `true`, `[]` | the `helm test` pod; see [helm test](#helm-test) |
| `curlImage` | `curlimages/curl:8.10.1` | image of the `fetch-contracts` init container and the test pod |
| `securityContext` | non-root uid 65532, read-only root, no privilege escalation, all capabilities dropped, `RuntimeDefault` seccomp | `/tmp` is an `emptyDir` |

## Declaring mocks

`mocks` declares the mocks in the values, so a GitOps repository (or a pull request) carries them. Each entry is a [`mocks.<name>`](../../../docs/configuration.md#mocks) section of `mockingbird.yaml`, with its keys as there (`hosts`, `persona`, `llm_profile`, `route_prefix`, `scenarios`, `webhooks`, `limits`, ...), merged into `config.mocks`. Only `contract` differs: besides a path it takes where the contract comes from.

```yaml
mocks:
  payments:
    contract:
      inline: |                    # stored in the chart's ConfigMap as payments.contract.yaml
        openapi: 3.0.3
        ...
      fileName: contract.yaml      # optional; the key is <mock>.<fileName>
    persona: A European card processor; amounts in cents.
    scenarios:
      card-declined: {status: 402, operations: [createCharge]}
  stripe:
    contract:
      configMap: {name: stripe-openapi, key: openapi.yaml}   # an existing ConfigMap; the name may use {{ .Release.Name }}
    hosts: [api.stripe.com]
    llm_profile: openrouter
  petstore:
    contract:
      url: https://example.com/openapi.json                   # downloaded at pod start
      sha256: 9f86d0...                                       # optional, checked after the download
  baked:
    contract: /contracts/baked.yaml                           # a path, for an image that carries it
```

- `inline` contracts sit next to `mockingbird.yaml` in the chart's ConfigMap.
- `configMap` contracts are mounted (the whole ConfigMap, never a `subPath`, so updates reach the pod) at `/var/lib/mockingbird/contracts/<mock>/<key>`.
- `url` contracts are downloaded by a `fetch-contracts` init container (`curlImage`) into an `emptyDir` at `/var/lib/mockingbird/fetched/<mock>/<fileName>` (`fileName` defaults to `contract`). The pods need egress to the URL. A private URL needs no token in the values: serve it from inside the cluster, or use `configMap`.

A mock without a contract does not render. Mock names may contain letters, digits, `.`, `_` and `-`.

### LLM profiles

`llm.existingSecret` (with `llm.baseUrl` and `llm.model`) configures the `default` profile, which mocks naming no profile use. Other profiles go in `llm.profiles`, each an [`llm.profiles.<name>`](../../../docs/configuration.md#llm) section; `apiKeySecret` reads the key from a Secret:

```yaml
llm:
  existingSecret: mockingbird-llm      # key api-key -> MB_LLM_API_KEY
  profiles:
    openrouter:
      base_url: https://openrouter.ai/api/v1
      model: deepseek/deepseek-v4.1-flash
      apiKeySecret: {name: openrouter, key: api-key}   # -> MB_PROFILE_KEY_OPENROUTER, api_key: env:MB_PROFILE_KEY_OPENROUTER
```

Without any key, every mock answers from the faker, offline.

## Reloading

| `reload` | A change to `config`, `mocks` or an inline contract | A change to a `contract.configMap` ConfigMap |
| --- | --- | --- |
| `rollout` (default) | the ConfigMap's checksum (a pod annotation) changes, so the pods roll | not seen: restart the pods (`kubectl rollout restart`) |
| `watch` | the all-in-one (or control) pods run `serve --watch` and reconcile in place once the kubelet syncs the ConfigMap, usually within a minute; sessions and the request log survive | the same |

With `watch`, keys that Mockingbird reads at startup (`server`, `storage`, `limits`, ...; see [the reference](../../../docs/configuration.md)) still need a restart. In `split` mode only the control pods watch; the data pods follow through the bus. A config that fails to load keeps the previous mocks and makes `/readyz` report the error.

## helm test

`helm test <release>` runs a pod (`curlImage`) that waits for `/readyz` on the admin service, then sends each of `tests.requests` to the mocks service and fails unless it gets the expected status:

```yaml
tests:
  requests:
    - path: /payments/v1/charges     # /<route prefix>/<operation path>
      method: POST                   # default GET
      status: 201                    # default 200
      body: {amount: 1999, currency: eur}
      headers: {X-Mockingbird-Scenario: card-declined}
```

Argo CD and Flux (`spec.test.enable`) can run it after each sync. With `networkPolicy.enabled`, the test pod carries the release's labels, so the policy lets it in.

## Per-pull-request installs

One release per pull request, each in its own namespace:

```sh
helm upgrade --install "mocks-pr-$PR" oci://ghcr.io/rj-s-org/charts/mockingbird --version 0.1.0 \
  --namespace "preview-pr-$PR" --create-namespace -f deploy/preview/mocks-values.yaml --wait
helm test "mocks-pr-$PR" --namespace "preview-pr-$PR"
# when the PR closes
helm uninstall "mocks-pr-$PR" --namespace "preview-pr-$PR"
```

Ingress hosts and TLS entries are rendered with `tpl`, so one values file gives each release its own host:

```yaml
ingress:
  enabled: true
  hosts:
    - host: "{{ .Release.Name }}.preview.example.com"
      paths: [{path: /, pathType: Prefix}]
  tls:
    - hosts: ["{{ .Release.Name }}.preview.example.com"]
      secretName: "{{ .Release.Name }}-tls"
```

The app under test reaches the mocks at `http://mocks-pr-<n>-mockingbird.preview-pr-<n>:8080/<mock>`. [`docs/deploy/gitops.md`](../../../docs/deploy/gitops.md) does the same with an Argo CD `ApplicationSet` and Flux.

## Services

- `<release>`: the mocks (`http`, `grpc`) and `admin` for metrics, on the all-in-one or data pods.
- `<release>-admin`: the console, the Admin API and the ops endpoints, on the all-in-one or control pods.

Pods are probed on `admin`: liveness `GET /healthz`, readiness `GET /readyz`. They get 30 s to shut down; on Kubernetes 1.30 or newer a 5 s `preStop` sleep first lets the endpoints drop the pod (the image is distroless, so the kubelet's sleep action is used, which older clusters lack).

## Checking the chart

`scripts/check-helm.sh` downloads helm v3.19.0 and kubeconform v0.7.0 into `.tools/`, runs `helm lint --strict` with the default values, every `ci/*.yaml` and every `examples/*.yaml`, validates the rendered manifests against Kubernetes 1.30, checks what the declarative-mock values render (contracts, `--watch`, the per-release host, the test pod), and checks that `mode: split` without storage and a mock without a contract refuse to render. `scripts/check-helm-kind.sh` installs [`ci/declarative-mocks-values.yaml`](ci/declarative-mocks-values.yaml) on a kind cluster, runs `helm test`, and checks that a contract change is hot-reloaded.
