package provider

import (
	"context"
	"net/http"

	"github.com/hashicorp/terraform-plugin-framework/path"
	"github.com/hashicorp/terraform-plugin-framework/resource"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/stringplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/types"

	"github.com/RJ-s-Org/terraform-provider-mockingbird/internal/client"
)

var (
	_ resource.ResourceWithConfigure   = (*eventSettingsResource)(nil)
	_ resource.ResourceWithImportState = (*eventSettingsResource)(nil)
)

// NewEventSettingsResource is the `mockingbird_event_settings` resource.
func NewEventSettingsResource() resource.Resource { return &eventSettingsResource{} }

type eventSettingsResource struct{ pd *providerData }

type eventSettingsModel struct {
	ID        types.String `tfsdk:"id"`
	Workspace types.String `tfsdk:"workspace"`
	Mock      types.String `tfsdk:"mock"`
	Broker    types.String `tfsdk:"broker"`
}

func (r *eventSettingsResource) Metadata(_ context.Context, req resource.MetadataRequest, resp *resource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_event_settings"
}

func (r *eventSettingsResource) Schema(_ context.Context, _ resource.SchemaRequest, resp *resource.SchemaResponse) {
	resp.Schema = schema.Schema{
		MarkdownDescription: "A mock's event settings: which configured broker its event channels use. Destroying it returns to automatic broker selection.",
		Attributes: map[string]schema.Attribute{
			"id":        idAttribute("<workspace>/<mock>"),
			"workspace": workspaceAttribute(),
			"mock": schema.StringAttribute{
				Required:            true,
				MarkdownDescription: "Name of the mock.",
				PlanModifiers:       []planmodifier.String{stringplanmodifier.RequiresReplace()},
			},
			"broker": schema.StringAttribute{
				Optional:            true,
				MarkdownDescription: "Name of a broker in the server's configuration (see `GET /api/v1/brokers`). Unset picks one automatically.",
			},
		},
	}
}

func (r *eventSettingsResource) Configure(_ context.Context, req resource.ConfigureRequest, resp *resource.ConfigureResponse) {
	r.pd = providerDataFrom(req.ProviderData, &resp.Diagnostics)
}

func eventSettingsPath(ws, mock string) string {
	return client.WsPath(ws, "/mocks/"+client.Seg(mock)+"/event-settings")
}

func (r *eventSettingsResource) put(ctx context.Context, m *eventSettingsModel) error {
	var v client.EventSettings
	if _, err := r.pd.client.Do(ctx, http.MethodPut, eventSettingsPath(m.Workspace.ValueString(), m.Mock.ValueString()), client.EventSettings{Broker: strPtr(m.Broker)}, &v); err != nil {
		return err
	}
	m.ID = types.StringValue(m.Workspace.ValueString() + "/" + m.Mock.ValueString())
	m.Broker = strValue(v.Broker)
	return nil
}

func (r *eventSettingsResource) Create(ctx context.Context, req resource.CreateRequest, resp *resource.CreateResponse) {
	var plan eventSettingsModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	if resp.Diagnostics.HasError() {
		return
	}
	plan.Workspace = types.StringValue(r.pd.workspaceOr(plan.Workspace))
	if err := r.put(ctx, &plan); err != nil {
		resp.Diagnostics.AddError("Writing event settings failed", err.Error())
		return
	}
	resp.Diagnostics.Append(resp.State.Set(ctx, &plan)...)
}

func (r *eventSettingsResource) Read(ctx context.Context, req resource.ReadRequest, resp *resource.ReadResponse) {
	var state eventSettingsModel
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	if resp.Diagnostics.HasError() {
		return
	}
	var v client.EventSettings
	if _, err := r.pd.client.Do(ctx, http.MethodGet, eventSettingsPath(state.Workspace.ValueString(), state.Mock.ValueString()), nil, &v); err != nil {
		if client.IsNotFound(err) {
			resp.State.RemoveResource(ctx)
			return
		}
		resp.Diagnostics.AddError("Reading event settings failed", err.Error())
		return
	}
	state.ID = types.StringValue(state.Workspace.ValueString() + "/" + state.Mock.ValueString())
	state.Broker = strValue(v.Broker)
	resp.Diagnostics.Append(resp.State.Set(ctx, &state)...)
}

func (r *eventSettingsResource) Update(ctx context.Context, req resource.UpdateRequest, resp *resource.UpdateResponse) {
	var plan eventSettingsModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	if resp.Diagnostics.HasError() {
		return
	}
	if err := r.put(ctx, &plan); err != nil {
		resp.Diagnostics.AddError("Writing event settings failed", err.Error())
		return
	}
	resp.Diagnostics.Append(resp.State.Set(ctx, &plan)...)
}

func (r *eventSettingsResource) Delete(ctx context.Context, req resource.DeleteRequest, resp *resource.DeleteResponse) {
	var state eventSettingsModel
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	if resp.Diagnostics.HasError() {
		return
	}
	state.Broker = types.StringNull()
	if err := r.put(ctx, &state); err != nil && !client.IsNotFound(err) {
		resp.Diagnostics.AddError("Resetting event settings failed", err.Error())
	}
}

func (r *eventSettingsResource) ImportState(ctx context.Context, req resource.ImportStateRequest, resp *resource.ImportStateResponse) {
	parts, err := splitImportID(req.ID, 2, "<workspace>/<mock> or <mock>")
	if err != nil {
		resp.Diagnostics.AddError("Invalid import ID", err.Error())
		return
	}
	ws := parts[0]
	if ws == "" {
		ws = r.pd.workspace
	}
	resp.Diagnostics.Append(resp.State.SetAttribute(ctx, path.Root("workspace"), ws)...)
	resp.Diagnostics.Append(resp.State.SetAttribute(ctx, path.Root("mock"), parts[1])...)
}
