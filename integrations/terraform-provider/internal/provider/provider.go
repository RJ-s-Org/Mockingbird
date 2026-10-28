// Package provider implements the Mockingbird Terraform and OpenTofu
// provider on the Admin API.
package provider

import (
	"context"
	"os"
	"strings"

	"github.com/hashicorp/terraform-plugin-framework/datasource"
	"github.com/hashicorp/terraform-plugin-framework/path"
	"github.com/hashicorp/terraform-plugin-framework/provider"
	"github.com/hashicorp/terraform-plugin-framework/provider/schema"
	"github.com/hashicorp/terraform-plugin-framework/resource"
	"github.com/hashicorp/terraform-plugin-framework/types"

	"github.com/RJ-s-Org/terraform-provider-mockingbird/internal/client"
)

// DefaultEndpoint is the Admin API of a local `mockingbird serve`.
const DefaultEndpoint = "http://127.0.0.1:9090"

// DefaultWorkspace is used when neither the provider nor a resource names one.
const DefaultWorkspace = "default"

var _ provider.Provider = (*mockingbirdProvider)(nil)

type mockingbirdProvider struct {
	version string
}

// New returns a provider factory for version.
func New(version string) func() provider.Provider {
	return func() provider.Provider {
		return &mockingbirdProvider{version: version}
	}
}

type providerModel struct {
	Endpoint     types.String `tfsdk:"endpoint"`
	Token        types.String `tfsdk:"token"`
	Workspace    types.String `tfsdk:"workspace"`
	DataPlaneURL types.String `tfsdk:"data_plane_url"`
}

// providerData is handed to every resource and data source.
type providerData struct {
	client       *client.Client
	workspace    string
	dataPlaneURL string
}

func (p *mockingbirdProvider) Metadata(_ context.Context, _ provider.MetadataRequest, resp *provider.MetadataResponse) {
	resp.TypeName = "mockingbird"
	resp.Version = p.version
}

func (p *mockingbirdProvider) Schema(_ context.Context, _ provider.SchemaRequest, resp *provider.SchemaResponse) {
	resp.Schema = schema.Schema{
		MarkdownDescription: "Manages [Mockingbird](https://github.com/RJ-s-Org/Mockingbird) mocks, contracts, scenarios, LLM profiles and API keys through the Admin API, so mocked lower environments can be created from code.",
		Attributes: map[string]schema.Attribute{
			"endpoint": schema.StringAttribute{
				MarkdownDescription: "Admin API origin, e.g. `https://mockingbird.staging.example.com:9090`. Defaults to `MB_ADMIN_URL`, then `" + DefaultEndpoint + "`.",
				Optional:            true,
			},
			"token": schema.StringAttribute{
				MarkdownDescription: "Bearer credential for the Admin API: an API key (`mbk_…`) or an OIDC access token. Defaults to `MB_ADMIN_TOKEN`. Not needed when the server runs with `auth.mode: none`.",
				Optional:            true,
				Sensitive:           true,
			},
			"workspace": schema.StringAttribute{
				MarkdownDescription: "Workspace that resources are created in unless they set `workspace`. Defaults to `MB_WORKSPACE`, then `default`.",
				Optional:            true,
			},
			"data_plane_url": schema.StringAttribute{
				MarkdownDescription: "Origin the data plane is reached at, e.g. `http://mockingbird.staging:8080`. When set, `mockingbird_mock` exports `url` (this origin plus the route prefix). Defaults to `MB_DATA_PLANE_URL`.",
				Optional:            true,
			},
		},
	}
}

func firstNonEmpty(vals ...string) string {
	for _, v := range vals {
		if v != "" {
			return v
		}
	}
	return ""
}

func (p *mockingbirdProvider) Configure(ctx context.Context, req provider.ConfigureRequest, resp *provider.ConfigureResponse) {
	var cfg providerModel
	resp.Diagnostics.Append(req.Config.Get(ctx, &cfg)...)
	if resp.Diagnostics.HasError() {
		return
	}
	for name, v := range map[string]types.String{"endpoint": cfg.Endpoint, "token": cfg.Token, "workspace": cfg.Workspace, "data_plane_url": cfg.DataPlaneURL} {
		if v.IsUnknown() {
			resp.Diagnostics.AddAttributeError(path.Root(name), "Unknown provider setting",
				"The provider cannot be configured with a value that is only known after apply. Set "+name+" statically or through its environment variable.")
		}
	}
	if resp.Diagnostics.HasError() {
		return
	}
	endpoint := firstNonEmpty(cfg.Endpoint.ValueString(), os.Getenv("MB_ADMIN_URL"), DefaultEndpoint)
	token := firstNonEmpty(cfg.Token.ValueString(), os.Getenv("MB_ADMIN_TOKEN"))
	ws := firstNonEmpty(cfg.Workspace.ValueString(), os.Getenv("MB_WORKSPACE"), DefaultWorkspace)
	dp := strings.TrimRight(firstNonEmpty(cfg.DataPlaneURL.ValueString(), os.Getenv("MB_DATA_PLANE_URL")), "/")
	data := &providerData{
		client:       client.New(endpoint, token, "terraform-provider-mockingbird/"+p.version),
		workspace:    ws,
		dataPlaneURL: dp,
	}
	resp.DataSourceData = data
	resp.ResourceData = data
}

func (p *mockingbirdProvider) Resources(_ context.Context) []func() resource.Resource {
	return []func() resource.Resource{
		NewWorkspaceResource,
		NewMockResource,
		NewContractVersionResource,
		NewScenarioResource,
		NewLLMProfileResource,
		NewAPIKeyResource,
		NewChannelResource,
		NewEventSettingsResource,
	}
}

func (p *mockingbirdProvider) DataSources(_ context.Context) []func() datasource.DataSource {
	return []func() datasource.DataSource{
		NewMockDataSource,
		NewWorkspaceDataSource,
	}
}
