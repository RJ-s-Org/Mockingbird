package provider

import (
	"context"
	"net/http"

	"github.com/hashicorp/terraform-plugin-framework-validators/stringvalidator"
	"github.com/hashicorp/terraform-plugin-framework/diag"
	"github.com/hashicorp/terraform-plugin-framework/path"
	"github.com/hashicorp/terraform-plugin-framework/resource"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/booldefault"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/listplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/stringdefault"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/stringplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/schema/validator"
	"github.com/hashicorp/terraform-plugin-framework/types"

	"github.com/RJ-s-Org/terraform-provider-mockingbird/internal/client"
)

var (
	_ resource.ResourceWithConfigure   = (*mockResource)(nil)
	_ resource.ResourceWithImportState = (*mockResource)(nil)
	_ resource.ResourceWithModifyPlan  = (*mockResource)(nil)
)

// NewMockResource is the `mockingbird_mock` resource.
func NewMockResource() resource.Resource { return &mockResource{} }

type mockResource struct{ pd *providerData }

type mockModel struct {
	ID              types.String        `tfsdk:"id"`
	Workspace       types.String        `tfsdk:"workspace"`
	Name            types.String        `tfsdk:"name"`
	RoutePrefix     types.String        `tfsdk:"route_prefix"`
	Hosts           types.List          `tfsdk:"hosts"`
	Persona         types.String        `tfsdk:"persona"`
	LLMProfile      types.String        `tfsdk:"llm_profile"`
	HeaderOverrides types.Bool          `tfsdk:"header_overrides"`
	Consistency     types.String        `tfsdk:"consistency"`
	UnknownIDs      types.String        `tfsdk:"unknown_ids"`
	DataPlaneAuth   *dataPlaneAuthModel `tfsdk:"data_plane_auth"`
	Chaos           *chaosModel         `tfsdk:"chaos"`
	Limits          *limitsModel        `tfsdk:"limits"`
	Webhooks        []webhookModel      `tfsdk:"webhooks"`
	Contract        *contractModel      `tfsdk:"contract"`
	ContractSHA256  types.String        `tfsdk:"contract_sha256"`
	ContractVersion types.Int64         `tfsdk:"contract_version"`
	ActiveVersion   types.Int64         `tfsdk:"active_version"`
	Protocols       types.List          `tfsdk:"protocols"`
	URL             types.String        `tfsdk:"url"`
}

func (r *mockResource) Metadata(_ context.Context, req resource.MetadataRequest, resp *resource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_mock"
}

