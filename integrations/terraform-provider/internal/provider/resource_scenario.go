package provider

import (
	"context"
	"encoding/json"
	"net/http"

	"github.com/hashicorp/terraform-plugin-framework-validators/int64validator"
	"github.com/hashicorp/terraform-plugin-framework/diag"
	"github.com/hashicorp/terraform-plugin-framework/path"
	"github.com/hashicorp/terraform-plugin-framework/resource"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/stringplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/schema/validator"
	"github.com/hashicorp/terraform-plugin-framework/types"

	"github.com/RJ-s-Org/terraform-provider-mockingbird/internal/client"
)

var (
	_ resource.ResourceWithConfigure   = (*scenarioResource)(nil)
	_ resource.ResourceWithImportState = (*scenarioResource)(nil)
)

// NewScenarioResource is the `mockingbird_scenario` resource.
func NewScenarioResource() resource.Resource { return &scenarioResource{} }

type scenarioResource struct{ pd *providerData }

type scenarioModel struct {
	ID                    types.String `tfsdk:"id"`
	Workspace             types.String `tfsdk:"workspace"`
	Mock                  types.String `tfsdk:"mock"`
	Name                  types.String `tfsdk:"name"`
	Instructions          types.String `tfsdk:"instructions"`
	Status                types.Int64  `tfsdk:"status"`
	LatencyMs             types.Int64  `tfsdk:"latency_ms"`
	Set                   types.String `tfsdk:"set"`
	Operations            types.List   `tfsdk:"operations"`
	StreamItems           types.Int64  `tfsdk:"stream_items"`
	StreamItemIntervalMs  types.Int64  `tfsdk:"stream_item_interval_ms"`
	Chaos                 *chaosModel  `tfsdk:"chaos"`
	OperationInstructions types.Map    `tfsdk:"operation_instructions"`
}

func (r *scenarioResource) Metadata(_ context.Context, req resource.MetadataRequest, resp *resource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_scenario"
}

func (r *scenarioResource) Schema(_ context.Context, _ resource.SchemaRequest, resp *resource.SchemaResponse) {
	replace := []planmodifier.String{stringplanmodifier.RequiresReplace()}
	resp.Schema = schema.Schema{
		MarkdownDescription: "A named scenario of a mock: extra LLM instructions, a forced status, latency, forced body fields or chaos, applied when a caller sends `X-Mockingbird-Scenario: <name>`.",
		Attributes: map[string]schema.Attribute{
			"id":        idAttribute("<workspace>/<mock>/<name>"),
			"workspace": workspaceAttribute(),
			"mock":      schema.StringAttribute{Required: true, MarkdownDescription: "Name of the mock.", PlanModifiers: replace},
			"name":      schema.StringAttribute{Required: true, MarkdownDescription: "Scenario name, 1–128 characters of `[A-Za-z0-9._-]`.", PlanModifiers: replace},
			"instructions": schema.StringAttribute{
				Optional:            true,
				MarkdownDescription: "Extra instructions for the LLM, e.g. `The card is declined for insufficient funds.`",
			},
			"status": schema.Int64Attribute{
				Optional:            true,
				MarkdownDescription: "Forced response status, 100–599. A non-5xx status must be declared by an operation the scenario applies to.",
				Validators:          []validator.Int64{int64validator.Between(100, 599)},
			},
			"latency_ms": schema.Int64Attribute{Optional: true, MarkdownDescription: "Added latency in milliseconds."},
			"set": schema.StringAttribute{
				Optional:            true,
				MarkdownDescription: "A JSON object of fields forced into the response body, e.g. `jsonencode({ status = \"failed\" })`.",
			},
			"operations": schema.ListAttribute{
				ElementType:         types.StringType,
				Optional:            true,
				MarkdownDescription: "Operation ids the scenario applies to; unset means all.",
			},
			"stream_items":            schema.Int64Attribute{Optional: true, MarkdownDescription: "Items per streamed response (1–10000).", Validators: []validator.Int64{int64validator.Between(1, 10000)}},
			"stream_item_interval_ms": schema.Int64Attribute{Optional: true, MarkdownDescription: "Pause between streamed items in milliseconds (at most 30000).", Validators: []validator.Int64{int64validator.Between(0, 30000)}},
			"chaos":                   chaosSchema(),
			"operation_instructions": schema.MapAttribute{
				ElementType:         types.StringType,
				Optional:            true,
				MarkdownDescription: "Instructions replacing `instructions` for one operation, by operation id.",
			},
		},
	}
}

func (r *scenarioResource) Configure(_ context.Context, req resource.ConfigureRequest, resp *resource.ConfigureResponse) {
	r.pd = providerDataFrom(req.ProviderData, &resp.Diagnostics)
}

func scenarioPath(ws, mock, name string) string {
	return client.WsPath(ws, "/mocks/"+client.Seg(mock)+"/scenarios/"+client.Seg(name))
}

