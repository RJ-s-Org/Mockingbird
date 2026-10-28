#!/usr/bin/env bash
# Installs the Helm chart with declarative, offline mocks on a kind cluster,
# runs `helm test`, then checks that a mock added by `helm upgrade` is
# hot-reloaded (`reload: watch`) without restarting the pod.
#
#   scripts/check-helm-kind.sh [image]
#
# The image defaults to the chart's appVersion on GHCR; pass a local one
# (e.g. `mockingbird:dev`, built from the Dockerfile) to load it into kind.
# Needs docker, kind, kubectl and helm. Creates the cluster `mockingbird-ci`
# unless KIND_CLUSTER names an existing one; KEEP_CLUSTER=1 keeps it.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
chart="$root/deploy/helm/mockingbird"
cluster=${KIND_CLUSTER:-mockingbird-ci}
image=${1:-}
release=pr-42
namespace=preview-pr-42
created=false

if ! kind get clusters 2>/dev/null | grep -qx "$cluster"; then
  kind create cluster --name "$cluster" --wait 120s
  created=true
fi
cleanup() {
  if [ "$created" = true ] && [ -z "${KEEP_CLUSTER:-}" ]; then
    kind delete cluster --name "$cluster"
  fi
}
trap cleanup EXIT
kubectl config use-context "kind-$cluster" >/dev/null

load() {
  # `kind load docker-image` fails on Docker's containerd image store with
  # multi-platform images; a single-platform archive works everywhere.
  local tmp
  tmp=$(mktemp -d)
  docker save --platform "linux/$(docker version -f '{{.Server.Arch}}')" "$1" -o "$tmp/image.tar" \
    || docker save "$1" -o "$tmp/image.tar"
  kind load image-archive "$tmp/image.tar" --name "$cluster"
  rm -rf "$tmp"
}

set_image=()
if [ -n "$image" ]; then
  load "$image"
  set_image=(--set "image.repository=${image%:*}" --set "image.tag=${image##*:}" --set image.pullPolicy=Never)
fi
curl_image=$(helm show values "$chart" | awk '/^curlImage:/{c=1} c&&/repository:/{r=$2} c&&/tag:/{gsub(/"/,"",$2); print r":"$2; exit}')
docker image inspect "$curl_image" >/dev/null 2>&1 || docker pull "$curl_image"
load "$curl_image"

echo "== install"
helm upgrade --install "$release" "$chart" --namespace "$namespace" --create-namespace \
  -f "$chart/ci/declarative-mocks-values.yaml" "${set_image[@]}" --wait --timeout 3m

echo "== helm test"
helm test "$release" --namespace "$namespace" --logs --timeout 2m

echo "== hot reload"
pod=$(kubectl -n "$namespace" get pods -l app.kubernetes.io/instance="$release",app.kubernetes.io/component=all -o name)
helm upgrade "$release" "$chart" --namespace "$namespace" --reuse-values \
  --set-json 'mocks.inventory={"contract":{"inline":"openapi: 3.0.3\ninfo: {title: Inventory, version: 1.0.0}\npaths:\n  /stock:\n    get:\n      operationId: getStock\n      responses:\n        \"200\":\n          description: OK\n          content:\n            application/json:\n              schema: {type: object, required: [count], properties: {count: {type: integer}}}\n"}}' \
  --set-json 'tests.requests=[{"path":"/inventory/stock","status":200}]' \
  --wait --timeout 2m
# The kubelet syncs ConfigMap volumes about once a minute; the test pod retries.
for attempt in $(seq 1 24); do
  if helm test "$release" --namespace "$namespace" --logs --timeout 1m >/dev/null 2>&1; then
    break
  fi
  if [ "$attempt" = 24 ]; then
    echo "error: the added mock was not served within 2 minutes" >&2
    kubectl -n "$namespace" logs "$pod" --tail 50 >&2
    exit 1
  fi
  sleep 5
done
now=$(kubectl -n "$namespace" get pods -l app.kubernetes.io/instance="$release",app.kubernetes.io/component=all -o name)
if [ "$pod" != "$now" ]; then
  echo "error: the pod was replaced ($pod -> $now); expected an in-place reload" >&2
  exit 1
fi
echo "reloaded in place ($pod)"

echo "helm kind checks passed"