func (r *mockResource) Schema(_ context.Context, _ resource.SchemaRequest, resp *resource.SchemaResponse) {
	attrs := mockResourceAttributes()
	attrs["id"] = idAttribute("<workspace>/<name>")
	attrs["workspace"] = workspaceAttribute()
	attrs["name"] = schema.StringAttribute{
		MarkdownDescription: "Mock name, `^[a-z0-9][a-z0-9-]{0,62}$`. Changing it replaces the mock.",
		Required:            true,
		PlanModifiers:       []planmodifier.String{stringplanmodifier.RequiresReplace()},
		Validators:          []validator.String{stringvalidator.RegexMatches(regexpMust(`^[a-z0-9][a-z0-9-]{0,62}$`), "must match ^[a-z0-9][a-z0-9-]{0,62}$")},
	}
	attrs["route_prefix"] = schema.StringAttribute{
		MarkdownDescription: "Path prefix the mock is served under on the data plane. Defaults to `/<name>`. Two mocks on overlapping hosts may not share a prefix.",
		Optional:            true,
		Computed:            true,
		PlanModifiers:       []planmodifier.String{stringplanmodifier.UseStateForUnknown()},
	}
	attrs["hosts"] = schema.ListAttribute{
		ElementType:         types.StringType,
		MarkdownDescription: "Host names the mock answers for (lowercase, no port). Unset answers for any host. Mocks with different hosts may share a route prefix.",
		Optional:            true,
	}
	attrs["persona"] = schema.StringAttribute{
		MarkdownDescription: "Persona the LLM answers in, e.g. `A terse payments API for a European marketplace`.",
		Optional:            true,
	}
	attrs["llm_profile"] = schema.StringAttribute{
		MarkdownDescription: "LLM profile the mock generates with (see `mockingbird_llm_profile`). Unset uses the server's default; without any, responses come from the faker.",
		Optional:            true,
	}
	attrs["header_overrides"] = schema.BoolAttribute{
		MarkdownDescription: "Whether callers may steer responses with `X-Mockingbird-*` headers. Default `true`.",
		Optional:            true,
		Computed:            true,
		Default:             booldefault.StaticBool(true),
	}
	attrs["consistency"] = schema.StringAttribute{
		MarkdownDescription: "`on` (default): repeated reads in a session get the same answer and listed items are served by id. `off` generates every answer anew.",
		Optional:            true,
		Computed:            true,
		Default:             stringdefault.StaticString("on"),
		Validators:          []validator.String{stringvalidator.OneOf("on", "off")},
	}
	attrs["unknown_ids"] = schema.StringAttribute{
		MarkdownDescription: "`generate` (default) or `404`: how a read of an id the session does not hold is answered.",
		Optional:            true,
		Computed:            true,
		Default:             stringdefault.StaticString("generate"),
		Validators:          []validator.String{stringvalidator.OneOf("generate", "404")},
	}
	attrs["contract_sha256"] = schema.StringAttribute{
		MarkdownDescription: "Hex sha256 of the contract source: the configured one in a plan, and the served version's `source_digest` after refresh. A difference means a new version is uploaded.",
		Computed:            true,
	}
	attrs["contract_version"] = schema.Int64Attribute{
		MarkdownDescription: "The contract version this resource last uploaded.",
		Computed:            true,
	}
	attrs["active_version"] = schema.Int64Attribute{
		MarkdownDescription: "The active contract version; null until one is activated.",
		Computed:            true,
	}
	attrs["protocols"] = schema.ListAttribute{
		ElementType:         types.StringType,
		MarkdownDescription: "Protocols of the active contract (`http`, `graphql`, `grpc`, `soap`, and event protocols such as `kafka` or `websocket`), sorted.",
		Computed:            true,
		PlanModifiers:       []planmodifier.List{listplanmodifier.UseStateForUnknown()},
	}
	attrs["url"] = schema.StringAttribute{
		MarkdownDescription: "The provider's `data_plane_url` plus the route prefix: the base URL an application points at. Null without `data_plane_url`.",
		Computed:            true,
	}
	resp.Schema = schema.Schema{
		MarkdownDescription: "A mock: an API that Mockingbird serves from a contract, with responses written by an LLM (or the faker when offline) that stay consistent within a session.",
		Attributes:          attrs,
	}
}

func (r *mockResource) Configure(_ context.Context, req resource.ConfigureRequest, resp *resource.ConfigureResponse) {
	r.pd = providerDataFrom(req.ProviderData, &resp.Diagnostics)
}

