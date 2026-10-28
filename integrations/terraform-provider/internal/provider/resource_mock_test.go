package provider

import (
	"fmt"
	"net/http"
	"net/http/httptest"
	"os"
	"regexp"
	"testing"

	"github.com/hashicorp/terraform-plugin-testing/helper/resource"
	"github.com/hashicorp/terraform-plugin-testing/plancheck"

	"github.com/RJ-s-Org/terraform-provider-mockingbird/internal/client"
)

func mockConfig(name, persona, contractFile string) string {
	return fmt.Sprintf(`
resource "mockingbird_mock" "test" {
  name         = %[1]q
  route_prefix = "/%[1]s"
  hosts        = ["api.%[1]s.test"]
  persona      = %[2]q
  consistency  = "on"

  chaos = {
    error_rate = 0.05
    latency_jitter = { min_ms = 10, max_ms = 50 }
  }
  limits = {
    rate_limit        = { requests_per_second = 50, burst = 100 }
    max_request_bytes = 65536
  }
  webhooks = [{
    url    = "https://hooks.example.test/in?token=abc"
    events = ["createOrder"]
    secret = "env:MB_WEBHOOK_TEST"
  }]

  contract = {
    file = %[3]q
  }
}

data "mockingbird_mock" "test" {
  name       = mockingbird_mock.test.name
  depends_on = [mockingbird_mock.test]
}
`, name, persona, contractFile)
}

func TestAccMock_lifecycle(t *testing.T) {
	name := randName("tfmock")
	v31 := testdata(t, "orders-3.1.yaml")
	v30 := testdata(t, "orders-3.0.yaml")
	resource.Test(t, resource.TestCase{
		PreCheck:                 func() { testAccPreCheck(t) },
		ProtoV6ProviderFactories: testAccProviders,
		Steps: []resource.TestStep{
			{
				Config: mockConfig(name, "A terse orders API", v31),
				Check: resource.ComposeAggregateTestCheckFunc(
					resource.TestCheckResourceAttr("mockingbird_mock.test", "id", "default/"+name),
					resource.TestCheckResourceAttr("mockingbird_mock.test", "workspace", "default"),
					resource.TestCheckResourceAttr("mockingbird_mock.test", "active_version", "1"),
					resource.TestCheckResourceAttr("mockingbird_mock.test", "contract_version", "1"),
					resource.TestCheckResourceAttr("mockingbird_mock.test", "protocols.#", "1"),
					resource.TestCheckResourceAttr("mockingbird_mock.test", "protocols.0", "http"),
					resource.TestCheckResourceAttr("mockingbird_mock.test", "header_overrides", "true"),
					resource.TestCheckResourceAttr("mockingbird_mock.test", "unknown_ids", "generate"),
					resource.TestCheckResourceAttr("mockingbird_mock.test", "webhooks.0.url", "https://hooks.example.test/in?token=abc"),
					resource.TestMatchResourceAttr("mockingbird_mock.test", "url", regexp.MustCompile(`^http://127\.0\.0\.1:\d+/`+name+`$`)),
					resource.TestMatchResourceAttr("mockingbird_mock.test", "contract_sha256", regexp.MustCompile(`^[0-9a-f]{64}$`)),
					resource.TestCheckResourceAttrPair("data.mockingbird_mock.test", "contract_sha256", "mockingbird_mock.test", "contract_sha256"),
					resource.TestCheckResourceAttr("data.mockingbird_mock.test", "operations.#", "5"),
					resource.TestCheckResourceAttr("data.mockingbird_mock.test", "source", "api"),
				),
			},
			// Settings change, same contract: patched in place, no new version.
			{
				Config: mockConfig(name, "A chatty orders API", v31),
				ConfigPlanChecks: resource.ConfigPlanChecks{
					PreApply: []plancheck.PlanCheck{
						plancheck.ExpectResourceAction("mockingbird_mock.test", plancheck.ResourceActionUpdate),
						plancheck.ExpectKnownValue("mockingbird_mock.test", tfjsonPath("contract_version"), knownInt(1)),
					},
				},
				Check: resource.ComposeAggregateTestCheckFunc(
					resource.TestCheckResourceAttr("mockingbird_mock.test", "persona", "A chatty orders API"),
					resource.TestCheckResourceAttr("mockingbird_mock.test", "active_version", "1"),
				),
			},
			// A new source: uploaded as version 2 and activated.
			{
				Config: mockConfig(name, "A chatty orders API", v30),
				Check: resource.ComposeAggregateTestCheckFunc(
					resource.TestCheckResourceAttr("mockingbird_mock.test", "active_version", "2"),
					resource.TestCheckResourceAttr("mockingbird_mock.test", "contract_version", "2"),
				),
			},
			// Someone activates version 1 out of band: the hash differs, so
			// the provider uploads the configured source again.
			{
				PreConfig: func() {
					apiDo(t, http.MethodPost, client.WsPath("default", "/mocks/"+name+"/contracts/1/activate"), nil, nil)
				},
				Config: mockConfig(name, "A chatty orders API", v30),
				ConfigPlanChecks: resource.ConfigPlanChecks{
					PreApply: []plancheck.PlanCheck{plancheck.ExpectResourceAction("mockingbird_mock.test", plancheck.ResourceActionUpdate)},
				},
				Check: resource.TestCheckResourceAttr("mockingbird_mock.test", "active_version", "3"),
			},
			{
				ResourceName:      "mockingbird_mock.test",
				ImportState:       true,
				ImportStateId:     "default/" + name,
				ImportStateVerify: true,
				// The API never shows a webhook URL's query.
				ImportStateVerifyIgnore: []string{"contract", "contract_version", "webhooks.0.url"},
			},
		},
	})
}

