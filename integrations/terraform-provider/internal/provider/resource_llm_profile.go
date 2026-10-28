package provider

import (
	"context"
	"encoding/json"
	"net/http"
	"strconv"

	"github.com/hashicorp/terraform-plugin-framework/diag"
	"github.com/hashicorp/terraform-plugin-framework/path"
	"github.com/hashicorp/terraform-plugin-framework/resource"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/booldefault"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/float64default"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/int64default"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/stringplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/schema/validator"
	"github.com/hashicorp/terraform-plugin-framework/types"

	"github.com/RJ-s-Org/terraform-provider-mockingbird/internal/client"
)

var (
	_ resource.ResourceWithConfigure   = (*llmProfileResource)(nil)
	_ resource.ResourceWithImportState = (*llmProfileResource)(nil)
)

// NewLLMProfileResource is the `mockingbird_llm_profile` resource.
func NewLLMProfileResource() resource.Resource { return &llmProfileResource{} }

type llmProfileResource struct{ pd *providerData }

type llmProfileModel struct {
	ID              types.String  `tfsdk:"id"`
	Workspace       types.String  `tfsdk:"workspace"`
	Name            types.String  `tfsdk:"name"`
	BaseURL         types.String  `tfsdk:"base_url"`
	Model           types.String  `tfsdk:"model"`
	SecretRef       types.String  `tfsdk:"secret_ref"`
	JSONSchema      types.Bool    `tfsdk:"json_schema"`
	Temperature     types.Float64 `tfsdk:"temperature"`
	TimeoutMs       types.Int64   `tfsdk:"timeout_ms"`
	MaxTokens       types.Int64   `tfsdk:"max_tokens"`
	FallbackProfile types.String  `tfsdk:"fallback_profile"`
	Headers         types.Map     `tfsdk:"headers"`
	ExtraBody       types.String  `tfsdk:"extra_body"`
}

func (r *llmProfileResource) Metadata(_ context.Context, req resource.MetadataRequest, resp *resource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_llm_profile"
}

func (r *llmProfileResource) Schema(_ context.Context, _ resource.SchemaRequest, resp *resource.SchemaResponse) {
	resp.Schema = schema.Schema{
		MarkdownDescription: "An LLM profile: an OpenAI-compatible endpoint and model that mocks generate responses with. The API key is only ever a **reference** (`env:NAME`) resolved by the Mockingbird server, never a value, so no secret reaches the Terraform state. Through the API a reference must match the server's `server.api_secret_refs` (default `env:MB_LLM_*` or `env:*_API_KEY`), and only a sysadmin may set it.",
		Attributes: map[string]schema.Attribute{
			"id":        idAttribute("<workspace>/<name>"),
			"workspace": workspaceAttribute(),
			"name": schema.StringAttribute{
				Required:            true,
				MarkdownDescription: "Profile name, 1–128 characters of `[A-Za-z0-9._-]`.",
				PlanModifiers:       []planmodifier.String{stringplanmodifier.RequiresReplace()},
			},
			"base_url": schema.StringAttribute{Required: true, MarkdownDescription: "OpenAI-compatible base URL, e.g. `https://api.deepseek.com` or `https://openrouter.ai/api/v1`."},
			"model":    schema.StringAttribute{Required: true, MarkdownDescription: "Model name."},
			"secret_ref": schema.StringAttribute{
				Optional:            true,
				MarkdownDescription: "Reference to the API key on the Mockingbird server, e.g. `env:MB_LLM_API_KEY` or `env:OPENROUTER_API_KEY`. Never the key itself.",
				Validators:          []validator.String{secretRefValidator()},
			},
			"json_schema": schema.BoolAttribute{Optional: true, Computed: true, Default: booldefault.StaticBool(false), MarkdownDescription: "Use the provider's `json_schema` response format."},
			"temperature": schema.Float64Attribute{Optional: true, Computed: true, Default: float64default.StaticFloat64(0.7), MarkdownDescription: "Sampling temperature. Default `0.7`."},
			"timeout_ms":  schema.Int64Attribute{Optional: true, Computed: true, Default: int64default.StaticInt64(60000), MarkdownDescription: "Request timeout. Default `60000`."},
			"max_tokens":  schema.Int64Attribute{Optional: true, Computed: true, Default: int64default.StaticInt64(16384), MarkdownDescription: "Most tokens per completion. Default `16384`."},
			"fallback_profile": schema.StringAttribute{
				Optional:            true,
				MarkdownDescription: "Profile tried when this one fails.",
			},
			"headers": schema.MapAttribute{
				ElementType:         types.StringType,
				Optional:            true,
				MarkdownDescription: "Extra provider request headers by lowercase name. A value may embed secret references as `${env:NAME}`; a header whose name suggests a credential must use one.",
			},
			"extra_body": schema.StringAttribute{
				Optional:            true,
				MarkdownDescription: "JSON object merged into every provider request body (at most 8 KiB, not for secrets), e.g. `jsonencode({ provider = { sort = \"price\" } })`.",
			},
		},
	}
}

func (r *llmProfileResource) Configure(_ context.Context, req resource.ConfigureRequest, resp *resource.ConfigureResponse) {
	r.pd = providerDataFrom(req.ProviderData, &resp.Diagnostics)
}

func profilePath(ws, name string) string {
	return client.WsPath(ws, "/llm-profiles/"+client.Seg(name))
}

