package provider

import (
	"context"
	"fmt"
	"net/http"
	"strconv"

	"github.com/hashicorp/terraform-plugin-framework-validators/resourcevalidator"
	"github.com/hashicorp/terraform-plugin-framework/path"
	"github.com/hashicorp/terraform-plugin-framework/resource"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/booldefault"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/int64planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/listplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/stringplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/types"

	"github.com/RJ-s-Org/terraform-provider-mockingbird/internal/client"
)

var (
	_ resource.ResourceWithConfigure        = (*contractVersionResource)(nil)
	_ resource.ResourceWithImportState      = (*contractVersionResource)(nil)
	_ resource.ResourceWithModifyPlan       = (*contractVersionResource)(nil)
	_ resource.ResourceWithConfigValidators = (*contractVersionResource)(nil)
)

// NewContractVersionResource is the `mockingbird_contract_version` resource.
func NewContractVersionResource() resource.Resource { return &contractVersionResource{} }

type contractVersionResource struct{ pd *providerData }

type contractVersionModel struct {
	ID           types.String `tfsdk:"id"`
	Workspace    types.String `tfsdk:"workspace"`
	Mock         types.String `tfsdk:"mock"`
	Content      types.String `tfsdk:"content"`
	File         types.String `tfsdk:"file"`
	URL          types.String `tfsdk:"url"`
	SourceSHA256 types.String `tfsdk:"source_sha256"`
	Activate     types.Bool   `tfsdk:"activate"`
	Version      types.Int64  `tfsdk:"version"`
	Status       types.String `tfsdk:"status"`
	Title        types.String `tfsdk:"title"`
	APIVersion   types.String `tfsdk:"api_version"`
	Operations   types.List   `tfsdk:"operations"`
}

func (r *contractVersionResource) Metadata(_ context.Context, req resource.MetadataRequest, resp *resource.MetadataResponse) {
	resp.TypeName = req.ProviderTypeName + "_contract_version"
}

func (r *contractVersionResource) Schema(_ context.Context, _ resource.SchemaRequest, resp *resource.SchemaResponse) {
	keep := []planmodifier.String{stringplanmodifier.UseStateForUnknown()}
	resp.Schema = schema.Schema{
		MarkdownDescription: "One contract version of a mock, for explicit version history: every change of the source creates a new version (versions are immutable, so this resource is replaced). Destroying it only forgets the version, which the Admin API keeps. Do not combine with `contract` on the same `mockingbird_mock`.",
		Attributes: map[string]schema.Attribute{
			"id":        idAttribute("<workspace>/<mock>/<version>"),
			"workspace": workspaceAttribute(),
			"mock": schema.StringAttribute{
				MarkdownDescription: "Name of the mock.",
				Required:            true,
				PlanModifiers:       []planmodifier.String{stringplanmodifier.RequiresReplace()},
			},
			"content": schema.StringAttribute{Optional: true, MarkdownDescription: "The contract inline."},
			"file":    schema.StringAttribute{Optional: true, MarkdownDescription: "Path of a contract file, read at plan time."},
			"url":     schema.StringAttribute{Optional: true, MarkdownDescription: "http(s) URL of the contract, fetched at plan time."},
			"source_sha256": schema.StringAttribute{
				MarkdownDescription: "Hex sha256 of the source; a change replaces the version.",
				Computed:            true,
			},
			"activate": schema.BoolAttribute{
				MarkdownDescription: "Keep this version active. When another version has been activated since, the next apply activates this one again. Default `true`.",
				Optional:            true,
				Computed:            true,
				Default:             booldefault.StaticBool(true),
			},
			"version": schema.Int64Attribute{
				MarkdownDescription: "The version number.",
				Computed:            true,
				PlanModifiers:       []planmodifier.Int64{int64planmodifier.UseStateForUnknown()},
			},
			"status":      schema.StringAttribute{MarkdownDescription: "`draft`, `approved`, `active` or `archived`.", Computed: true},
			"title":       schema.StringAttribute{MarkdownDescription: "The contract's `info.title`.", Computed: true, PlanModifiers: keep},
			"api_version": schema.StringAttribute{MarkdownDescription: "The contract's `info.version`.", Computed: true, PlanModifiers: keep},
			"operations": schema.ListAttribute{
				ElementType:         types.StringType,
				MarkdownDescription: "The operations, as targets such as `GET /orders/{id}`, `query user` or `/pkg.Service/Method`.",
				Computed:            true,
				PlanModifiers:       []planmodifier.List{listplanmodifier.UseStateForUnknown()},
			},
		},
	}
}

