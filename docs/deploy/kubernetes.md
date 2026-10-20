# Kubernetes

The Helm chart in [`deploy/helm/mockingbird`](../../deploy/helm/mockingbird) runs Mockingbird on Kubernetes 1.27 or newer. Its [README](../../deploy/helm/mockingbird/README.md) lists every value; this page walks through a production install. Releases publish the chart, signed, to `oci://ghcr.io/rj-s-org/charts/mockingbird`.

The chart has no subcharts: Postgres, Redis and the OIDC issuer are external, and the chart only takes their URLs or Secrets.

## Install

### All-in-one

One Deployment running `--role all`. With SQLite or in-memory stores it must stay at one replica; with Postgres it can run more, but `split` is the better HA shape.

```sh
kubectl create secret generic mockingbird-db --from-literal=url='postgres://mb:…@pg:5432/mockingbird'
kubectl create secret generic mockingbird-llm --from-literal=api-key='sk-…'

helm install mockingbird oci://ghcr.io/rj-s-org/charts/mockingbird --version 0.1.0 -f values.yaml
```

```yaml
# values.yaml
storage:
  existingSecret: mockingbird-db
llm:
  existingSecret: mockingbird-llm
config:
  auth: { … }   # see OIDC below; without auth the pods refuse to bind 0.0.0.0
```

### Split control and data planes

`mode: split` renders `<release>-control` (`--role control`: console, Admin API, leader-elected reconcile and maintenance, import workers) and `<release>-data` (`--role data`: the mocks, scaled by an HPA). The control plane must be Postgres; rendering fails without `storage.url` or `storage.existingSecret`. Session state is Postgres or Redis, and the bus Postgres or Redis.

```yaml
mode: split
storage:
  existingSecret: mockingbird-db        # MB_STORAGE_URL, postgres://
state:
  existingSecret: mockingbird-redis     # MB_STATE_URL, redis:// or rediss://
bus:
  existingSecret: mockingbird-redis     # MB_BUS_URL; omit to use Postgres LISTEN/NOTIFY
control:
  replicas: 2                           # one leads at a time
data:
  autoscaling: {enabled: true, minReplicas: 2, maxReplicas: 10, targetCPUUtilizationPercentage: 70}
```

Do not set `config.server.role`; the mode sets each Deployment's `--role`. See [architecture](../architecture.md#roles) for what each role runs.

## External Postgres and Redis

