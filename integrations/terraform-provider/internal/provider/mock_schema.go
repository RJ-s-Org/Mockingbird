package provider

import (
	"context"
	"fmt"
	"strings"

	"github.com/hashicorp/terraform-plugin-framework-validators/float64validator"
	"github.com/hashicorp/terraform-plugin-framework-validators/int64validator"
	"github.com/hashicorp/terraform-plugin-framework-validators/stringvalidator"
	"github.com/hashicorp/terraform-plugin-framework/diag"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema"
	"github.com/hashicorp/terraform-plugin-framework/schema/validator"
	"github.com/hashicorp/terraform-plugin-framework/types"

	"github.com/RJ-s-Org/terraform-provider-mockingbird/internal/client"
)

// The nested blocks a mock shares with its data source and scenarios.

type dataPlaneAuthModel struct {
	Mode   types.String `tfsdk:"mode"`
	Header types.String `tfsdk:"header"`
}

type latencyModel struct {
	MinMs types.Int64 `tfsdk:"min_ms"`
	MaxMs types.Int64 `tfsdk:"max_ms"`
}

type chaosModel struct {
	ErrorRate      types.Float64 `tfsdk:"error_rate"`
	DropStreamRate types.Float64 `tfsdk:"drop_stream_rate"`
	LatencyJitter  *latencyModel `tfsdk:"latency_jitter"`
}

type rateLimitModel struct {
	RequestsPerSecond types.Float64 `tfsdk:"requests_per_second"`
	Burst             types.Int64   `tfsdk:"burst"`
}

type limitsModel struct {
	RateLimit        *rateLimitModel `tfsdk:"rate_limit"`
	DailyTokenBudget types.Int64     `tfsdk:"daily_token_budget"`
	MaxRequestBytes  types.Int64     `tfsdk:"max_request_bytes"`
	MaxResponseBytes types.Int64     `tfsdk:"max_response_bytes"`
}

type webhookModel struct {
	URL     types.String `tfsdk:"url"`
	Events  types.List   `tfsdk:"events"`
	Secret  types.String `tfsdk:"secret"`
	Signing types.String `tfsdk:"signing"`
	DelayMs types.Int64  `tfsdk:"delay_ms"`
}

type contractModel struct {
	Content  types.String `tfsdk:"content"`
	File     types.String `tfsdk:"file"`
	URL      types.String `tfsdk:"url"`
	Activate types.Bool   `tfsdk:"activate"`
}

func (c *contractModel) source() contractSource {
	return contractSource{Content: c.Content, File: c.File, URL: c.URL}
}

// activate is whether an upload activates the version (default true).
func (c *contractModel) activate() bool {
	return c.Activate.IsNull() || c.Activate.IsUnknown() || c.Activate.ValueBool()
}

func secretRefValidator() validator.String {
	return stringvalidator.RegexMatches(
		regexpMust(`^(env|file|vault):.+`),
		"must be a secret reference (env:NAME, file:/path or vault:path#key), never a secret value",
	)
}

func chaosSchema() schema.SingleNestedAttribute {
	return schema.SingleNestedAttribute{
		MarkdownDescription: "Fault injection.",
		Optional:            true,
		Attributes: map[string]schema.Attribute{
			"error_rate": schema.Float64Attribute{
				MarkdownDescription: "Probability (0–1) that a call answers with an injected error.",
				Optional:            true,
				Validators:          []validator.Float64{float64validator.Between(0, 1)},
			},
			"drop_stream_rate": schema.Float64Attribute{
				MarkdownDescription: "Probability (0–1) that a streamed response is cut short.",
				Optional:            true,
				Validators:          []validator.Float64{float64validator.Between(0, 1)},
			},
			"latency_jitter": schema.SingleNestedAttribute{
				MarkdownDescription: "Uniformly random added latency.",
				Optional:            true,
				Attributes: map[string]schema.Attribute{
					"min_ms": schema.Int64Attribute{Required: true, Validators: []validator.Int64{int64validator.AtLeast(0)}},
					"max_ms": schema.Int64Attribute{Required: true, MarkdownDescription: "At most 30000 and at least `min_ms`.", Validators: []validator.Int64{int64validator.Between(0, 30000)}},
				},
			},
		},
	}
}

