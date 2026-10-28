# Docker

```sh
docker build -t mockingbird .
docker run --rm mockingbird --version
```

This builds the full image (Kafka, AMQP and MQTT); `--build-arg FEATURES=--no-default-features` builds the slim one (see [Images](../events.md#images)). Release images are published as `ghcr.io/rj-s-org/mockingbird:<version>` (and `-slim`), signed with cosign; see [verifying releases](kubernetes.md#verifying-images-and-binaries).

The image builds the console too and exposes ports 8080 (data plane), 9090 (Admin API, console and ops) and 50051 (gRPC). It runs as a non-root user and, like the binary, binds loopback unless told otherwise. With `auth.mode: none`, binding all interfaces needs `--insecure-no-auth`; only do this on a trusted network (or use `oidc`, as in the [Compose stack](../../deploy/compose/README.md)):

```sh
docker run --rm -p 8080:8080 -p 9090:9090 -p 50051:50051 \
  -v "$PWD/orders.yaml:/work/orders.yaml:ro" \
  mockingbird serve --host 0.0.0.0 --insecure-no-auth --offline /work/orders.yaml
```