func (r *contractVersionResource) ConfigValidators(_ context.Context) []resource.ConfigValidator {
	return []resource.ConfigValidator{
		resourcevalidator.ExactlyOneOf(path.MatchRoot("content"), path.MatchRoot("file"), path.MatchRoot("url")),
	}
}

func (r *contractVersionResource) Configure(_ context.Context, req resource.ConfigureRequest, resp *resource.ConfigureResponse) {
	r.pd = providerDataFrom(req.ProviderData, &resp.Diagnostics)
}

func (m *contractVersionModel) source() contractSource {
	return contractSource{Content: m.Content, File: m.File, URL: m.URL}
}

func (r *contractVersionResource) ModifyPlan(ctx context.Context, req resource.ModifyPlanRequest, resp *resource.ModifyPlanResponse) {
	if req.Plan.Raw.IsNull() || r.pd == nil {
		return
	}
	var plan contractVersionModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	if resp.Diagnostics.HasError() {
		return
	}
	src := plan.source()
	if !src.known() {
		plan.SourceSHA256 = types.StringUnknown()
	} else if src.set() {
		b, err := src.load(ctx)
		if err != nil {
			resp.Diagnostics.AddError("Cannot read contract", err.Error())
			return
		}
		plan.SourceSHA256 = types.StringValue(sha256Hex(b))
	}
	replace := false
	if !req.State.Raw.IsNull() {
		var state contractVersionModel
		resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
		if state.SourceSHA256.Equal(plan.SourceSHA256) {
			plan.Status = state.Status
			if plan.Activate.ValueBool() && !state.Activate.ValueBool() {
				plan.Status = types.StringValue("active")
			}
		} else {
			// A new source is a new version.
			plan.Version = types.Int64Unknown()
			plan.Status = types.StringUnknown()
			plan.Title = types.StringUnknown()
			plan.APIVersion = types.StringUnknown()
			plan.Operations = types.ListUnknown(types.StringType)
			plan.ID = types.StringUnknown()
			replace = true
		}
	}
	resp.Diagnostics.Append(resp.Plan.Set(ctx, &plan)...)
	if replace {
		resp.RequiresReplace = append(resp.RequiresReplace, path.Root("source_sha256"))
	}
}

func (r *contractVersionResource) read(ctx context.Context, m *contractVersionModel) error {
	var cv client.ContractVersion
	if _, err := r.pd.client.Do(ctx, http.MethodGet, versionPath(m.Workspace.ValueString(), m.Mock.ValueString(), m.Version.ValueInt64()), nil, &cv); err != nil {
		return err
	}
	m.ID = types.StringValue(fmt.Sprintf("%s/%s/%d", m.Workspace.ValueString(), m.Mock.ValueString(), cv.Version))
	m.SourceSHA256 = types.StringValue(cv.SourceDigest)
	m.Status = types.StringValue(cv.Status)
	m.Title = types.StringValue(cv.Title)
	m.APIVersion = types.StringValue(cv.APIVersion)
	targets := make([]string, 0, len(cv.Operations))
	for _, op := range cv.Operations {
		targets = append(targets, op.Target)
	}
	m.Operations = listValue(targets, false)
	return nil
}

func (r *contractVersionResource) activate(ctx context.Context, m *contractVersionModel) error {
	p := versionPath(m.Workspace.ValueString(), m.Mock.ValueString(), m.Version.ValueInt64()) + "/activate"
	_, err := r.pd.client.Do(ctx, http.MethodPost, p, nil, nil)
	return err
}

