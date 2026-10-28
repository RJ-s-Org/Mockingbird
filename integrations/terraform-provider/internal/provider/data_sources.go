package provider

import (
	"context"
	"net/http"

	"github.com/hashicorp/terraform-plugin-framework/datasource"
	"github.com/hashicorp/terraform-plugin-framework/datasource/schema"
	"github.com/hashicorp/terraform-plugin-framework/types"

	"github.com/RJ-s-Org/terraform-provider-mockingbird/internal/client"
)

var (
	_ datasource.DataSourceWithConfigure = (*mockDataSource)(nil)
	_ datasource.DataSourceWithConfigure = (*workspaceDataSource)(nil)
)

// NewMockDataSource is the `mockingbird_mock` data source.
func NewMockDataSource() datasource.DataSource { return &mockDataSource{} }

type mockDataSource struct{ pd *providerData }

type mockDataModel struct {
	ID              types.String `tfsdk:"id"`
	Workspace       types.String `tfsdk:"workspace"`
	Name            types.String `tfsdk:"name"`
	RoutePrefix     types.String `tfsdk:"route_prefix"`
	Hosts           types.List   `tfsdk:"hosts"`
	Persona         types.String `tfsdk:"persona"`
	LLMProfile      types.String `tfsdk:"llm_profile"`
	HeaderOverrides types.Bool   `tfsdk:"header_overrides"`
	Consistency     types.String `tfsdk:"consistency"`
	UnknownIDs      types.String `tfsdk:"unknown_ids"`
	DefaultScenario types.String `tfsdk:"default_scenario"`
	DataPlaneAuth   types.String `tfsdk:"data_plane_auth_mode"`
	Source          types.String `tfsdk:"source"`
	ActiveVersion   types.Int64  `tfsdk:"active_version"`
	ContractSHA256  types.String `tfsdk:"contract_sha256"`
	Protocols       types.List   `tfsdk:"protocols"`
	Operations      types.List   `tfsdk:"operations"`
	URL             types.String `tfsdk:"url"`
}

func (d *mockDataSource) Metadata(_ context.Context, req datasource.MetadataRequest, resp *datasource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_mock"
}

func (d *mockDataSource) Schema(_ context.Context, _ datasource.SchemaRequest, resp *datasource.SchemaResponse) {
	str := func(desc string) schema.StringAttribute {
		return schema.StringAttribute{Computed: true, MarkdownDescription: desc}
	}
	resp.Schema = schema.Schema{
		MarkdownDescription: "Reads a mock, including mocks declared in the server's config file (`source = \"file\"`), which resources cannot manage.",
		Attributes: map[string]schema.Attribute{
			"id":                   str("`<workspace>/<name>`."),
			"workspace":            schema.StringAttribute{Optional: true, Computed: true, MarkdownDescription: "Workspace. Defaults to the provider's."},
			"name":                 schema.StringAttribute{Required: true, MarkdownDescription: "Mock name."},
			"route_prefix":         str("Path prefix on the data plane."),
			"hosts":                schema.ListAttribute{Computed: true, ElementType: types.StringType, MarkdownDescription: "Host names the mock answers for; empty for any."},
			"persona":              str("Persona."),
			"llm_profile":          str("LLM profile."),
			"header_overrides":     schema.BoolAttribute{Computed: true, MarkdownDescription: "Whether `X-Mockingbird-*` headers are honoured."},
			"consistency":          str("`on` or `off`."),
			"unknown_ids":          str("`generate` or `404`."),
			"default_scenario":     str("Scenario applied when a call names none."),
			"data_plane_auth_mode": str("`none`, `api_key` or `passthrough_validate`."),
			"source":               str("`api` or `file`."),
			"active_version":       schema.Int64Attribute{Computed: true, MarkdownDescription: "Active contract version; null when none."},
			"contract_sha256":      str("Hex sha256 of the active contract source."),
			"protocols":            schema.ListAttribute{Computed: true, ElementType: types.StringType, MarkdownDescription: "Protocols of the active contract."},
			"operations":           schema.ListAttribute{Computed: true, ElementType: types.StringType, MarkdownDescription: "Operations of the active contract, as targets (`GET /orders/{id}`)."},
			"url":                  str("The provider's `data_plane_url` plus the route prefix; null without `data_plane_url`."),
		},
	}
}

func (d *mockDataSource) Configure(_ context.Context, req datasource.ConfigureRequest, resp *datasource.ConfigureResponse) {
	d.pd = providerDataFrom(req.ProviderData, &resp.Diagnostics)
}