func (r *llmProfileResource) put(ctx context.Context, m *llmProfileModel, diags *diag.Diagnostics) {
	body := client.LLMProfile{
		BaseURL:         m.BaseURL.ValueString(),
		Model:           m.Model.ValueString(),
		SecretRef:       strPtr(m.SecretRef),
		JSONSchema:      m.JSONSchema.ValueBool(),
		Temperature:     m.Temperature.ValueFloat64(),
		TimeoutMs:       m.TimeoutMs.ValueInt64(),
		MaxTokens:       m.MaxTokens.ValueInt64(),
		FallbackProfile: strPtr(m.FallbackProfile),
		Headers:         map[string]string{},
	}
	if !m.Headers.IsNull() && !m.Headers.IsUnknown() {
		diags.Append(m.Headers.ElementsAs(ctx, &body.Headers, false)...)
	}
	if s := strPtr(m.ExtraBody); s != nil {
		if !json.Valid([]byte(*s)) {
			diags.AddAttributeError(path.Root("extra_body"), "Invalid JSON", "extra_body must be a JSON object")
			return
		}
		body.ExtraBody = json.RawMessage(*s)
	}
	if diags.HasError() {
		return
	}
	var v client.LLMProfile
	if _, err := r.pd.client.Do(ctx, http.MethodPut, profilePath(m.Workspace.ValueString(), m.Name.ValueString()), body, &v); err != nil {
		diags.AddError("Writing LLM profile failed", err.Error())
		return
	}
	r.apply(m, &v)
}

func (r *llmProfileResource) apply(m *llmProfileModel, v *client.LLMProfile) {
	m.ID = types.StringValue(m.Workspace.ValueString() + "/" + v.Name)
	m.Name = types.StringValue(v.Name)
	m.BaseURL = types.StringValue(v.BaseURL)
	m.Model = types.StringValue(v.Model)
	m.SecretRef = strValue(v.SecretRef)
	m.JSONSchema = types.BoolValue(v.JSONSchema)
	// The API stores an f32: keep the configured spelling when it rounds
	// to the same value.
	if m.Temperature.IsNull() || m.Temperature.IsUnknown() || float32(m.Temperature.ValueFloat64()) != float32(v.Temperature) {
		m.Temperature = types.Float64Value(shortestFloat32(v.Temperature))
	}
	m.TimeoutMs = types.Int64Value(v.TimeoutMs)
	m.MaxTokens = types.Int64Value(v.MaxTokens)
	m.FallbackProfile = strValue(v.FallbackProfile)
	m.Headers = mapValue(v.Headers, m.Headers.IsNull())
	m.ExtraBody = jsonFromServer(v.ExtraBody, m.ExtraBody, true)
}

// shortestFloat32 is f as the shortest decimal that rounds to the same
// float32 (0.2 rather than 0.20000000298023224).
func shortestFloat32(f float64) float64 {
	out, err := strconv.ParseFloat(strconv.FormatFloat(f, 'g', -1, 32), 64)
	if err != nil {
		return f
	}
	return out
}

func (r *llmProfileResource) Create(ctx context.Context, req resource.CreateRequest, resp *resource.CreateResponse) {
	var plan llmProfileModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	if resp.Diagnostics.HasError() {
		return
	}
	plan.Workspace = types.StringValue(r.pd.workspaceOr(plan.Workspace))
	r.put(ctx, &plan, &resp.Diagnostics)
	if resp.Diagnostics.HasError() {
		return
	}
	resp.Diagnostics.Append(resp.State.Set(ctx, &plan)...)
}

func (r *llmProfileResource) Read(ctx context.Context, req resource.ReadRequest, resp *resource.ReadResponse) {
	var state llmProfileModel
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	if resp.Diagnostics.HasError() {
		return
	}
	var v client.LLMProfile
	if _, err := r.pd.client.Do(ctx, http.MethodGet, profilePath(state.Workspace.ValueString(), state.Name.ValueString()), nil, &v); err != nil {
		if client.IsNotFound(err) {
			resp.State.RemoveResource(ctx)
			return
		}
		resp.Diagnostics.AddError("Reading LLM profile failed", err.Error())
		return
	}
	r.apply(&state, &v)
	resp.Diagnostics.Append(resp.State.Set(ctx, &state)...)
}

func (r *llmProfileResource) Update(ctx context.Context, req resource.UpdateRequest, resp *resource.UpdateResponse) {
	var plan llmProfileModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	if resp.Diagnostics.HasError() {
		return
	}
	r.put(ctx, &plan, &resp.Diagnostics)
	if resp.Diagnostics.HasError() {
		return
	}
	resp.Diagnostics.Append(resp.State.Set(ctx, &plan)...)
}

func (r *llmProfileResource) Delete(ctx context.Context, req resource.DeleteRequest, resp *resource.DeleteResponse) {
	var state llmProfileModel
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	if resp.Diagnostics.HasError() {
		return
	}
	var res struct {
		Warnings []string `json:"warnings"`
	}
	if _, err := r.pd.client.Do(ctx, http.MethodDelete, profilePath(state.Workspace.ValueString(), state.Name.ValueString()), nil, &res); err != nil && !client.IsNotFound(err) {
		resp.Diagnostics.AddError("Deleting LLM profile failed", err.Error())
		return
	}
	for _, w := range res.Warnings {
		resp.Diagnostics.AddWarning("LLM profile still in use", w)
	}
}

func (r *llmProfileResource) ImportState(ctx context.Context, req resource.ImportStateRequest, resp *resource.ImportStateResponse) {
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
}
