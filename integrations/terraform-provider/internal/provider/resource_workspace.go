package provider

import (
	"context"
	"net/http"

	"github.com/hashicorp/terraform-plugin-framework/path"
	"github.com/hashicorp/terraform-plugin-framework/resource"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/booldefault"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/stringplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/types"

	"github.com/RJ-s-Org/terraform-provider-mockingbird/internal/client"
)

var (
	_ resource.ResourceWithConfigure   = (*workspaceResource)(nil)
	_ resource.ResourceWithImportState = (*workspaceResource)(nil)
)

// NewWorkspaceResource is the `mockingbird_workspace` resource.
func NewWorkspaceResource() resource.Resource { return &workspaceResource{} }

type workspaceResource struct{ pd *providerData }

type workspaceModel struct {
	ID           types.String `tfsdk:"id"`
	Name         types.String `tfsdk:"name"`
	DisplayName  types.String `tfsdk:"display_name"`
	ForceDestroy types.Bool   `tfsdk:"force_destroy"`
	CreatedAt    types.String `tfsdk:"created_at"`
}

func (r *workspaceResource) Metadata(_ context.Context, req resource.MetadataRequest, resp *resource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_workspace"
}

func (r *workspaceResource) Schema(_ context.Context, _ resource.SchemaRequest, resp *resource.SchemaResponse) {
	resp.Schema = schema.Schema{
		MarkdownDescription: "A workspace: an isolated set of mocks, LLM profiles, scenarios and API keys. Creating one needs a sysadmin credential (or `auth.mode: none`).",
		Attributes: map[string]schema.Attribute{
			"id": idAttribute("<name>"),
			"name": schema.StringAttribute{
				MarkdownDescription: "Workspace name, `^[a-z0-9][a-z0-9-]{0,62}$`. Changing it replaces the workspace.",
				Required:            true,
				PlanModifiers:       []planmodifier.String{stringplanmodifier.RequiresReplace()},
			},
			"display_name": schema.StringAttribute{
				MarkdownDescription: "Human-readable name. Defaults to `name`.",
				Optional:            true,
				Computed:            true,
				PlanModifiers:       []planmodifier.String{stringplanmodifier.UseStateForUnknown()},
			},
			"force_destroy": schema.BoolAttribute{
				MarkdownDescription: "Delete the workspace's mocks and LLM profiles with it on destroy. Without it, destroying a non-empty workspace fails.",
				Optional:            true,
				Computed:            true,
				Default:             booldefault.StaticBool(false),
			},
			"created_at": schema.StringAttribute{
				MarkdownDescription: "Creation time (RFC 3339).",
				Computed:            true,
				PlanModifiers:       []planmodifier.String{stringplanmodifier.UseStateForUnknown()},
			},
		},
	}
}

func (r *workspaceResource) Configure(_ context.Context, req resource.ConfigureRequest, resp *resource.ConfigureResponse) {
	r.pd = providerDataFrom(req.ProviderData, &resp.Diagnostics)
}

func (r *workspaceResource) apply(m *workspaceModel, w *client.Workspace) {
	m.ID = types.StringValue(w.Name)
	m.Name = types.StringValue(w.Name)
	m.DisplayName = types.StringValue(w.DisplayName)
	m.CreatedAt = types.StringValue(w.CreatedAt)
	if m.ForceDestroy.IsNull() || m.ForceDestroy.IsUnknown() {
		m.ForceDestroy = types.BoolValue(false)
	}
}

func (r *workspaceResource) Create(ctx context.Context, req resource.CreateRequest, resp *resource.CreateResponse) {
	var plan workspaceModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	if resp.Diagnostics.HasError() {
		return
	}
	body := map[string]any{"name": plan.Name.ValueString()}
	if dn := strPtr(plan.DisplayName); dn != nil {
		body["display_name"] = *dn
	}
	var w client.Workspace
	if _, err := r.pd.client.Do(ctx, http.MethodPost, "/api/v1/workspaces", body, &w); err != nil {
		resp.Diagnostics.AddError("Creating workspace failed", err.Error())
		return
	}
	r.apply(&plan, &w)
	resp.Diagnostics.Append(resp.State.Set(ctx, &plan)...)
}

func (r *workspaceResource) Read(ctx context.Context, req resource.ReadRequest, resp *resource.ReadResponse) {
	var state workspaceModel
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	if resp.Diagnostics.HasError() {
		return
	}
	var w client.Workspace
	if _, err := r.pd.client.Do(ctx, http.MethodGet, "/api/v1/workspaces/"+client.Seg(state.Name.ValueString()), nil, &w); err != nil {
		if client.IsNotFound(err) {
			resp.State.RemoveResource(ctx)
			return
		}
		resp.Diagnostics.AddError("Reading workspace failed", err.Error())
		return
	}
	r.apply(&state, &w)
	resp.Diagnostics.Append(resp.State.Set(ctx, &state)...)
}

func (r *workspaceResource) Update(ctx context.Context, req resource.UpdateRequest, resp *resource.UpdateResponse) {
	var plan workspaceModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	if resp.Diagnostics.HasError() {
		return
	}
	name := plan.Name.ValueString()
	dn := plan.DisplayName.ValueString()
	if plan.DisplayName.IsUnknown() || plan.DisplayName.IsNull() {
		dn = name
	}
	var w client.Workspace
	if _, err := r.pd.client.Do(ctx, http.MethodPatch, "/api/v1/workspaces/"+client.Seg(name), map[string]any{"display_name": dn}, &w); err != nil {
		resp.Diagnostics.AddError("Updating workspace failed", err.Error())
		return
	}
	r.apply(&plan, &w)
	resp.Diagnostics.Append(resp.State.Set(ctx, &plan)...)
}

func (r *workspaceResource) Delete(ctx context.Context, req resource.DeleteRequest, resp *resource.DeleteResponse) {
	var state workspaceModel
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	if resp.Diagnostics.HasError() {
		return
	}
	p := "/api/v1/workspaces/" + client.Seg(state.Name.ValueString())
	if state.ForceDestroy.ValueBool() {
		p += "?force=true"
	}
	if _, err := r.pd.client.Do(ctx, http.MethodDelete, p, nil, nil); err != nil && !client.IsNotFound(err) {
		resp.Diagnostics.AddError("Deleting workspace failed", err.Error())
	}
}

func (r *workspaceResource) ImportState(ctx context.Context, req resource.ImportStateRequest, resp *resource.ImportStateResponse) {
	resp.Diagnostics.Append(resp.State.SetAttribute(ctx, path.Root("name"), req.ID)...)
	resp.Diagnostics.Append(resp.State.SetAttribute(ctx, path.Root("force_destroy"), false)...)
}
