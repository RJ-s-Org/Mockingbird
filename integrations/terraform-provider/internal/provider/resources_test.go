package provider

import (
	"fmt"
	"net/http"
	"regexp"
	"testing"

	"github.com/hashicorp/terraform-plugin-testing/helper/resource"
	"github.com/hashicorp/terraform-plugin-testing/knownvalue"
	"github.com/hashicorp/terraform-plugin-testing/plancheck"
	"github.com/hashicorp/terraform-plugin-testing/tfjsonpath"

	"github.com/RJ-s-Org/terraform-provider-mockingbird/internal/client"
)

func tfjsonPath(attr string) tfjsonpath.Path { return tfjsonpath.New(attr) }

func knownInt(n int64) knownvalue.Check { return knownvalue.Int64Exact(n) }

func TestAccWorkspace(t *testing.T) {
	ws := randName("tfws")
	cfg := func(display string) string {
		return fmt.Sprintf(`
resource "mockingbird_workspace" "test" {
  name          = %q
  display_name  = %q
  force_destroy = true
}

resource "mockingbird_mock" "in_ws" {
  workspace = mockingbird_workspace.test.name
  name      = "orders"
  route_prefix = "/%[1]s-orders"
}

data "mockingbird_workspace" "test" {
  name = mockingbird_workspace.test.name
}
`, ws, display)
	}
	resource.Test(t, resource.TestCase{
		PreCheck:                 func() { testAccPreCheck(t) },
		ProtoV6ProviderFactories: testAccProviders,
		Steps: []resource.TestStep{
			{
				Config: cfg("Staging"),
				Check: resource.ComposeAggregateTestCheckFunc(
					resource.TestCheckResourceAttr("mockingbird_workspace.test", "id", ws),
					resource.TestCheckResourceAttr("mockingbird_workspace.test", "display_name", "Staging"),
					resource.TestCheckResourceAttrSet("mockingbird_workspace.test", "created_at"),
					resource.TestCheckResourceAttr("mockingbird_mock.in_ws", "id", ws+"/orders"),
					resource.TestCheckResourceAttr("data.mockingbird_workspace.test", "display_name", "Staging"),
				),
			},
			{
				Config: cfg("Staging (EU)"),
				Check:  resource.TestCheckResourceAttr("mockingbird_workspace.test", "display_name", "Staging (EU)"),
			},
			{
				ResourceName:            "mockingbird_workspace.test",
				ImportState:             true,
				ImportStateVerify:       true,
				ImportStateVerifyIgnore: []string{"force_destroy"},
			},
			{
				ResourceName:      "mockingbird_mock.in_ws",
				ImportState:       true,
				ImportStateId:     ws + "/orders",
				ImportStateVerify: true,
			},
		},
	})
}

func TestAccContractVersion(t *testing.T) {
	name := randName("tfcv")
	cfg := func(file string, activate bool) string {
		return fmt.Sprintf(`
resource "mockingbird_mock" "test" {
  name = %q
}

resource "mockingbird_contract_version" "test" {
  mock     = mockingbird_mock.test.name
  file     = %q
  activate = %t
}
`, name, file, activate)
	}
	v31, v30 := testdata(t, "orders-3.1.yaml"), testdata(t, "orders-3.0.yaml")
	resource.Test(t, resource.TestCase{
		PreCheck:                 func() { testAccPreCheck(t) },
		ProtoV6ProviderFactories: testAccProviders,
		Steps: []resource.TestStep{
			{
				Config: cfg(v31, true),
				Check: resource.ComposeAggregateTestCheckFunc(
					resource.TestCheckResourceAttr("mockingbird_contract_version.test", "version", "1"),
					resource.TestCheckResourceAttr("mockingbird_contract_version.test", "status", "active"),
					resource.TestCheckResourceAttr("mockingbird_contract_version.test", "operations.#", "5"),
					resource.TestCheckResourceAttr("mockingbird_contract_version.test", "id", "default/"+name+"/1"),
				),
			},
			// A new source replaces the version.
			{
				Config: cfg(v30, true),
				ConfigPlanChecks: resource.ConfigPlanChecks{
					PreApply: []plancheck.PlanCheck{plancheck.ExpectResourceAction("mockingbird_contract_version.test", plancheck.ResourceActionDestroyBeforeCreate)},
				},
				Check: resource.ComposeAggregateTestCheckFunc(
					resource.TestCheckResourceAttr("mockingbird_contract_version.test", "version", "2"),
					resource.TestCheckResourceAttr("mockingbird_contract_version.test", "status", "active"),
				),
			},
			// Another version activated out of band: activated again.
			{
				PreConfig: func() {
					apiDo(t, http.MethodPost, client.WsPath("default", "/mocks/"+name+"/contracts/1/activate"), nil, nil)
				},
				Config: cfg(v30, true),
				ConfigPlanChecks: resource.ConfigPlanChecks{
					PreApply: []plancheck.PlanCheck{plancheck.ExpectResourceAction("mockingbird_contract_version.test", plancheck.ResourceActionUpdate)},
				},
				Check: resource.TestCheckResourceAttr("mockingbird_contract_version.test", "status", "active"),
			},
			{
				ResourceName:            "mockingbird_contract_version.test",
				ImportState:             true,
				ImportStateId:           "default/" + name + "/2",
				ImportStateVerify:       true,
				ImportStateVerifyIgnore: []string{"file"},
			},
		},
	})
}