func (r *contractVersionResource) Create(ctx context.Context, req resource.CreateRequest, resp *resource.CreateResponse) {
	var plan contractVersionModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	if resp.Diagnostics.HasError() {
		return
	}
	plan.Workspace = types.StringValue(r.pd.workspaceOr(plan.Workspace))
	src, err := plan.source().load(ctx)
	if err != nil {
		resp.Diagnostics.AddError("Cannot read contract", err.Error())
		return
	}
	p := client.WsPath(plan.Workspace.ValueString(), "/mocks/"+client.Seg(plan.Mock.ValueString())+"/contracts")
	if plan.Activate.ValueBool() {
		p += "?activate=true"
	}
	var res client.UploadResult
	if _, err := r.pd.client.Upload(ctx, p, uploadContentType, src, &res); err != nil {
		resp.Diagnostics.AddError("Uploading contract failed", err.Error())
		return
	}
	for _, w := range res.Warnings {
		resp.Diagnostics.AddWarning("Contract warning: "+w.Code, w.Message)
	}
	plan.Version = types.Int64Value(res.Version)
	if err := r.read(ctx, &plan); err != nil {
		resp.Diagnostics.AddError("Reading contract version failed", err.Error())
		return
	}
	resp.Diagnostics.Append(resp.State.Set(ctx, &plan)...)
}

func (r *contractVersionResource) Read(ctx context.Context, req resource.ReadRequest, resp *resource.ReadResponse) {
	var state contractVersionModel
	resp.Diagnostics.Append(req.State.Get(ctx, &state)...)
	if resp.Diagnostics.HasError() {
		return
	}
	if err := r.read(ctx, &state); err != nil {
		if client.IsNotFound(err) {
			resp.State.RemoveResource(ctx)
			return
		}
		resp.Diagnostics.AddError("Reading contract version failed", err.Error())
		return
	}
	// Another version activated since: plan to activate this one again.
	if state.Activate.ValueBool() && state.Status.ValueString() != "active" {
		state.Activate = types.BoolValue(false)
	}
	resp.Diagnostics.Append(resp.State.Set(ctx, &state)...)
}

func (r *contractVersionResource) Update(ctx context.Context, req resource.UpdateRequest, resp *resource.UpdateResponse) {
	var plan contractVersionModel
	resp.Diagnostics.Append(req.Plan.Get(ctx, &plan)...)
	if resp.Diagnostics.HasError() {
		return
	}
	if plan.Activate.ValueBool() {
		if err := r.activate(ctx, &plan); err != nil {
			resp.Diagnostics.AddError("Activating contract version failed", err.Error())
			return
		}
	}
	if err := r.read(ctx, &plan); err != nil {
		resp.Diagnostics.AddError("Reading contract version failed", err.Error())
		return
	}
	resp.Diagnostics.Append(resp.State.Set(ctx, &plan)...)
}

// Delete forgets the version: the Admin API keeps every version.
func (r *contractVersionResource) Delete(_ context.Context, _ resource.DeleteRequest, resp *resource.DeleteResponse) {
	resp.Diagnostics.AddWarning("Contract version kept",
		"The Admin API keeps every contract version; it was removed from the Terraform state only. Destroy the mock to remove its versions.")
}

func (r *contractVersionResource) ImportState(ctx context.Context, req resource.ImportStateRequest, resp *resource.ImportStateResponse) {
	parts, err := splitImportID(req.ID, 3, "<workspace>/<mock>/<version> or <mock>/<version>")
	if err != nil {
		resp.Diagnostics.AddError("Invalid import ID", err.Error())
		return
	}
	v, err := strconv.ParseInt(parts[2], 10, 64)
	if err != nil {
		resp.Diagnostics.AddError("Invalid import ID", "version must be a number")
		return
	}
	ws := parts[0]
	if ws == "" {
		ws = r.pd.workspace
	}
	resp.Diagnostics.Append(resp.State.SetAttribute(ctx, path.Root("workspace"), ws)...)
	resp.Diagnostics.Append(resp.State.SetAttribute(ctx, path.Root("mock"), parts[1])...)
	resp.Diagnostics.Append(resp.State.SetAttribute(ctx, path.Root("version"), v)...)
	resp.Diagnostics.Append(resp.State.SetAttribute(ctx, path.Root("activate"), true)...)
}