func (r *scenarioResource) put(ctx context.Context, m *scenarioModel, diags *diag.Diagnostics) {
	body := client.Scenario{
		Instructions:         strPtr(m.Instructions),
		Status:               int64Ptr(m.Status),
		LatencyMs:            int64Ptr(m.LatencyMs),
		Operations:           nonNil(stringList(ctx, m.Operations, diags)),
		StreamItems:          int64Ptr(m.StreamItems),
		StreamItemIntervalMs: int64Ptr(m.StreamItemIntervalMs),
		Chaos:                chaosToAPI(m.Chaos),
	}
	if s := strPtr(m.Set); s != nil {
		if !json.Valid([]byte(*s)) {
			diags.AddAttributeError(path.Root("set"), "Invalid JSON", "set must be a JSON object")
			return
		}
		body.Set = json.RawMessage(*s)
	}
	if !m.OperationInstructions.IsNull() && !m.OperationInstructions.IsUnknown() {
		body.OperationInstructions = map[string]string{}
		diags.Append(m.OperationInstructions.ElementsAs(ctx, &body.OperationInstructions, false)...)
	}
	if diags.HasError() {
		return
	}
	var v client.Scenario
	if _, err := r.pd.client.Do(ctx, http.MethodPut, scenarioPath(m.Workspace.ValueString(), m.Mock.ValueString(), m.Name.ValueString()), body, &v); err != nil {
		diags.AddError("Writing scenario failed", err.Error())
		return
	}
	r.apply(m, &v)
}

// get finds the scenario in the mock's list; nil when it does not exist.
func (r *scenarioResource) get(ctx context.Context, ws, mock, name string) (*client.Scenario, error) {
	var list []client.Scenario
	if _, err := r.pd.client.Do(ctx, http.MethodGet, client.WsPath(ws, "/mocks/"+client.Seg(mock)+"/scenarios"), nil, &list); err != nil {
		if client.IsNotFound(err) {
			return nil, nil
		}
		return nil, err
	}
	for i := range list {
		if list[i].Name == name {
			return &list[i], nil
		}
	}
	return nil, nil
}

func (r *scenarioResource) apply(m *scenarioModel, v *client.Scenario) {
	m.ID = types.StringValue(m.Workspace.ValueString() + "/" + m.Mock.ValueString() + "/" + v.Name)
	m.Name = types.StringValue(v.Name)
	m.Instructions = strValue(v.Instructions)
	m.Status = int64Value(v.Status)
	m.LatencyMs = int64Value(v.LatencyMs)
	m.Set = jsonFromServer(v.Set, m.Set, false)
	m.Operations = listValue(v.Operations, m.Operations.IsNull())
	m.StreamItems = int64Value(v.StreamItems)
	m.StreamItemIntervalMs = int64Value(v.StreamItemIntervalMs)
	m.Chaos = chaosFromAPI(v.Chaos, m.Chaos)
	m.OperationInstructions = mapValue(v.OperationInstructions, m.OperationInstructions.IsNull())
}

func (r *scenarioResource) Create(ctx context.Context, req resource.CreateRequest, resp *resource.CreateResponse) {
	var plan scenarioModel
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

func (r *scenarioResource) Read(ctx context.Context, req resource.ReadRequest, resp *resource.ReadResponse) {
	var state scenarioModel
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	if resp.Diagnostics.HasError() {
		return
	}
	v, err := r.get(ctx, state.Workspace.ValueString(), state.Mock.ValueString(), state.Name.ValueString())
	if err != nil {
		resp.Diagnostics.AddError("Reading scenario failed", err.Error())
		return
	}
	if v == nil {
		resp.State.RemoveResource(ctx)
		return
	}
	r.apply(&state, v)
	resp.Diagnostics.Append(resp.State.Set(ctx, &state)...)
}

func (r *scenarioResource) Update(ctx context.Context, req resource.UpdateRequest, resp *resource.UpdateResponse) {
	var plan scenarioModel
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

func (r *scenarioResource) Delete(ctx context.Context, req resource.DeleteRequest, resp *resource.DeleteResponse) {
	var state scenarioModel
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	if resp.Diagnostics.HasError() {
		return
	}
	if _, err := r.pd.client.Do(ctx, http.MethodDelete, scenarioPath(state.Workspace.ValueString(), state.Mock.ValueString(), state.Name.ValueString()), nil, nil); err != nil && !client.IsNotFound(err) {
		resp.Diagnostics.AddError("Deleting scenario failed", err.Error())
	}
}

func (r *scenarioResource) ImportState(ctx context.Context, req resource.ImportStateRequest, resp *resource.ImportStateResponse) {
	parts, err := splitImportID(req.ID, 3, "<workspace>/<mock>/<name> or <mock>/<name>")
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
	resp.Diagnostics.Append(resp.State.SetAttribute(ctx, path.Root("name"), parts[2])...)
}