func mockResourceAttributes() map[string]schema.Attribute {
	return map[string]schema.Attribute{
		"data_plane_auth": schema.SingleNestedAttribute{
			MarkdownDescription: "How callers of the mock's data plane authenticate. Unset means no authentication.",
			Optional:            true,
			Attributes: map[string]schema.Attribute{
				"mode": schema.StringAttribute{
					MarkdownDescription: "`none`, `api_key` (a `data-plane` API key of the workspace is required) or `passthrough_validate` (credentials must be present as the contract declares, without being verified).",
					Required:            true,
					Validators:          []validator.String{stringvalidator.OneOf("none", "api_key", "passthrough_validate")},
				},
				"header": schema.StringAttribute{
					MarkdownDescription: "Lowercase header carrying the key in `api_key` mode. Server default `x-api-key`.",
					Optional:            true,
				},
			},
		},
		"chaos": chaosSchema(),
		"limits": schema.SingleNestedAttribute{
			MarkdownDescription: "Per-mock limits; unset members fall back to the global limits.",
			Optional:            true,
			Attributes: map[string]schema.Attribute{
				"rate_limit": schema.SingleNestedAttribute{
					MarkdownDescription: "A token bucket.",
					Optional:            true,
					Attributes: map[string]schema.Attribute{
						"requests_per_second": schema.Float64Attribute{Required: true, MarkdownDescription: "Greater than 0, at most 100000."},
						"burst":               schema.Int64Attribute{Required: true, MarkdownDescription: "1–100000.", Validators: []validator.Int64{int64validator.Between(1, 100000)}},
					},
				},
				"daily_token_budget": schema.Int64Attribute{Optional: true, MarkdownDescription: "LLM tokens per UTC day across all replicas.", Validators: []validator.Int64{int64validator.AtLeast(1)}},
				"max_request_bytes":  schema.Int64Attribute{Optional: true, MarkdownDescription: "1024–67108864.", Validators: []validator.Int64{int64validator.Between(1024, 67108864)}},
				"max_response_bytes": schema.Int64Attribute{Optional: true, MarkdownDescription: "1024–16777216.", Validators: []validator.Int64{int64validator.Between(1024, 16777216)}},
			},
		},
		"webhooks": schema.ListNestedAttribute{
			MarkdownDescription: "Where the mock POSTs events after successful writes (at most 16 targets).",
			Optional:            true,
			NestedObject: schema.NestedAttributeObject{
				Attributes: map[string]schema.Attribute{
					"url": schema.StringAttribute{Required: true, MarkdownDescription: "Absolute `http` or `https` URL events are POSTed to."},
					"events": schema.ListAttribute{
						ElementType:         types.StringType,
						Optional:            true,
						MarkdownDescription: "Operation ids, event types (`customer.created`) or `*`. Server default `[\"*\"]`.",
					},
					"secret": schema.StringAttribute{
						Optional:            true,
						MarkdownDescription: "Signing secret as a reference (`env:NAME`, `file:/path`, `vault:path#key`) allowed by the server's `server.api_webhook_secret_refs`; never a value.",
						Validators:          []validator.String{secretRefValidator()},
					},
					"signing": schema.StringAttribute{
						Optional:            true,
						MarkdownDescription: "`stripe`, `hmac-sha256` or `none`. Server default `hmac-sha256` with a secret, else `none`.",
						Validators:          []validator.String{stringvalidator.OneOf("stripe", "hmac-sha256", "none")},
					},
					"delay_ms": schema.Int64Attribute{Optional: true, MarkdownDescription: "Wait before the first attempt, 0–60000.", Validators: []validator.Int64{int64validator.Between(0, 60000)}},
				},
			},
		},
		"contract": schema.SingleNestedAttribute{
			MarkdownDescription: "The contract the mock serves: OpenAPI 3.x or Swagger 2.0, GraphQL SDL or introspection JSON, a `.proto` file or FileDescriptorSet, WSDL 1.1, AsyncAPI 2.x/3.0, or a zip/tar.gz archive of several files. Exactly one of `content`, `file` or `url`. The provider hashes the source at plan time and uploads a new contract version when the sha256 differs from the active version's `source_digest`, so editing the file (or the document behind the URL) is a normal plan diff. Removing `contract` leaves the uploaded versions in place. For explicit version history, use `mockingbird_contract_version` instead.",
			Optional:            true,
			Attributes: map[string]schema.Attribute{
				"content": schema.StringAttribute{Optional: true, MarkdownDescription: "The contract inline, e.g. `file(\"openapi.yaml\")` or a heredoc."},
				"file":    schema.StringAttribute{Optional: true, MarkdownDescription: "Path of a contract file, read at plan time."},
				"url":     schema.StringAttribute{Optional: true, MarkdownDescription: "http(s) URL of the contract, fetched at plan time (at most 20 MiB)."},
				"activate": schema.BoolAttribute{
					Optional:            true,
					MarkdownDescription: "Activate each uploaded version (default `true`). With `false`, a new version is uploaded but the mock keeps serving its active one (its first version is activated regardless).",
				},
			},
		},
	}
}

