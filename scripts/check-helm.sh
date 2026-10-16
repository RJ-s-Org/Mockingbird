#!/usr/bin/env bash
# Lints and renders the Helm chart (deploy/helm/mockingbird) and validates the
# rendered manifests against the Kubernetes 1.30 schemas.
#
#   scripts/check-helm.sh
#
# helm and kubeconform are downloaded into .tools/ when missing. Without
# network access the check prints `skipped: cannot download helm` (or
# kubeconform) and exits 0, so it never blocks an offline build.
set -euo pipefail

HELM_VERSION=v3.19.0
KUBECONFORM_VERSION=v0.7.0
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

echo "helm chart checks passed"