func TestAccScenario(t *testing.T) {
	name := randName("tfsc")
	cfg := func(status int, instr string) string {
		return fmt.Sprintf(`
resource "mockingbird_mock" "test" {
  name     = %q
  contract = { file = %q }
}

resource "mockingbird_scenario" "declined" {
  mock         = mockingbird_mock.test.name
  name         = "declined"
  instructions = %q
  status       = %d
  latency_ms   = 25
  operations   = ["createOrder"]
  set          = jsonencode({ status = "declined" })
  chaos        = { error_rate = 0.5 }
  operation_instructions = {
    createOrder = "Explain that the card was declined."
  }
}
`, name, testdata(t, "orders-3.1.yaml"), instr, status)
	}
	resource.Test(t, resource.TestCase{
		PreCheck:                 func() { testAccPreCheck(t) },
		ProtoV6ProviderFactories: testAccProviders,
		Steps: []resource.TestStep{
			{
				Config: cfg(503, "The payment processor is down."),
				Check: resource.ComposeAggregateTestCheckFunc(
					resource.TestCheckResourceAttr("mockingbird_scenario.declined", "id", "default/"+name+"/declined"),
					resource.TestCheckResourceAttr("mockingbird_scenario.declined", "status", "503"),
					resource.TestCheckResourceAttr("mockingbird_scenario.declined", "operations.0", "createOrder"),
					resource.TestCheckResourceAttr("mockingbird_scenario.declined", "set", `{"status":"declined"}`),
				),
			},
			{
				Config: cfg(500, "Fail every order."),
				Check:  resource.TestCheckResourceAttr("mockingbird_scenario.declined", "instructions", "Fail every order."),
			},
			{
				ResourceName:      "mockingbird_scenario.declined",
				ImportState:       true,
				ImportStateId:     name + "/declined",
				ImportStateVerify: true,
			},
			// A status no operation declares is refused by the server.
			{
				Config:      cfg(418, "Teapot."),
				ExpectError: regexp.MustCompile(`422`),
			},
		},
	})
}

func TestAccLLMProfile(t *testing.T) {
	name := randName("tfllm")
	cfg := func(model string) string {
		return fmt.Sprintf(`
resource "mockingbird_llm_profile" "test" {
  name        = %q
  base_url    = "https://openrouter.ai/api/v1"
  model       = %q
  secret_ref  = "env:MB_LLM_API_KEY"
  json_schema = true
  temperature = 0.2
  headers     = { "x-title" = "Mockingbird staging" }
  extra_body  = jsonencode({ provider = { sort = "price" } })
}

resource "mockingbird_mock" "test" {
  name        = "%[1]s-mock"
  llm_profile = mockingbird_llm_profile.test.name
}
`, name, model)
	}
	resource.Test(t, resource.TestCase{
		PreCheck:                 func() { testAccPreCheck(t) },
		ProtoV6ProviderFactories: testAccProviders,
		Steps: []resource.TestStep{
			{
				Config: cfg("deepseek/deepseek-v4.1-flash"),
				Check: resource.ComposeAggregateTestCheckFunc(
					resource.TestCheckResourceAttr("mockingbird_llm_profile.test", "secret_ref", "env:MB_LLM_API_KEY"),
					resource.TestCheckResourceAttr("mockingbird_llm_profile.test", "temperature", "0.2"),
					resource.TestCheckResourceAttr("mockingbird_llm_profile.test", "max_tokens", "16384"),
					resource.TestCheckResourceAttr("mockingbird_mock.test", "llm_profile", name),
				),
			},
			{
				Config: cfg("google/gemini-2.5-flash"),
				Check:  resource.TestCheckResourceAttr("mockingbird_llm_profile.test", "model", "google/gemini-2.5-flash"),
			},
			{
				ResourceName:      "mockingbird_llm_profile.test",
				ImportState:       true,
				ImportStateVerify: true,
			},
		},
	})
}