// ModifyPlan hashes the configured contract, so a changed source plans an
// upload and an unchanged one plans nothing.
func (r *mockResource) ModifyPlan(ctx context.Context, req resource.ModifyPlanRequest, resp *resource.ModifyPlanResponse) {
	if req.Plan.Raw.IsNull() || r.pd == nil {
		return
	}
	var plan mockModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	if resp.Diagnostics.HasError() {
		return
	}
	var state *mockModel
	if !req.State.Raw.IsNull() {
		state = &mockModel{}
		resp.Diagnostics.Append(req.State.Get(ctx, state)...)
		if resp.Diagnostics.HasError() {
			return
		}
	}

	// url follows the route prefix.
	switch {
	case r.pd.dataPlaneURL == "":
		plan.URL = types.StringNull()
	case plan.RoutePrefix.IsUnknown():
		plan.URL = types.StringUnknown()
	default:
		plan.URL = r.pd.mockURL(plan.RoutePrefix.ValueString())
	}

	newUpload := false
	switch {
	case plan.Contract == nil:
		// Without a contract the resource leaves versions alone.
		if state != nil {
			plan.ContractSHA256 = state.ContractSHA256
			plan.ContractVersion = state.ContractVersion
			plan.ActiveVersion = state.ActiveVersion
		} else {
			plan.ContractSHA256 = types.StringNull()
			plan.ContractVersion = types.Int64Null()
			plan.ActiveVersion = types.Int64Unknown()
		}
	case !plan.Contract.source().known():
		newUpload = true
		plan.ContractSHA256 = types.StringUnknown()
	default:
		src, err := plan.Contract.source().load(ctx)
		if err != nil {
			resp.Diagnostics.AddAttributeError(path.Root("contract"), "Cannot read contract", err.Error())
			return
		}
		sum := sha256Hex(src)
		plan.ContractSHA256 = types.StringValue(sum)
		newUpload = state == nil || state.ContractSHA256.ValueString() != sum
		if !newUpload {
			plan.ContractVersion = state.ContractVersion
			plan.ActiveVersion = state.ActiveVersion
		}
	}
	if newUpload {
		plan.ContractVersion = types.Int64Unknown()
		plan.ActiveVersion = types.Int64Unknown()
		plan.Protocols = types.ListUnknown(types.StringType)
	}
	resp.Diagnostics.Append(resp.Plan.Set(ctx, &plan)...)
}

func (r *mockResource) body(ctx context.Context, m *mockModel, diags *diag.Diagnostics) client.MockWrite {
	return client.MockWrite{
		RoutePrefix:     strPtr(m.RoutePrefix),
		Persona:         strPtr(m.Persona),
		LLMProfile:      strPtr(m.LLMProfile),
		HeaderOverrides: m.HeaderOverrides.ValueBool(),
		DataPlaneAuth:   dataPlaneAuthToAPI(m.DataPlaneAuth),
		Chaos:           chaosToAPI(m.Chaos),
		Limits:          limitsToAPI(m.Limits),
		Consistency:     m.Consistency.ValueString(),
		UnknownIDs:      m.UnknownIDs.ValueString(),
		Hosts:           nonNil(stringList(ctx, m.Hosts, diags)),
		Webhooks:        webhooksToAPI(ctx, m.Webhooks, diags),
	}
}

func nonNil(s []string) []string {
	if s == nil {
		return []string{}
	}
	return s
}