// --- conversions between the model and the API -------------------------

func chaosToAPI(c *chaosModel) *client.Chaos {
	if c == nil {
		return nil
	}
	out := &client.Chaos{ErrorRate: c.ErrorRate.ValueFloat64(), DropStreamRate: c.DropStreamRate.ValueFloat64()}
	if c.LatencyJitter != nil {
		out.LatencyJitter = &client.LatencyJitter{MinMs: c.LatencyJitter.MinMs.ValueInt64(), MaxMs: c.LatencyJitter.MaxMs.ValueInt64()}
	}
	return out
}

func floatOrNull(v float64, prior types.Float64) types.Float64 {
	if v == 0 && prior.IsNull() {
		return types.Float64Null()
	}
	return types.Float64Value(v)
}

func chaosFromAPI(c *client.Chaos, prior *chaosModel) *chaosModel {
	if c == nil {
		return nil
	}
	if prior == nil {
		prior = &chaosModel{ErrorRate: types.Float64Null(), DropStreamRate: types.Float64Null()}
	}
	out := &chaosModel{
		ErrorRate:      floatOrNull(c.ErrorRate, prior.ErrorRate),
		DropStreamRate: floatOrNull(c.DropStreamRate, prior.DropStreamRate),
	}
	if c.LatencyJitter != nil {
		out.LatencyJitter = &latencyModel{MinMs: types.Int64Value(c.LatencyJitter.MinMs), MaxMs: types.Int64Value(c.LatencyJitter.MaxMs)}
	}
	return out
}

func limitsToAPI(l *limitsModel) client.Limits {
	if l == nil {
		return client.Limits{}
	}
	out := client.Limits{
		DailyTokenBudget: int64Ptr(l.DailyTokenBudget),
		MaxRequestBytes:  int64Ptr(l.MaxRequestBytes),
		MaxResponseBytes: int64Ptr(l.MaxResponseBytes),
	}
	if l.RateLimit != nil {
		out.RateLimit = &client.RateLimit{RequestsPerSecond: l.RateLimit.RequestsPerSecond.ValueFloat64(), Burst: l.RateLimit.Burst.ValueInt64()}
	}
	return out
}

// limitsFromAPI is nil for no limits, unless prior configured an empty
// object.
func limitsFromAPI(l client.Limits, prior *limitsModel) *limitsModel {
	if l.RateLimit == nil && l.DailyTokenBudget == nil && l.MaxRequestBytes == nil && l.MaxResponseBytes == nil && prior == nil {
		return nil
	}
	out := &limitsModel{
		DailyTokenBudget: int64Value(l.DailyTokenBudget),
		MaxRequestBytes:  int64Value(l.MaxRequestBytes),
		MaxResponseBytes: int64Value(l.MaxResponseBytes),
	}
	if l.RateLimit != nil {
		out.RateLimit = &rateLimitModel{RequestsPerSecond: types.Float64Value(l.RateLimit.RequestsPerSecond), Burst: types.Int64Value(l.RateLimit.Burst)}
	}
	return out
}

