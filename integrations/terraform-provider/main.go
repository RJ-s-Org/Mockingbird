// Command terraform-provider-mockingbird is the Mockingbird provider for
// Terraform and OpenTofu.
package main

import (
	"context"
	"flag"
	"log"

	"github.com/hashicorp/terraform-plugin-framework/providerserver"

	"github.com/RJ-s-Org/terraform-provider-mockingbird/internal/provider"
)

// Generates the registry docs from the schemas, templates and examples.
//go:generate go tool tfplugindocs generate --provider-name mockingbird

// version is set by the release build (goreleaser `-X main.version=…`).
var version = "dev"

func main() {
	var debug bool
	flag.BoolVar(&debug, "debug", false, "run the provider with support for debuggers like delve")
	flag.Parse()

	err := providerserver.Serve(context.Background(), provider.New(version), providerserver.ServeOpts{
		Address: "registry.terraform.io/rj-s-org/mockingbird",
		Debug:   debug,
	})
	if err != nil {
		log.Fatal(err.Error())
	}
}