func TestAccMock_inlineAndURL(t *testing.T) {
	name := randName("tfmock")
	src, err := os.ReadFile(testdata(t, "starwars.graphql"))
	if err != nil {
		t.Fatal(err)
	}
	body := string(src)
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		_, _ = w.Write([]byte(body))
	}))
	defer srv.Close()

	inline := fmt.Sprintf(`
resource "mockingbird_mock" "test" {
  name     = %q
  limits   = {}
  contract = { content = file(%q), activate = true }
}
`, name, testdata(t, "starwars.graphql"))
	byURL := fmt.Sprintf(`
resource "mockingbird_mock" "test" {
  name     = %q
  contract = { url = %q }
}
`, name, srv.URL+"/schema.graphql")

	resource.Test(t, resource.TestCase{
		PreCheck:                 func() { testAccPreCheck(t) },
		ProtoV6ProviderFactories: testAccProviders,
		Steps: []resource.TestStep{
			{
				Config: inline,
				Check: resource.ComposeAggregateTestCheckFunc(
					resource.TestCheckResourceAttr("mockingbird_mock.test", "route_prefix", "/"+name),
					resource.TestCheckResourceAttr("mockingbird_mock.test", "protocols.0", "graphql"),
					resource.TestCheckResourceAttr("mockingbird_mock.test", "active_version", "1"),
				),
			},
			// Same bytes from a URL: nothing to upload.
			{
				Config: byURL,
				ConfigPlanChecks: resource.ConfigPlanChecks{
					PreApply: []plancheck.PlanCheck{plancheck.ExpectKnownValue("mockingbird_mock.test", tfjsonPath("contract_version"), knownInt(1))},
				},
				Check: resource.TestCheckResourceAttr("mockingbird_mock.test", "active_version", "1"),
			},
			// The document behind the URL changes: a new version.
			{
				PreConfig: func() { body += "\ntype Extra { id: ID! }\n" },
				Config:    byURL,
				ConfigPlanChecks: resource.ConfigPlanChecks{
					PreApply: []plancheck.PlanCheck{plancheck.ExpectResourceAction("mockingbird_mock.test", plancheck.ResourceActionUpdate)},
				},
				Check: resource.TestCheckResourceAttr("mockingbird_mock.test", "active_version", "2"),
			},
		},
	})
}

func TestAccMock_invalidContract(t *testing.T) {
	name := randName("tfmock")
	resource.Test(t, resource.TestCase{
		PreCheck:                 func() { testAccPreCheck(t) },
		ProtoV6ProviderFactories: testAccProviders,
		Steps: []resource.TestStep{
			{
				Config: fmt.Sprintf(`
resource "mockingbird_mock" "test" {
  name     = %q
  contract = { content = "this is not a contract" }
}
`, name),
				ExpectError: regexp.MustCompile(`(?s)Uploading contract failed.*422`),
			},
		},
	})
}
