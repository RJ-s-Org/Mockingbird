package provider

import (
	"context"
	"encoding/json"
	"net/http"

	"github.com/hashicorp/terraform-plugin-framework/diag"
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
	_ resource.ResourceWithConfigure   = (*channelResource)(nil)
	_ resource.ResourceWithImportState = (*channelResource)(nil)
)

// NewChannelResource is the `mockingbird_channel` resource.
func NewChannelResource() resource.Resource { return &channelResource{} }

type channelResource struct{ pd *providerData }

type channelModel struct {
	ID                types.String `tfsdk:"id"`
	Workspace         types.String `tfsdk:"workspace"`
	Mock              types.String `tfsdk:"mock"`
	Channel           types.String `tfsdk:"channel"`
	Enabled           types.Bool   `tfsdk:"enabled"`
	Triggers          types.String `tfsdk:"triggers"`
	EffectiveTriggers types.String `tfsdk:"effective_triggers"`
	Direction         types.String `tfsdk:"direction"`
	Protocol          types.String `tfsdk:"protocol"`
	Address           types.String `tfsdk:"address"`
}

func (r *channelResource) Metadata(_ context.Context, req resource.MetadataRequest, resp *resource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_channel"
}

func (r *channelResource) Schema(_ context.Context, _ resource.SchemaRequest, resp *resource.SchemaResponse) {
	replace := []planmodifier.String{stringplanmodifier.RequiresReplace()}
	keep := []planmodifier.String{stringplanmodifier.UseStateForUnknown()}
	resp.Schema = schema.Schema{
		MarkdownDescription: "The settings of one event channel (Kafka, AMQP, MQTT or WebSocket) of a mock's active contract: whether it runs and when the mock publishes on it. Destroying it re-enables the channel with the contract's triggers.",
		Attributes: map[string]schema.Attribute{
			"id":        idAttribute("<workspace>/<mock>/<channel>"),
			"workspace": workspaceAttribute(),
			"mock":      schema.StringAttribute{Required: true, MarkdownDescription: "Name of the mock.", PlanModifiers: replace},
			"channel":   schema.StringAttribute{Required: true, MarkdownDescription: "Channel id in the active contract.", PlanModifiers: replace},
			"enabled": schema.BoolAttribute{
				Optional:            true,
				Computed:            true,
				Default:             booldefault.StaticBool(true),
				MarkdownDescription: "Whether the channel runs. Default `true`.",
			},
			"triggers": schema.StringAttribute{
				Optional:            true,
				MarkdownDescription: "JSON array of MCM triggers replacing the contract's, e.g. `jsonencode([{ schedule = { every_ms = 5000 } }])`, `jsonencode([\"manual\"])` or `jsonencode([{ cron = { expr = \"*/5 * * * *\" } }])`. Unset uses the contract's. Only channels the mock publishes may have triggers.",
			},
			"effective_triggers": schema.StringAttribute{Computed: true, MarkdownDescription: "The triggers in effect, as a JSON array."},
			"direction":          schema.StringAttribute{Computed: true, MarkdownDescription: "`publish` (the mock produces) or `subscribe` (the mock consumes).", PlanModifiers: keep},
			"protocol":           schema.StringAttribute{Computed: true, MarkdownDescription: "`kafka`, `amqp`, `mqtt` or `websocket`.", PlanModifiers: keep},
			"address":            schema.StringAttribute{Computed: true, MarkdownDescription: "Topic, routing key, queue or WebSocket path.", PlanModifiers: keep},
		},
	}
}

func (r *channelResource) Configure(_ context.Context, req resource.ConfigureRequest, resp *resource.ConfigureResponse) {
	r.pd = providerDataFrom(req.ProviderData, &resp.Diagnostics)
}

func channelPath(ws, mock, ch string) string {
	return client.WsPath(ws, "/mocks/"+client.Seg(mock)+"/channels/"+client.Seg(ch))
}

