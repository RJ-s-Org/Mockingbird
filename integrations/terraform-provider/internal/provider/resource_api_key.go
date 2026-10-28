package provider

import (
	"context"
	"net/http"

	"github.com/hashicorp/terraform-plugin-framework-validators/listvalidator"
	"github.com/hashicorp/terraform-plugin-framework-validators/stringvalidator"
	"github.com/hashicorp/terraform-plugin-framework/path"
	"github.com/hashicorp/terraform-plugin-framework/resource"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/booldefault"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/boolplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/listplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/stringdefault"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/stringplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/schema/validator"
	"github.com/hashicorp/terraform-plugin-framework/types"

	"github.com/RJ-s-Org/terraform-provider-mockingbird/internal/client"
)

var (
	_ resource.ResourceWithConfigure   = (*apiKeyResource)(nil)
	_ resource.ResourceWithImportState = (*apiKeyResource)(nil)
)

// NewAPIKeyResource is the `mockingbird_api_key` resource.
func NewAPIKeyResource() resource.Resource { return &apiKeyResource{} }

type apiKeyResource struct{ pd *providerData }

type apiKeyModel struct {
	ID        types.String `tfsdk:"id"`
	Workspace types.String `tfsdk:"workspace"`
	Name      types.String `tfsdk:"name"`
	Role      types.String `tfsdk:"role"`
	Scopes    types.List   `tfsdk:"scopes"`
	ExpiresAt types.String `tfsdk:"expires_at"`
	Sysadmin  types.Bool   `tfsdk:"sysadmin"`
	KeyID     types.String `tfsdk:"key_id"`
	Token     types.String `tfsdk:"token"`
	CreatedAt types.String `tfsdk:"created_at"`
	CreatedBy types.String `tfsdk:"created_by"`
}

func (r *apiKeyResource) Metadata(_ context.Context, req resource.MetadataRequest, resp *resource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_api_key"
}

func (r *apiKeyResource) Schema(_ context.Context, _ resource.SchemaRequest, resp *resource.SchemaResponse) {
	replaceStr := []planmodifier.String{stringplanmodifier.RequiresReplace()}
	keepStr := []planmodifier.String{stringplanmodifier.UseStateForUnknown()}
	resp.Schema = schema.Schema{
		MarkdownDescription: "A workspace API key, for example a `data-plane` key an application sends to mocks with `data_plane_auth.mode = \"api_key\"`. The token is shown once, at creation; it is stored in the Terraform state as a sensitive value, so keep the state encrypted. Every change replaces the key (revoking the old one). The Admin API does not let an API key create keys: the provider must authenticate as a user (an OIDC access token) or the server must run with `auth.mode: none`.",
		Attributes: map[string]schema.Attribute{
			"id":        idAttribute("<workspace>/<key_id>"),
			"workspace": workspaceAttribute(),
			"name": schema.StringAttribute{
				Required:            true,
				MarkdownDescription: "Key name, 1–128 characters.",
				PlanModifiers:       replaceStr,
				Validators:          []validator.String{stringvalidator.LengthBetween(1, 128)},
			},
			"role": schema.StringAttribute{
				Optional:            true,
				Computed:            true,
				Default:             stringdefault.StaticString("viewer"),
				MarkdownDescription: "`viewer` (default), `editor`, `admin` or `owner`; at most the caller's own role.",
				PlanModifiers:       replaceStr,
				Validators:          []validator.String{stringvalidator.OneOf("viewer", "editor", "admin", "owner")},
			},
			"scopes": schema.ListAttribute{
				ElementType:         types.StringType,
				Optional:            true,
				Computed:            true,
				MarkdownDescription: "`admin` (the Admin API) and/or `data-plane` (calls to mocks with `api_key` data-plane auth). Default `[\"admin\"]`.",
				PlanModifiers:       []planmodifier.List{listplanmodifier.RequiresReplaceIfConfigured(), listplanmodifier.UseStateForUnknown()},
				Validators:          []validator.List{listvalidator.ValueStringsAre(stringvalidator.OneOf("admin", "data-plane"))},
			},
			"expires_at": schema.StringAttribute{
				Optional:            true,
				MarkdownDescription: "Expiry time (RFC 3339); unset never expires.",
				PlanModifiers:       replaceStr,
			},
			"sysadmin": schema.BoolAttribute{
				Optional:            true,
				Computed:            true,
				Default:             booldefault.StaticBool(false),
				MarkdownDescription: "Acts as a sysadmin; only a sysadmin can create such a key.",
				PlanModifiers:       []planmodifier.Bool{boolplanmodifier.RequiresReplace()},
			},
			"key_id":     schema.StringAttribute{Computed: true, MarkdownDescription: "The key's public id.", PlanModifiers: keepStr},
			"created_at": schema.StringAttribute{Computed: true, MarkdownDescription: "Creation time (RFC 3339).", PlanModifiers: keepStr},
			"created_by": schema.StringAttribute{Computed: true, MarkdownDescription: "Subject of the principal that created the key.", PlanModifiers: keepStr},
			"token": schema.StringAttribute{
				Computed:            true,
				Sensitive:           true,
				MarkdownDescription: "The secret token, `mbk_<key_id>_<secret>`. Only known for keys created by this resource (null after import).",
				PlanModifiers:       keepStr,
			},
		},
	}
}

