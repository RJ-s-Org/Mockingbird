#!/usr/bin/env bash
# Lints and renders the Helm chart (deploy/helm/mockingbird), builds the
# Kustomize overlays (deploy/kustomize) and validates them and the GitOps
# examples (examples/gitops) against the Kubernetes 1.30 schemas (and, for
# Argo CD, Argo Workflows and Flux, the CRDs catalog's schemas).
#
#   scripts/check-helm.sh
#
# helm, kustomize and kubeconform are downloaded into .tools/ when missing.
# Without network access the check prints `skipped: cannot download helm` (or
# another tool) and exits 0, so it never blocks an offline build.
set -euo pipefail

HELM_VERSION=v3.19.0
KUBECONFORM_VERSION=v0.7.0
KUSTOMIZE_VERSION=v5.5.0
KUBERNETES_VERSION=1.30.0

root=$(cd "$(dirname "$0")/.." && pwd)
chart="$root/deploy/helm/mockingbird"
tools="$root/.tools"
mkdir -p "$tools"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

os=$(uname -s | tr '[:upper:]' '[:lower:]')
case "$(uname -m)" in
  x86_64 | amd64) arch=amd64 ;;
  aarch64 | arm64) arch=arm64 ;;
  *) echo "skipped: unsupported architecture $(uname -m)"; exit 0 ;;
esac

helm="$tools/helm-$HELM_VERSION"
if [ ! -x "$helm" ]; then
  if ! curl -fsSL --retry 2 "https://get.helm.sh/helm-$HELM_VERSION-$os-$arch.tar.gz" -o "$tmp/helm.tgz"; then
    echo "skipped: cannot download helm"
    exit 0
  fi
  tar -xzf "$tmp/helm.tgz" -C "$tmp"
  mv "$tmp/$os-$arch/helm" "$helm"
  chmod +x "$helm"
fi

kubeconform="$tools/kubeconform-$KUBECONFORM_VERSION"
if [ ! -x "$kubeconform" ]; then
  if ! curl -fsSL --retry 2 "https://github.com/yannh/kubeconform/releases/download/$KUBECONFORM_VERSION/kubeconform-$os-$arch.tar.gz" -o "$tmp/kubeconform.tgz"; then
    echo "skipped: cannot download kubeconform"
    exit 0
  fi
  tar -xzf "$tmp/kubeconform.tgz" -C "$tmp"
  mv "$tmp/kubeconform" "$kubeconform"
  chmod +x "$kubeconform"
fi

kustomize="$tools/kustomize-$KUSTOMIZE_VERSION"
if [ ! -x "$kustomize" ]; then
  if ! curl -fsSL --retry 2 "https://github.com/kubernetes-sigs/kustomize/releases/download/kustomize%2F$KUSTOMIZE_VERSION/kustomize_${KUSTOMIZE_VERSION}_${os}_$arch.tar.gz" -o "$tmp/kustomize.tgz"; then
    echo "skipped: cannot download kustomize"
    exit 0
  fi
  tar -xzf "$tmp/kustomize.tgz" -C "$tmp"
  mv "$tmp/kustomize" "$kustomize"
  chmod +x "$kustomize"
fi

# kubeconform fetches its schemas over the network; keep them across runs.
schema_cache="$tools/kubeconform-cache"
mkdir -p "$schema_cache"

check() {
  local name=$1
  shift
  echo "== $name"
  "$helm" lint --strict "$chart" "$@"
  "$helm" template mockingbird "$chart" --namespace mockingbird --kube-version "$KUBERNETES_VERSION" "$@" \
    | "$kubeconform" -strict -summary -kubernetes-version "$KUBERNETES_VERSION" \
        -ignore-missing-schemas -cache "$schema_cache"
}

check "default values"
for values in "$chart"/ci/*.yaml "$chart"/examples/*.yaml; do
  check "${values#"$root"/}" -f "$values"
done

# A split install without a store must not render.
echo "== mode: split without storage"
if out=$("$helm" template mockingbird "$chart" --set mode=split 2>&1); then
  echo "error: mode: split without storage rendered successfully" >&2
  exit 1
fi
if ! grep -q "storage.url or storage.existingSecret" <<<"$out"; then
  echo "error: mode: split without storage failed for another reason:" >&2
  echo "$out" >&2
  exit 1
fi
echo "refused as expected"

# Declarative mocks: the inline contract lands next to mockingbird.yaml, the
# per-PR ingress host is templated, `reload: watch` adds --watch and drops the
# checksum, and the test pod calls the mocks.
echo "== declarative mocks"
out=$("$helm" template pr-42 "$chart" -f "$chart/ci/declarative-mocks-values.yaml")
for want in \
  'orders.contract.yaml: |' \
  'contract: orders.contract.yaml' \
  'host: "pr-42.mocks.ci.example"' \
  '"--role", "all", "--watch"]' \
  'helm.sh/hook: test' \
  "\"\$mocks\"'/orders/orders'"; do
  if ! grep -qF -- "$want" <<<"$out"; then
    echo "error: rendered manifests lack: $want" >&2
    exit 1
  fi
done
if grep -q 'checksum/config' <<<"$out"; then
  echo "error: reload: watch still renders the config checksum" >&2
  exit 1
fi
out=$("$helm" template t "$chart" -f "$chart/ci/contract-sources-values.yaml")
for want in \
  'contract: /var/lib/mockingbird/contracts/stripe/openapi.yaml' \
  'name: t-stripe-openapi' \
  'name: fetch-contracts' \
  'sha256sum -c -' \
  'api_key: env:MB_PROFILE_KEY_OPENROUTER' \
  'name: MB_PROFILE_KEY_OPENROUTER'; do
  if ! grep -qF -- "$want" <<<"$out"; then
    echo "error: rendered manifests lack: $want" >&2
    exit 1
  fi
done
echo "rendered as expected"

# A mock without a contract must not render.
echo "== mock without a contract"
if "$helm" template t "$chart" --set mocks.broken.persona=x >/dev/null 2>&1; then
  echo "error: a mock without a contract rendered successfully" >&2
  exit 1
fi
echo "refused as expected"

for overlay in "$root"/deploy/kustomize/base "$root"/deploy/kustomize/overlays/*/; do
  echo "== ${overlay#"$root"/}"
  "$kustomize" build "$overlay" \
    | "$kubeconform" -strict -summary -kubernetes-version "$KUBERNETES_VERSION" -cache "$schema_cache"
done

# Tekton publishes no CRD schemas; its Task is skipped here.
echo "== examples/gitops"
"$kubeconform" -strict -summary -kubernetes-version "$KUBERNETES_VERSION" -cache "$schema_cache" \
  -schema-location default \
  -schema-location 'https://raw.githubusercontent.com/datreeio/CRDs-catalog/main/{{.Group}}/{{.ResourceKind}}_{{.ResourceAPIVersion}}.json' \
  -skip Task \
  "$root"/examples/gitops/argocd "$root"/examples/gitops/argo-workflows "$root"/examples/gitops/flux "$root"/examples/gitops/tekton
# The values file the GitOps examples install must stay valid for the chart.
check "examples/gitops/mocks-values.yaml" -f "$root/examples/gitops/mocks-values.yaml"

echo "helm chart checks passed"
