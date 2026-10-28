# The Mockingbird binary the acceptance tests start (built from this
# repository's root; override with MOCKINGBIRD_BIN=…).
MOCKINGBIRD_BIN ?= $(abspath ../../target/debug/mockingbird)

.PHONY: build test testacc testacc-tofu docs

build:
	go build ./...
	go vet ./...

test:
	go test ./...

# Starts `mockingbird serve --offline` on free ports and runs every resource
# against it with Terraform (downloaded if not on PATH).
testacc:
	MOCKINGBIRD_BIN=$(MOCKINGBIRD_BIN) TF_ACC=1 go test ./... -count=1 -timeout 10m

# The same against OpenTofu.
testacc-tofu:
	MOCKINGBIRD_BIN=$(MOCKINGBIRD_BIN) TF_ACC=1 TF_ACC_TERRAFORM_PATH=$$(command -v tofu) \
	TF_ACC_PROVIDER_HOST=registry.opentofu.org go test ./... -count=1 -timeout 10m

docs:
	go generate ./...