func (r *channelResource) apply(m *channelModel, c *client.Channel) {
	m.ID = types.StringValue(m.Workspace.ValueString() + "/" + m.Mock.ValueString() + "/" + c.ID)
	m.Channel = types.StringValue(c.ID)
	m.Enabled = types.BoolValue(c.Enabled)
	triggers := c.Triggers
	if triggers == nil {
		triggers = []json.RawMessage{}
	}
	b, _ := json.Marshal(triggers)
	m.EffectiveTriggers = types.StringValue(string(b))
	// Unset triggers stay unset; configured ones show drift.
	if !m.Triggers.IsNull() && !m.Triggers.IsUnknown() && !jsonEqual(m.Triggers.ValueString(), string(b)) {
		m.Triggers = types.StringValue(string(b))
	}
	m.Direction = types.StringValue(c.Direction)
	m.Protocol = types.StringValue(c.Protocol)
	m.Address = types.StringValue(c.Address)
}

func (r *channelResource) put(ctx context.Context, m *channelModel, diags *diag.Diagnostics) {
	body := client.PutChannel{Enabled: m.Enabled.ValueBool()}
	if s := strPtr(m.Triggers); s != nil {
		if err := json.Unmarshal([]byte(*s), &body.Triggers); err != nil {
			diags.AddAttributeError(path.Root("triggers"), "Invalid triggers", "triggers must be a JSON array: "+err.Error())
			return
		}
		if body.Triggers == nil {
			body.Triggers = []json.RawMessage{}
		}
	}
	var c client.Channel
	if _, err := r.pd.client.Do(ctx, http.MethodPut, channelPath(m.Workspace.ValueString(), m.Mock.ValueString(), m.Channel.ValueString()), body, &c); err != nil {
		diags.AddError("Writing channel settings failed", err.Error())
		return
	}
	r.apply(m, &c)
}

func (r *channelResource) Create(ctx context.Context, req resource.CreateRequest, resp *resource.CreateResponse) {
	var plan channelModel
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

func (r *channelResource) Read(ctx context.Context, req resource.ReadRequest, resp *resource.ReadResponse) {
	var state channelModel
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	if resp.Diagnostics.HasError() {
		return
	}
	// The detail flattens the channel view next to its schemas.
	var c client.Channel
	if _, err := r.pd.client.Do(ctx, http.MethodGet, channelPath(state.Workspace.ValueString(), state.Mock.ValueString(), state.Channel.ValueString()), nil, &c); err != nil {
		if client.IsNotFound(err) {
			resp.State.RemoveResource(ctx)
			return
		}
		resp.Diagnostics.AddError("Reading channel failed", err.Error())
		return
	}
	r.apply(&state, &c)
	resp.Diagnostics.Append(resp.State.Set(ctx, &state)...)
}

func (r *channelResource) Update(ctx context.Context, req resource.UpdateRequest, resp *resource.UpdateResponse) {
	var plan channelModel
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

// Delete re-enables the channel with the contract's triggers.
func (r *channelResource) Delete(ctx context.Context, req resource.DeleteRequest, resp *resource.DeleteResponse) {
	var state channelModel
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	if resp.Diagnostics.HasError() {
		return
	}
	p := channelPath(state.Workspace.ValueString(), state.Mock.ValueString(), state.Channel.ValueString())
	if _, err := r.pd.client.Do(ctx, http.MethodPut, p, client.PutChannel{Enabled: true}, nil); err != nil && !client.IsNotFound(err) {
		resp.Diagnostics.AddError("Resetting channel settings failed", err.Error())
	}
}

func (r *channelResource) ImportState(ctx context.Context, req resource.ImportStateRequest, resp *resource.ImportStateResponse) {
	parts, err := splitImportID(req.ID, 3, "<workspace>/<mock>/<channel> or <mock>/<channel>")
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
	resp.Diagnostics.Append(resp.State.SetAttribute(ctx, path.Root("channel"), parts[2])...)
}