// apply copies the API view into m, keeping m's spelling where the API only
// fills in defaults.
func (r *mockResource) apply(ctx context.Context, m *mockModel, v *client.Mock, diags *diag.Diagnostics) {
	m.ID = types.StringValue(v.Workspace + "/" + v.Name)
	m.Workspace = types.StringValue(v.Workspace)
	m.Name = types.StringValue(v.Name)
	m.RoutePrefix = types.StringValue(v.RoutePrefix)
	m.Hosts = listValue(v.Hosts, m.Hosts.IsNull())
	m.Persona = strValue(v.Persona)
	m.LLMProfile = strValue(v.LLMProfile)
	m.HeaderOverrides = types.BoolValue(v.HeaderOverrides)
	m.Consistency = types.StringValue(v.Consistency)
	m.UnknownIDs = types.StringValue(v.UnknownIDs)
	m.DataPlaneAuth = dataPlaneAuthFromAPI(v.DataPlaneAuth, m.DataPlaneAuth)
	m.Chaos = chaosFromAPI(v.Chaos, m.Chaos)
	m.Limits = limitsFromAPI(v.Limits, m.Limits)
	m.Webhooks = webhooksFromAPI(v.Webhooks, m.Webhooks)
	m.ActiveVersion = int64Value(v.ActiveVersion)
	m.URL = r.pd.mockURL(v.RoutePrefix)
	if m.ContractVersion.IsUnknown() {
		m.ContractVersion = types.Int64Null()
	}

	// The digest that counts: the active version's, or with activate =
	// false the one this resource uploaded.
	digestOf := v.ActiveVersion
	if m.Contract != nil && !m.Contract.activate() && !m.ContractVersion.IsNull() {
		n := m.ContractVersion.ValueInt64()
		digestOf = &n
	}
	m.Protocols = types.ListNull(types.StringType)
	m.ContractSHA256 = types.StringNull()
	if digestOf == nil {
		return
	}
	var cv client.ContractVersion
	if _, err := r.pd.client.Do(ctx, http.MethodGet, versionPath(v.Workspace, v.Name, *digestOf), nil, &cv); err != nil {
		if client.IsNotFound(err) {
			m.ContractSHA256 = types.StringNull()
			return
		}
		diags.AddError("Reading contract version failed", err.Error())
		return
	}
	m.ContractSHA256 = types.StringValue(cv.SourceDigest)
	if v.ActiveVersion != nil && *v.ActiveVersion == cv.Version {
		m.Protocols = listValue(protocolsOf(&cv), false)
	} else if v.ActiveVersion != nil {
		var active client.ContractVersion
		if _, err := r.pd.client.Do(ctx, http.MethodGet, versionPath(v.Workspace, v.Name, *v.ActiveVersion), nil, &active); err == nil {
			m.Protocols = listValue(protocolsOf(&active), false)
		}
	}
}

// upload uploads the configured contract as a new version.
func (r *mockResource) upload(ctx context.Context, m *mockModel, diags *diag.Diagnostics) {
	src, err := m.Contract.source().load(ctx)
	if err != nil {
		diags.AddAttributeError(path.Root("contract"), "Cannot read contract", err.Error())
		return
	}
	p := client.WsPath(m.Workspace.ValueString(), "/mocks/"+client.Seg(m.Name.ValueString())+"/contracts")
	if m.Contract.activate() {
		p += "?activate=true"
	}
	var res client.UploadResult
	if _, err := r.pd.client.Upload(ctx, p, uploadContentType, src, &res); err != nil {
		diags.AddAttributeError(path.Root("contract"), "Uploading contract failed", err.Error())
		return
	}
	for _, w := range res.Warnings {
		diags.AddAttributeWarning(path.Root("contract"), "Contract warning: "+w.Code, w.Message)
	}
	m.ContractVersion = types.Int64Value(res.Version)
}

func (r *mockResource) refresh(ctx context.Context, m *mockModel, diags *diag.Diagnostics) {
	var v client.Mock
	if _, err := r.pd.client.Do(ctx, http.MethodGet, mockPath(m.Workspace.ValueString(), m.Name.ValueString()), nil, &v); err != nil {
		diags.AddError("Reading mock failed", err.Error())
		return
	}
	r.apply(ctx, m, &v, diags)
}

func (r *mockResource) Create(ctx context.Context, req resource.CreateRequest, resp *resource.CreateResponse) {
	var plan mockModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	if resp.Diagnostics.HasError() {
		return
	}
	plan.Workspace = types.StringValue(r.pd.workspaceOr(plan.Workspace))
	body := r.body(ctx, &plan, &resp.Diagnostics)
	body.Name = plan.Name.ValueString()
	if resp.Diagnostics.HasError() {
		return
	}
	var v client.Mock
	if _, err := r.pd.client.Do(ctx, http.MethodPost, client.WsPath(plan.Workspace.ValueString(), "/mocks"), body, &v); err != nil {
		resp.Diagnostics.AddError("Creating mock failed", err.Error())
		return
	}
	// Saved before the upload, so a failed upload leaves a tracked mock
	// whose contract is planned again.
	plan.ContractVersion = types.Int64Null()
	r.apply(ctx, &plan, &v, &resp.Diagnostics)
	resp.Diagnostics.Append(resp.State.Set(ctx, &plan)...)
	if plan.Contract == nil || resp.Diagnostics.HasError() {
		return
	}
	r.upload(ctx, &plan, &resp.Diagnostics)
	if resp.Diagnostics.HasError() {
		return
	}
	r.refresh(ctx, &plan, &resp.Diagnostics)
	resp.Diagnostics.Append(resp.State.Set(ctx, &plan)...)
}