func (r *apiKeyResource) Configure(_ context.Context, req resource.ConfigureRequest, resp *resource.ConfigureResponse) {
	r.pd = providerDataFrom(req.ProviderData, &resp.Diagnostics)
}

func (r *apiKeyResource) apply(m *apiKeyModel, k *client.APIKey) {
	m.ID = types.StringValue(m.Workspace.ValueString() + "/" + k.KeyID)
	m.KeyID = types.StringValue(k.KeyID)
	m.Name = types.StringValue(k.Name)
	m.Role = types.StringValue(k.Role)
	m.Scopes = listValue(k.Scopes, false)
	m.Sysadmin = types.BoolValue(k.Sysadmin)
	m.CreatedAt = types.StringValue(k.CreatedAt)
	m.CreatedBy = types.StringValue(k.CreatedBy)
	// Keep the configured spelling of the expiry when it is the same time.
	if k.ExpiresAt == nil {
		m.ExpiresAt = types.StringNull()
	} else if m.ExpiresAt.IsNull() || m.ExpiresAt.IsUnknown() || !sameTime(m.ExpiresAt.ValueString(), *k.ExpiresAt) {
		m.ExpiresAt = types.StringValue(*k.ExpiresAt)
	}
}

func (r *apiKeyResource) Create(ctx context.Context, req resource.CreateRequest, resp *resource.CreateResponse) {
	var plan apiKeyModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	if resp.Diagnostics.HasError() {
		return
	}
	plan.Workspace = types.StringValue(r.pd.workspaceOr(plan.Workspace))
	body := client.CreateAPIKey{
		Name:      plan.Name.ValueString(),
		Role:      plan.Role.ValueString(),
		Scopes:    nonNil(stringList(ctx, plan.Scopes, &resp.Diagnostics)),
		ExpiresAt: strPtr(plan.ExpiresAt),
		Sysadmin:  plan.Sysadmin.ValueBool(),
	}
	var created client.CreatedAPIKey
	if _, err := r.pd.client.Do(ctx, http.MethodPost, client.WsPath(plan.Workspace.ValueString(), "/api-keys"), body, &created); err != nil {
		resp.Diagnostics.AddError("Creating API key failed", err.Error())
		return
	}
	r.apply(&plan, &created.Key)
	plan.Token = types.StringValue(created.Token)
	resp.Diagnostics.Append(resp.State.Set(ctx, &plan)...)
}

func (r *apiKeyResource) Read(ctx context.Context, req resource.ReadRequest, resp *resource.ReadResponse) {
	var state apiKeyModel
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	if resp.Diagnostics.HasError() {
		return
	}
	var keys []client.APIKey
	if _, err := r.pd.client.Do(ctx, http.MethodGet, client.WsPath(state.Workspace.ValueString(), "/api-keys"), nil, &keys); err != nil {
		if client.IsNotFound(err) {
			resp.State.RemoveResource(ctx)
			return
		}
		resp.Diagnostics.AddError("Reading API keys failed", err.Error())
		return
	}
	for i := range keys {
		k := &keys[i]
		if k.KeyID != state.KeyID.ValueString() {
			continue
		}
		if k.RevokedAt != nil {
			break
		}
		r.apply(&state, k)
		if state.Token.IsUnknown() {
			state.Token = types.StringNull()
		}
		resp.Diagnostics.Append(resp.State.Set(ctx, &state)...)
		return
	}
	// Revoked or gone: plan a new key.
	resp.State.RemoveResource(ctx)
}

// Update is never called with a change: every attribute replaces the key.
func (r *apiKeyResource) Update(ctx context.Context, req resource.UpdateRequest, resp *resource.UpdateResponse) {
	var plan apiKeyModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	resp.Diagnostics.Append(resp.State.Set(ctx, &plan)...)
}

func (r *apiKeyResource) Delete(ctx context.Context, req resource.DeleteRequest, resp *resource.DeleteResponse) {
	var state apiKeyModel
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	if resp.Diagnostics.HasError() {
		return
	}
	p := client.WsPath(state.Workspace.ValueString(), "/api-keys/"+client.Seg(state.KeyID.ValueString()))
	if _, err := r.pd.client.Do(ctx, http.MethodDelete, p, nil, nil); err != nil && !client.IsNotFound(err) {
		resp.Diagnostics.AddError("Revoking API key failed", err.Error())
	}
}

func (r *apiKeyResource) ImportState(ctx context.Context, req resource.ImportStateRequest, resp *resource.ImportStateResponse) {
	parts, err := splitImportID(req.ID, 2, "<workspace>/<key_id> or <key_id>")
	if err != nil {
		resp.Diagnostics.AddError("Invalid import ID", err.Error())
		return
	}
	ws := parts[0]
	if ws == "" {
		ws = r.pd.workspace
	}
	resp.Diagnostics.Append(resp.State.SetAttribute(ctx, path.Root("workspace"), ws)...)
	resp.Diagnostics.Append(resp.State.SetAttribute(ctx, path.Root("key_id"), parts[1])...)
	resp.Diagnostics.Append(resp.State.SetAttribute(ctx, path.Root("token"), types.StringNull())...)
}