func (d *mockDataSource) Read(ctx context.Context, req datasource.ReadRequest, resp *datasource.ReadResponse) {
	var m mockDataModel
	resp.Diagnostics.Append(req.Config.Get(ctx, &m)...)
	if resp.Diagnostics.HasError() {
		return
	}
	ws := d.pd.workspaceOr(m.Workspace)
	var v client.Mock
	if _, err := d.pd.client.Do(ctx, http.MethodGet, mockPath(ws, m.Name.ValueString()), nil, &v); err != nil {
		resp.Diagnostics.AddError("Reading mock failed", err.Error())
		return
	}
	m.ID = types.StringValue(v.Workspace + "/" + v.Name)
	m.Workspace = types.StringValue(v.Workspace)
	m.RoutePrefix = types.StringValue(v.RoutePrefix)
	m.Hosts = listValue(v.Hosts, false)
	m.Persona = strValue(v.Persona)
	m.LLMProfile = strValue(v.LLMProfile)
	m.HeaderOverrides = types.BoolValue(v.HeaderOverrides)
	m.Consistency = types.StringValue(v.Consistency)
	m.UnknownIDs = types.StringValue(v.UnknownIDs)
	m.DefaultScenario = strValue(v.DefaultScenario)
	m.DataPlaneAuth = types.StringValue(v.DataPlaneAuth.Mode)
	m.Source = types.StringValue(v.Source)
	m.ActiveVersion = int64Value(v.ActiveVersion)
	m.URL = d.pd.mockURL(v.RoutePrefix)
	m.ContractSHA256 = types.StringNull()
	m.Protocols = listValue(nil, false)
	m.Operations = listValue(nil, false)
	if v.ActiveVersion != nil {
		var cv client.ContractVersion
		if _, err := d.pd.client.Do(ctx, http.MethodGet, versionPath(v.Workspace, v.Name, *v.ActiveVersion), nil, &cv); err != nil {
			resp.Diagnostics.AddError("Reading active contract failed", err.Error())
			return
		}
		m.ContractSHA256 = types.StringValue(cv.SourceDigest)
		m.Protocols = listValue(protocolsOf(&cv), false)
		targets := make([]string, 0, len(cv.Operations))
		for _, op := range cv.Operations {
			targets = append(targets, op.Target)
		}
		m.Operations = listValue(targets, false)
	}
	resp.Diagnostics.Append(resp.State.Set(ctx, &m)...)
}

// NewWorkspaceDataSource is the `mockingbird_workspace` data source.
func NewWorkspaceDataSource() datasource.DataSource { return &workspaceDataSource{} }

type workspaceDataSource struct{ pd *providerData }

type workspaceDataModel struct {
	ID          types.String `tfsdk:"id"`
	Name        types.String `tfsdk:"name"`
	DisplayName types.String `tfsdk:"display_name"`
	CreatedAt   types.String `tfsdk:"created_at"`
	Role        types.String `tfsdk:"role"`
}

func (d *workspaceDataSource) Metadata(_ context.Context, req datasource.MetadataRequest, resp *datasource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_workspace"
}

func (d *workspaceDataSource) Schema(_ context.Context, _ datasource.SchemaRequest, resp *datasource.SchemaResponse) {
	resp.Schema = schema.Schema{
		MarkdownDescription: "Reads a workspace the caller has a role in.",
		Attributes: map[string]schema.Attribute{
			"id":           schema.StringAttribute{Computed: true, MarkdownDescription: "The workspace name."},
			"name":         schema.StringAttribute{Optional: true, Computed: true, MarkdownDescription: "Workspace name. Defaults to the provider's."},
			"display_name": schema.StringAttribute{Computed: true, MarkdownDescription: "Human-readable name."},
			"created_at":   schema.StringAttribute{Computed: true, MarkdownDescription: "Creation time (RFC 3339)."},
			"role":         schema.StringAttribute{Computed: true, MarkdownDescription: "The caller's role in the workspace (`viewer`, `editor`, `admin` or `owner`); null for a sysadmin without one."},
		},
	}
}

func (d *workspaceDataSource) Configure(_ context.Context, req datasource.ConfigureRequest, resp *datasource.ConfigureResponse) {
	d.pd = providerDataFrom(req.ProviderData, &resp.Diagnostics)
}

func (d *workspaceDataSource) Read(ctx context.Context, req datasource.ReadRequest, resp *datasource.ReadResponse) {
	var m workspaceDataModel
	resp.Diagnostics.Append(req.Config.Get(ctx, &m)...)
	if resp.Diagnostics.HasError() {
		return
	}
	name := d.pd.workspaceOr(m.Name)
	var w struct {
		client.Workspace
		Role *string `json:"role"`
	}
	if _, err := d.pd.client.Do(ctx, http.MethodGet, "/api/v1/workspaces/"+client.Seg(name), nil, &w); err != nil {
		resp.Diagnostics.AddError("Reading workspace failed", err.Error())
		return
	}
	m.ID = types.StringValue(w.Name)
	m.Name = types.StringValue(w.Name)
	m.DisplayName = types.StringValue(w.DisplayName)
	m.CreatedAt = types.StringValue(w.CreatedAt)
	m.Role = strValue(w.Role)
	resp.Diagnostics.Append(resp.State.Set(ctx, &m)...)
}