// A raw key is refused before anything reaches the server.
func TestAccLLMProfile_rawKeyRejected(t *testing.T) {
	resource.Test(t, resource.TestCase{
		PreCheck:                 func() { testAccPreCheck(t) },
		ProtoV6ProviderFactories: testAccProviders,
		Steps: []resource.TestStep{
			{
				Config: `
resource "mockingbird_llm_profile" "test" {
  name       = "raw"
  base_url   = "https://openrouter.ai/api/v1"
  model      = "deepseek/deepseek-v4.1-flash"
  secret_ref = "sk-not-a-reference"
}
`,
				ExpectError: regexp.MustCompile(`must be a secret reference`),
			},
		},
	})
}

func TestAccAPIKey(t *testing.T) {
	name := randName("tfkey")
	resource.Test(t, resource.TestCase{
		PreCheck:                 func() { testAccPreCheck(t) },
		ProtoV6ProviderFactories: testAccProviders,
		Steps: []resource.TestStep{
			{
				Config: fmt.Sprintf(`
resource "mockingbird_api_key" "app" {
  name       = %q
  role       = "viewer"
  scopes     = ["data-plane"]
  expires_at = "2030-01-01T00:00:00Z"
}

resource "mockingbird_mock" "test" {
  name            = "%[1]s-mock"
  data_plane_auth = { mode = "api_key", header = "x-mock-key" }
}
`, name),
				Check: resource.ComposeAggregateTestCheckFunc(
					resource.TestMatchResourceAttr("mockingbird_api_key.app", "token", regexp.MustCompile(`^mbk_`)),
					resource.TestCheckResourceAttrSet("mockingbird_api_key.app", "key_id"),
					resource.TestCheckResourceAttr("mockingbird_api_key.app", "scopes.0", "data-plane"),
					resource.TestCheckResourceAttr("mockingbird_mock.test", "data_plane_auth.mode", "api_key"),
				),
			},
			{
				ResourceName:            "mockingbird_api_key.app",
				ImportState:             true,
				ImportStateVerify:       true,
				ImportStateVerifyIgnore: []string{"token"},
			},
		},
	})
}

func TestAccChannelAndEventSettings(t *testing.T) {
	name := randName("tfch")
	cfg := func(enabled bool, triggers string) string {
		return fmt.Sprintf(`
resource "mockingbird_mock" "chat" {
  name     = %q
  contract = { file = %q }
}

resource "mockingbird_channel" "messages" {
  mock     = mockingbird_mock.chat.name
  channel  = "chatMessage"
  enabled  = %t
  triggers = %s
}

resource "mockingbird_event_settings" "chat" {
  mock = mockingbird_mock.chat.name
}
`, name, testdata(t, "chat-ws-2.6.yaml"), enabled, triggers)
	}
	resource.Test(t, resource.TestCase{
		PreCheck:                 func() { testAccPreCheck(t) },
		ProtoV6ProviderFactories: testAccProviders,
		Steps: []resource.TestStep{
			{
				Config: cfg(true, `jsonencode([{ schedule = { every_ms = 60000 } }])`),
				Check: resource.ComposeAggregateTestCheckFunc(
					resource.TestCheckResourceAttr("mockingbird_channel.messages", "protocol", "websocket"),
					resource.TestCheckResourceAttr("mockingbird_channel.messages", "direction", "publish"),
					resource.TestCheckResourceAttr("mockingbird_channel.messages", "address", "/chat"),
					resource.TestMatchResourceAttr("mockingbird_channel.messages", "effective_triggers", regexp.MustCompile(`every_ms`)),
					resource.TestCheckResourceAttr("mockingbird_event_settings.chat", "id", "default/"+name),
				),
			},
			{
				Config: cfg(false, "null"),
				Check: resource.ComposeAggregateTestCheckFunc(
					resource.TestCheckResourceAttr("mockingbird_channel.messages", "enabled", "false"),
					resource.TestCheckResourceAttr("mockingbird_channel.messages", "effective_triggers", `["manual"]`),
				),
			},
			{
				ResourceName:      "mockingbird_channel.messages",
				ImportState:       true,
				ImportStateId:     name + "/chatMessage",
				ImportStateVerify: true,
			},
			{
				ResourceName:      "mockingbird_event_settings.chat",
				ImportState:       true,
				ImportStateId:     name,
				ImportStateVerify: true,
			},
		},
	})
}