func (r *mockResource) Read(ctx context.Context, req resource.ReadRequest, resp *resource.ReadResponse) {
	var state mockModel
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	if resp.Diagnostics.HasError() {
		return
	}
	var v client.Mock
	if _, err := r.pd.client.Do(ctx, http.MethodGet, mockPath(state.Workspace.ValueString(), state.Name.ValueString()), nil, &v); err != nil {
		if client.IsNotFound(err) {
			resp.State.RemoveResource(ctx)
			return
		}
		resp.Diagnostics.AddError("Reading mock failed", err.Error())
		return
	}
	r.apply(ctx, &state, &v, &resp.Diagnostics)
	resp.Diagnostics.Append(resp.State.Set(ctx, &state)...)
}

func (r *mockResource) Update(ctx context.Context, req resource.UpdateRequest, resp *resource.UpdateResponse) {
	var plan, state mockModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	if resp.Diagnostics.HasError() {
		return
	}
	body := r.body(ctx, &plan, &resp.Diagnostics)
	if resp.Diagnostics.HasError() {
		return
	}
	if _, err := r.pd.client.Do(ctx, http.MethodPatch, mockPath(plan.Workspace.ValueString(), plan.Name.ValueString()), body, nil); err != nil {
		resp.Diagnostics.AddError("Updating mock failed", err.Error())
		return
	}
	upload := plan.Contract != nil && (plan.ContractSHA256.IsUnknown() || plan.ContractSHA256.ValueString() != state.ContractSHA256.ValueString())
	plan.ContractVersion = state.ContractVersion
	if upload {
		r.upload(ctx, &plan, &resp.Diagnostics)
	}
	// After a failed upload this records the patched settings and the
	// digest still served, so the upload is planned again.
	var refreshDiags diag.Diagnostics
	r.refresh(ctx, &plan, &refreshDiags)
	resp.Diagnostics.Append(refreshDiags...)
	if !refreshDiags.HasError() {
		resp.Diagnostics.Append(resp.State.Set(ctx, &plan)...)
	}
}

func (r *mockResource) Delete(ctx context.Context, req resource.DeleteRequest, resp *resource.DeleteResponse) {
	var state mockModel
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	if resp.Diagnostics.HasError() {
		return
	}
	if _, err := r.pd.client.Do(ctx, http.MethodDelete, mockPath(state.Workspace.ValueString(), state.Name.ValueString()), nil, nil); err != nil && !client.IsNotFound(err) {
		resp.Diagnostics.AddError("Deleting mock failed", err.Error())
	}
}

func (r *mockResource) ImportState(ctx context.Context, req resource.ImportStateRequest, resp *resource.ImportStateResponse) {
	parts, err := splitImportID(req.ID, 2, "<workspace>/<name> or <name>")
	if err != nil {
		resp.Diagnostics.AddError("Invalid import ID", err.Error())
		return
	}
	ws := parts[0]
	if ws == "" {
		ws = r.pd.workspace
	}
	resp.Diagnostics.Append(resp.State.SetAttribute(ctx, path.Root("workspace"), ws)...)
	resp.Diagnostics.Append(resp.State.SetAttribute(ctx, path.Root("name"), parts[1])...)
	resp.Diagnostics.Append(resp.State.SetAttribute(ctx, path.Root("contract_version"), types.Int64Null())...)
}
