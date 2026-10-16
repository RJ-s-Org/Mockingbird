# Mockingbird Helm chart

Runs [Mockingbird](https://github.com/rjs-git/mockingbird) on Kubernetes 1.27 or newer, either as one all-in-one Deployment or split into a control plane and an autoscaled data plane. There are no subcharts: Postgres, Redis and the OIDC issuer (Rauthy or any other) are external, and the chart only takes their URLs or Secrets.

```sh
helm install mockingbird deploy/helm/mockingbird -f my-values.yaml
```

## Modes

| `mode` | Deployments | Stores |
| --- | --- | --- |
| `allInOne` (default) | `<release>-all` (`--role all`) | anything Mockingbird accepts; SQLite (on the `/tmp` `emptyDir`) or `memory` only for one replica |
| `split` | `<release>-control` (`--role control`: console, Admin API, leader-elected reconcile and maintenance) and `<release>-data` (`--role data`: the mocks, scaled by an HPA) | `storage` must be Postgres (rendering fails without `storage.url` or `storage.existingSecret`); `state` Postgres or Redis; `bus` Redis or Postgres |

## Configuration

`config` is Mockingbird's `mockingbird.yaml`. It is deep-merged (`mergeOverwrite`, later wins) over [`files/default-config.yaml`](files/default-config.yaml), which only sets `server.host: 0.0.0.0`, and rendered into a ConfigMap mounted at `/etc/mockingbird/mockingbird.yaml`. Its checksum is a pod annotation, so a change rolls the pods. Do not set `server.role`; the mode sets it with `--role`. The container ports follow `config.server.port`, `grpc_port` and `admin_port`.

**Authentication is not configured by default.** Set `config.auth` (OIDC, see [`examples/rauthy-values.yaml`](examples/rauthy-values.yaml)); otherwise Mockingbird refuses to listen on `0.0.0.0` and the pods do not start. `config.server.insecure_no_auth: true` lifts that for a private test install, and makes everyone who reaches the admin service a sysadmin. The install notes warn when `config.auth.mode` is unset.

Secrets never go into `config`: the stores and the LLM key are passed as environment variables, from a value or an existing Secret, and `config` refers to other secrets as `env:NAME` (with `env` below) or `file:/path`.

| Value | Default | Meaning |
| --- | --- | --- |
| `image.repository` | `ghcr.io/rjs-git/mockingbird` | |
| `image.tag` | `""` | defaults to the chart's `appVersion` |
| `image.flavor` | `full` | `slim` appends `-slim` (no broker clients) |
| `image.pullPolicy` | `IfNotPresent` | |
| `imagePullSecrets` | `[]` | |
| `mode` | `allInOne` | `allInOne` or `split` |
| `config` | `{}` | `mockingbird.yaml`, merged over the defaults |
| `storage.{url,existingSecret,existingSecretKey}` | `"", "", url` | `MB_STORAGE_URL`; required in `split` |
| `state.{url,existingSecret,existingSecretKey}` | `"", "", url` | `MB_STATE_URL` (defaults to the storage URL) |
| `bus.{url,existingSecret,existingSecretKey}` | `"", "", url` | `MB_BUS_URL` (defaults to Postgres `LISTEN/NOTIFY` with Postgres storage) |
| `llm.existingSecret`, `llm.apiKeyKey` | `""`, `api-key` | `MB_LLM_API_KEY` |
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
| `securityContext` | non-root uid 65532, read-only root, no privilege escalation, all capabilities dropped, `RuntimeDefault` seccomp | `/tmp` is an `emptyDir` |

## Services

- `<release>`: the mocks (`http`, `grpc`) and `admin` for metrics, on the all-in-one or data pods.
- `<release>-admin`: the console, the Admin API and the ops endpoints, on the all-in-one or control pods.

Pods are probed on `admin`: liveness `GET /healthz`, readiness `GET /readyz`. They get 30 s to shut down; on Kubernetes 1.30 or newer a 5 s `preStop` sleep first lets the endpoints drop the pod (the image is distroless, so the kubelet's sleep action is used, which older clusters lack).

## Checking the chart

`scripts/check-helm.sh` downloads helm v3.19.0 and kubeconform v0.7.0 into `.tools/`, runs `helm lint --strict` with the default values, every `ci/*.yaml` and every `examples/*.yaml`, validates the rendered manifests against Kubernetes 1.30, and checks that `mode: split` without storage refuses to render.