func dataPlaneAuthToAPI(d *dataPlaneAuthModel) client.DataPlaneAuth {
	if d == nil {
		return client.DataPlaneAuth{Mode: "none"}
	}
	return client.DataPlaneAuth{Mode: d.Mode.ValueString(), Header: d.Header.ValueString()}
}

const defaultDataPlaneHeader = "x-api-key"

func dataPlaneAuthFromAPI(d client.DataPlaneAuth, prior *dataPlaneAuthModel) *dataPlaneAuthModel {
	if prior == nil && d.Mode == "none" && (d.Header == "" || d.Header == defaultDataPlaneHeader) {
		return nil
	}
	out := &dataPlaneAuthModel{Mode: types.StringValue(d.Mode), Header: types.StringValue(d.Header)}
	if d.Header == defaultDataPlaneHeader && (prior == nil || prior.Header.IsNull()) {
		out.Header = types.StringNull()
	}
	return out
}

func webhooksToAPI(ctx context.Context, ws []webhookModel, diags *diag.Diagnostics) []client.Webhook {
	out := make([]client.Webhook, 0, len(ws))
	for _, w := range ws {
		out = append(out, client.Webhook{
			URL:     w.URL.ValueString(),
			Events:  stringList(ctx, w.Events, diags),
			Secret:  strPtr(w.Secret),
			Signing: strPtr(w.Signing),
			DelayMs: int64Ptr(w.DelayMs),
		})
	}
	return out
}

// redactedURL is how the Admin API shows a webhook URL: its query replaced.
func redactedURL(u string) string {
	base, _, ok := strings.Cut(u, "?")
	if !ok {
		return u
	}
	base, _, _ = strings.Cut(base, "#")
	return base + "?<redacted>"
}

func webhooksFromAPI(ws []client.Webhook, prior []webhookModel) []webhookModel {
	if len(ws) == 0 {
		if prior != nil && len(prior) == 0 {
			return []webhookModel{}
		}
		return nil
	}
	out := make([]webhookModel, 0, len(ws))
	for i, w := range ws {
		var p *webhookModel
		if i < len(prior) {
			p = &prior[i]
		}
		m := webhookModel{
			URL:     types.StringValue(w.URL),
			Events:  listValue(w.Events, false),
			Secret:  strValue(w.Secret),
			Signing: strValue(w.Signing),
			DelayMs: int64Value(w.DelayMs),
		}
		if p != nil {
			// The API never shows a URL's query: keep the configured one.
			if redactedURL(p.URL.ValueString()) == w.URL {
				m.URL = p.URL
			}
			if p.Events.IsNull() && len(w.Events) == 1 && w.Events[0] == "*" {
				m.Events = types.ListNull(types.StringType)
			}
			if p.Signing.IsNull() {
				m.Signing = types.StringNull()
			}
			if p.DelayMs.IsNull() && (w.DelayMs == nil || *w.DelayMs == 0) {
				m.DelayMs = types.Int64Null()
			}
		} else if len(w.Events) == 1 && w.Events[0] == "*" {
			m.Events = types.ListNull(types.StringType)
		}
		out = append(out, m)
	}
	return out
}

// mockURL is the data-plane URL of a mock with route prefix prefix.
func (pd *providerData) mockURL(prefix string) types.String {
	if pd.dataPlaneURL == "" {
		return types.StringNull()
	}
	return types.StringValue(pd.dataPlaneURL + prefix)
}

// protocolsOf lists the distinct protocols of a contract version's
// operations and channels, sorted.
func protocolsOf(v *client.ContractVersion) []string {
	seen := map[string]bool{}
	var out []string
	add := func(p string) {
		if p != "" && !seen[p] {
			seen[p] = true
			out = append(out, p)
		}
	}
	for _, op := range v.Operations {
		add(op.Protocol)
	}
	for _, ch := range v.Channels {
		add(ch.Protocol)
	}
	sortStrings(out)
	return out
}

func mockPath(ws, name string) string {
	return client.WsPath(ws, "/mocks/"+client.Seg(name))
}

func versionPath(ws, mock string, version int64) string {
	return client.WsPath(ws, fmt.Sprintf("/mocks/%s/contracts/%d", client.Seg(mock), version))
}
