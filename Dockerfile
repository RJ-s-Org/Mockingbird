# syntax=docker/dockerfile:1

# Builds the console (the Vite build that mb-console embeds) into
# /src/crates/mb-console/dist.
FROM node:22-bookworm AS console
WORKDIR /src/crates/mb-console/web
COPY crates/mb-console/web/package.json crates/mb-console/web/package-lock.json ./
RUN npm ci
COPY crates/mb-console/web ./
RUN npm run build

# Collects only the Cargo manifests, so the dependency layer below is rebuilt
# only when a manifest or the lockfile changes.
FROM rust:1.97-bookworm AS manifests
WORKDIR /src
COPY . .
RUN mkdir /manifests \
 && find . \( -name Cargo.toml -o -name Cargo.lock \) -exec cp --parents {} /manifests/ \;

FROM rust:1.97-bookworm AS builder
# Cargo features of the binary. The default builds the full image (Kafka,
# AMQP and MQTT brokers; librdkafka is compiled with cmake). For the slim
# image (WebSocket channels and the memory broker only, no system libraries):
#   docker build --build-arg FEATURES=--no-default-features -t mockingbird:slim .
ARG FEATURES="--features full"
# libcurl headers: librdkafka 2.12 includes <curl/curl.h> even without curl.
RUN apt-get update \
 && apt-get install -y --no-install-recommends cmake libcurl4-openssl-dev \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY --from=manifests /manifests ./
# Build the dependencies against stub sources (an empty lib per crate, a
# stub `main` for the binary, a no-op build script where one is declared, and
# an empty file per declared test target).
RUN for manifest in crates/*/Cargo.toml; do \
        dir="$(dirname "$manifest")"; \
        mkdir -p "$dir/src" "$dir/tests"; \
        touch "$dir/src/lib.rs"; \
        if grep -q '^build *=' "$manifest"; then echo 'fn main() {}' > "$dir/build.rs"; fi; \
        awk '/^\[\[test\]\]/{t=1; next} /^\[/{t=0} t && $1=="name"{gsub(/"/,"",$3); print $3}' "$manifest" \
          | while read -r name; do touch "$dir/tests/$name.rs"; done; \
    done \
 && echo 'fn main() {}' > crates/mockingbird/src/main.rs \
 && cargo build --release --locked -p mockingbird $FEATURES \
 && rm -rf crates
COPY crates crates
COPY --from=console /src/crates/mb-console/dist crates/mb-console/dist
# COPY keeps the context's timestamps, which can be older than the stub
# build; touch the sources so Cargo rebuilds the workspace crates.
RUN find crates -name '*.rs' -exec touch {} + \
 && cargo build --release --locked -p mockingbird $FEATURES

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=builder /src/target/release/mockingbird /usr/local/bin/mockingbird
# Data plane, Admin API / ops and gRPC. All bind 127.0.0.1 unless told otherwise:
#   docker run -p 8080:8080 -p 9090:9090 -p 50051:50051 IMAGE serve --host 0.0.0.0 --insecure-no-auth ...
EXPOSE 8080 9090 50051
ENTRYPOINT ["mockingbird"]
CMD ["--help"]