- **Postgres**: one database for the control store, and optionally the session state and bus. Migrations run at startup under an advisory lock, so several pods starting together are safe. Pass the URL through `storage.existingSecret` (key `url` by default, `storage.existingSecretKey` to change it). Use `sslmode=require` (or stricter) in the URL for a managed database.
- **Redis** for session state and the bus: a single primary (Redis Cluster is not supported) with **`maxmemory-policy noeviction`**, since an evicted key silently loses session data. Use `rediss://` for TLS.
- With Postgres alone (no Redis), leave `state` and `bus` empty: state then lives in Postgres and the bus is `LISTEN/NOTIFY` on the storage URL. A pooler in transaction mode (PgBouncer) breaks `LISTEN/NOTIFY` and advisory locks, so it is unsupported for the bus and the leader lock (which follows the bus); point `bus.url` at a direct or session-mode connection, or use Redis for the bus. See [leader election](../architecture.md#leader-election) for the brief leadership overlap a Postgres lock allows.

## OIDC with Rauthy

[`examples/rauthy-values.yaml`](../../deploy/helm/mockingbird/examples/rauthy-values.yaml) is a complete split install behind an existing Rauthy. The essentials:

```yaml
config:
  auth:
    mode: oidc
    external_url: https://mockingbird.example.com
    oidc:
      issuer: https://auth.example.com/auth/v1/   # exactly the discovery document's issuer
      client_id: mockingbird
      client_secret: env:MB_OIDC_CLIENT_SECRET
env:
  - name: MB_OIDC_CLIENT_SECRET
    valueFrom: {secretKeyRef: {name: mockingbird-oidc, key: client-secret}}
```

In Rauthy, create the confidential client `mockingbird` with the redirect URI `https://mockingbird.example.com/auth/callback`, allow the scopes `openid`, `profile`, `email` and `groups`, and create the groups `mb:sysadmin` and `mb:<workspace>:<owner|admin|editor|viewer>`. Any other OIDC issuer works the same way, as long as a claim (`auth.oidc.roles_claim`, default `groups`) carries those strings.

## Ingress

Three ingresses, each off by default:

| Value | Routes | Backend |
| --- | --- | --- |
| `ingress` | HTTP mocks: REST, GraphQL, SOAP, SSE, WebSocket | `<release>` port `http` |
| `grpcIngress` | gRPC mocks (with `nginx.ingress.kubernetes.io/backend-protocol: GRPC`) | `<release>` port `grpc` |
| `adminIngress` | Console and Admin API | `<release>-admin` port `admin` |

```yaml
adminIngress:
  enabled: true
  className: nginx
  hosts: [{host: mockingbird.example.com, paths: [{path: /, pathType: Prefix}]}]
  tls: [{hosts: [mockingbird.example.com], secretName: mockingbird-tls}]
grpcIngress:
  enabled: true
  className: nginx
  hosts: [{host: grpc.mocks.example.com, paths: [{path: /, pathType: Prefix}]}]
  tls: [{hosts: [grpc.mocks.example.com], secretName: grpc-mocks-tls}]
```

gRPC through ingress-nginx needs TLS on the ingress. `auth.external_url` must be the admin ingress's URL: in `oidc` mode the admin port answers only to that host. For SSE and WebSocket mocks, raise the ingress's read timeout (`nginx.ingress.kubernetes.io/proxy-read-timeout`). With this chart, terminate TLS at the ingress and leave `config.server.tls` unset: the chart cannot mount certificate files into the pods, and its probes speak plain HTTP, so pods with `server.tls` set never become Ready.

## Monitoring

- **ServiceMonitor:** `serviceMonitor.enabled: true` (Prometheus Operator) scrapes `/metrics` on the `admin` port every `serviceMonitor.interval`; add `serviceMonitor.labels` to match your Prometheus's selector. With `config.auth.public_metrics: false`, `/metrics` needs an API key, which the ServiceMonitor does not send.
- **Probes:** liveness `GET /healthz`, readiness `GET /readyz` on `admin`. A `degraded` node stays ready (200); only an unreachable control store makes it unready (503).
- The [runbook](../operations/runbook.md) lists the metrics to alert on.

## NetworkPolicy

`networkPolicy.enabled: true` restricts ingress to the pods to `networkPolicy.ingressFrom` peers (empty: any) and, when `networkPolicy.egress` is non-empty, egress to those rules. Egress must allow DNS, Postgres, Redis, the LLM provider, the OIDC issuer, the audit webhook and any brokers:

```yaml
networkPolicy:
  enabled: true
  ingressFrom:
    - namespaceSelector: {matchLabels: {kubernetes.io/metadata.name: ingress-nginx}}
    - namespaceSelector: {matchLabels: {kubernetes.io/metadata.name: monitoring}}
  egress:
    - ports: [{port: 53, protocol: UDP}, {port: 53, protocol: TCP}]
    - ports: [{port: 5432, protocol: TCP}, {port: 6379, protocol: TCP}]
    - ports: [{port: 443, protocol: TCP}]
```

## Graceful shutdown

Pods get 30 s to shut down. The image is distroless (no shell, no `sleep` binary), so the 5 s `preStop` pause that lets Services and ingresses drop a terminating pod before it stops accepting connections uses the kubelet's own `sleep` action, which needs **Kubernetes 1.30 or newer**. The chart renders it only when `.Capabilities.KubeVersion` is at least 1.30. When rendering offline (`helm template`, or a GitOps tool that does not report the cluster), pass `--kube-version` so the check sees your cluster's version.

On Kubernetes 1.27 to 1.29 the chart installs without a `preStop` hook. The pod gets SIGTERM while its endpoint removal is still propagating, stops accepting new connections at once and finishes the calls in flight; a request routed to it during that window (usually well under a second, longer with slow ingress controllers) fails with a refused or reset connection. Retry idempotent calls at the client or ingress, or upgrade to 1.30 for the pause.

## Autoscaling

`data.autoscaling` (on by default in `split`) is an HPA on the data Deployment: 2 to 10 replicas at 70 % CPU. Data nodes are stateless, so scaling is safe at any time; see [scaling data nodes](../operations/runbook.md#scaling-data-nodes). A PodDisruptionBudget (`minAvailable: 1`) protects every Deployment with more than one replica.

## Secrets

Secrets never go into `config`. Store URLs and the LLM key are environment variables filled from a value or an existing Secret, and `config` refers to other secrets (OIDC client secret, audit webhook secret, broker passwords, LLM profile keys) with a reference.

### Kubernetes Secrets (`existingSecret`)

`storage`, `state`, `bus` and `llm` each take an `existingSecret`. For anything else, add an environment variable from a Secret with `env` (or `envFrom`) and refer to it as `env:NAME`, as the OIDC example above does.

### HashiCorp Vault (`vault:` references)

A `vault:<mount>/<path>#<field>` reference reads field `<field>` of the KV v2 secret `<path>` under mount `<mount>`. It is enabled when `VAULT_ADDR` is set:

| Variable | Meaning |
| --- | --- |
| `VAULT_ADDR` | Vault's URL. |
| `VAULT_TOKEN` or `VAULT_TOKEN_FILE` | The token, or a file holding it (re-read whenever the cache misses, so a rotated token is picked up). |
| `VAULT_NAMESPACE` | Vault Enterprise namespace (optional). |
| `VAULT_CACERT` | PEM root for Vault's TLS (optional). |

Values are cached for 5 minutes. With the Vault Agent injector writing the token file:

```yaml
control: &vault
  podAnnotations:
    vault.hashicorp.com/agent-inject: "true"
    vault.hashicorp.com/agent-inject-token: "true"
    vault.hashicorp.com/role: mockingbird
data: *vault
env:
  - {name: VAULT_ADDR, value: https://vault.example.com:8200}
  - {name: VAULT_TOKEN_FILE, value: /vault/secrets/token}
config:
  auth:
    oidc:
      client_secret: vault:secret/mockingbird#oidc_client_secret
```

To let LLM profiles created through the Admin API use Vault too, allow the references in `config.server.api_secret_refs` (for example `vault:secret/mockingbird*`).

### AWS, GCP and Azure secret managers (Secrets Store CSI driver)

Mockingbird has no native client for cloud secret managers. Use the [Secrets Store CSI driver](https://secrets-store-csi-driver.sigs.k8s.io/) with your cloud's provider (AWS, GCP or Azure):

1. Create a `SecretProviderClass` naming the secrets.
2. Either **mount** them as files and refer to them as `file:/mnt/secrets/<name>`, or let the driver **sync** them into a Kubernetes Secret (`secretObjects`) and pass them as environment variables (`env`) referred to as `env:NAME`.

The chart does not template extra volumes, so the mount route needs a small post-render patch, for example with kustomize:

```yaml
# kustomization.yaml (helm install … --post-renderer ./kustomize.sh)
resources: [all.yaml]
patches:
  - target: {kind: Deployment}
    patch: |
      - op: add
        path: /spec/template/spec/volumes/-
        value: {name: secrets, csi: {driver: secrets-store.csi.k8s.io, readOnly: true, volumeAttributes: {secretProviderClass: mockingbird}}}
      - op: add
        path: /spec/template/spec/containers/0/volumeMounts/-
        value: {name: secrets, mountPath: /mnt/secrets, readOnly: true}
```

The sync route needs no patch: the driver keeps the Kubernetes Secret up to date, and a pod picks up a changed value on restart.

## Verifying images and binaries

Release images (`ghcr.io/rj-s-org/mockingbird:<version>` and `:<version>-slim`) and the chart are signed in CI with keyless cosign; the images also carry SLSA build provenance and a CycloneDX SBOM attestation. Binary archives are not signed: each has SLSA build provenance and a sha256 checksum (its `.sha256` file and the release's `SHA256SUMS`).

```sh
# Image signature (keyless, from the release workflow on a tag).
cosign verify ghcr.io/rj-s-org/mockingbird:0.1.0 \
  --certificate-identity-regexp '^https://github.com/rj-s-org/mockingbird/\.github/workflows/release\.yml@refs/tags/v' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com

# Build provenance and SBOM of the image.
gh attestation verify oci://ghcr.io/rj-s-org/mockingbird:0.1.0 --repo rj-s-org/mockingbird
gh attestation verify oci://ghcr.io/rj-s-org/mockingbird:0.1.0 --repo rj-s-org/mockingbird \
  --predicate-type https://cyclonedx.org/bom

# Checksum and build provenance of a binary archive.
sha256sum --check --ignore-missing SHA256SUMS
gh attestation verify mockingbird-0.1.0-x86_64-unknown-linux-musl.tar.gz --repo rj-s-org/mockingbird

# The chart's signature.
cosign verify ghcr.io/rj-s-org/charts/mockingbird:0.1.0 \
  --certificate-identity-regexp '^https://github.com/rj-s-org/mockingbird/' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
```

To enforce signatures in the cluster, use an admission policy (Sigstore policy-controller or Kyverno `verifyImages`) with the same identity and issuer, and pin images by digest.
