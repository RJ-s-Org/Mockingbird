package provider

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"os"
	"regexp"
	"sort"
	"strings"
	"time"

	"github.com/hashicorp/terraform-plugin-framework/attr"
	"github.com/hashicorp/terraform-plugin-framework/diag"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/planmodifier"
	"github.com/hashicorp/terraform-plugin-framework/resource/schema/stringplanmodifier"
	"github.com/hashicorp/terraform-plugin-framework/types"
)

// providerDataFrom unwraps the data Configure hands to resources and data
// sources; nil before the provider is configured.
func providerDataFrom(data any, diags *diag.Diagnostics) *providerData {
	if data == nil {
		return nil
	}
	pd, ok := data.(*providerData)
	if !ok {
		diags.AddError("Unexpected provider data", fmt.Sprintf("expected *providerData, got %T", data))
		return nil
	}
	return pd
}

// workspaceAttribute is the optional `workspace` of every workspace
// resource: the provider's workspace when unset, and replaced when changed.
func workspaceAttribute() schema.StringAttribute {
	return schema.StringAttribute{
		MarkdownDescription: "Workspace of the resource. Defaults to the provider's `workspace`.",
		Optional:            true,
		Computed:            true,
		PlanModifiers: []planmodifier.String{
			stringplanmodifier.UseStateForUnknown(),
			stringplanmodifier.RequiresReplace(),
		},
	}
}

// idAttribute is the computed `id` of every resource.
func idAttribute(format string) schema.StringAttribute {
	return schema.StringAttribute{
		MarkdownDescription: "Identifier, `" + format + "`; also the import ID.",
		Computed:            true,
		PlanModifiers:       []planmodifier.String{stringplanmodifier.UseStateForUnknown()},
	}
}

// workspaceOr is ws when it is known and set, else the provider's workspace.
func (pd *providerData) workspaceOr(ws types.String) string {
	if ws.IsNull() || ws.IsUnknown() || ws.ValueString() == "" {
		return pd.workspace
	}
	return ws.ValueString()
}

// splitImportID splits id at `/` into exactly n non-empty parts; with n-1
// parts the first is taken to be the default workspace.
func splitImportID(id string, n int, format string) ([]string, error) {
	parts := strings.Split(id, "/")
	if len(parts) == n-1 {
		parts = append([]string{""}, parts...)
	}
	if len(parts) != n {
		return nil, fmt.Errorf("import ID %q is not of the form %s", id, format)
	}
	for i, p := range parts {
		if p == "" && i > 0 {
			return nil, fmt.Errorf("import ID %q is not of the form %s", id, format)
		}
	}
	return parts, nil
}

func strPtr(v types.String) *string {
	if v.IsNull() || v.IsUnknown() {
		return nil
	}
	s := v.ValueString()
	return &s
}

func int64Ptr(v types.Int64) *int64 {
	if v.IsNull() || v.IsUnknown() {
		return nil
	}
	n := v.ValueInt64()
	return &n
}

func strValue(p *string) types.String {
	if p == nil {
		return types.StringNull()
	}
	return types.StringValue(*p)
}

func int64Value(p *int64) types.Int64 {
	if p == nil {
		return types.Int64Null()
	}
	return types.Int64Value(*p)
}

// stringList converts a list of strings; null and unknown give nil.
func stringList(ctx context.Context, l types.List, diags *diag.Diagnostics) []string {
	if l.IsNull() || l.IsUnknown() {
		return nil
	}
	var out []string
	diags.Append(l.ElementsAs(ctx, &out, false)...)
	return out
}

// listValue is a list of strings; an empty list is null when nullIfEmpty.
func listValue(vals []string, nullIfEmpty bool) types.List {
	if len(vals) == 0 && nullIfEmpty {
		return types.ListNull(types.StringType)
	}
	elems := make([]attr.Value, 0, len(vals))
	for _, v := range vals {
		elems = append(elems, types.StringValue(v))
	}
	return types.ListValueMust(types.StringType, elems)
}

// jsonEqual reports whether two JSON documents are semantically equal.
func jsonEqual(a, b string) bool {
	var x, y any
	if json.Unmarshal([]byte(a), &x) != nil || json.Unmarshal([]byte(b), &y) != nil {
		return false
	}
	xa, _ := json.Marshal(x)
	ya, _ := json.Marshal(y)
	return string(xa) == string(ya)
}

// jsonFromServer is the server's JSON document as a string attribute,
// keeping prior's formatting when the two are semantically equal (so a
// hand-formatted `jsonencode` or heredoc does not show a diff).
func jsonFromServer(raw json.RawMessage, prior types.String, emptyAsNull bool) types.String {
	s := strings.TrimSpace(string(raw))
	if s == "" || s == "null" || (emptyAsNull && (s == "{}" || s == "[]")) {
		if !prior.IsNull() && !prior.IsUnknown() && jsonEqual(prior.ValueString(), orEmpty(s)) {
			return prior
		}
		return types.StringNull()
	}
	if !prior.IsNull() && !prior.IsUnknown() && jsonEqual(prior.ValueString(), s) {
		return prior
	}
	return types.StringValue(s)
}

func orEmpty(s string) string {
	if s == "" || s == "null" {
		return "null"
	}
	return s
}

// contractSource is where a contract comes from: exactly one of inline
// content, a local file or an http(s) URL.
type contractSource struct {
	Content types.String
	File    types.String
	URL     types.String
}

// known reports whether every source attribute is known (not computed
// during this plan).
func (s contractSource) known() bool {
	return !s.Content.IsUnknown() && !s.File.IsUnknown() && !s.URL.IsUnknown()
}

// set reports whether any source is configured.
func (s contractSource) set() bool {
	return !s.Content.IsNull() || !s.File.IsNull() || !s.URL.IsNull()
}

// load reads the contract bytes.
func (s contractSource) load(ctx context.Context) ([]byte, error) {
	n := 0
	for _, v := range []types.String{s.Content, s.File, s.URL} {
		if !v.IsNull() {
			n++
		}
	}
	if n != 1 {
		return nil, fmt.Errorf("exactly one of content, file or url must be set")
	}
	switch {
	case !s.Content.IsNull():
		return []byte(s.Content.ValueString()), nil
	case !s.File.IsNull():
		b, err := os.ReadFile(s.File.ValueString())
		if err != nil {
			return nil, fmt.Errorf("reading contract file: %w", err)
		}
		return b, nil
	default:
		return fetchContract(ctx, s.URL.ValueString())
	}
}

// maxContractBytes is the Admin API's upload limit.
const maxContractBytes = 20 << 20

func fetchContract(ctx context.Context, url string) ([]byte, error) {
	if !strings.HasPrefix(url, "https://") && !strings.HasPrefix(url, "http://") {
		return nil, fmt.Errorf("contract url %q must be http(s)", url)
	}
	ctx, cancel := context.WithTimeout(ctx, time.Minute)
	defer cancel()
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, url, nil)
	if err != nil {
		return nil, err
	}
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		return nil, fmt.Errorf("fetching contract: %w", err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return nil, fmt.Errorf("fetching contract %s: HTTP %d", url, resp.StatusCode)
	}
	b, err := io.ReadAll(io.LimitReader(resp.Body, maxContractBytes+1))
	if err != nil {
		return nil, fmt.Errorf("fetching contract: %w", err)
	}
	if len(b) > maxContractBytes {
		return nil, fmt.Errorf("contract at %s exceeds %d bytes", url, maxContractBytes)
	}
	return b, nil
}

// sha256Hex is the digest the Admin API reports as `source_digest`.
func sha256Hex(b []byte) string {
	h := sha256.Sum256(b)
	return hex.EncodeToString(h[:])
}

// uploadContentType is sent with every contract upload: the server sniffs
// the format from the bytes, so one accepted type covers every format.
const uploadContentType = "application/octet-stream"

func regexpMust(expr string) *regexp.Regexp { return regexp.MustCompile(expr) }

func sortStrings(s []string) { sort.Strings(s) }

// mapValue is a map of strings; an empty map is null when nullIfEmpty.
func mapValue(vals map[string]string, nullIfEmpty bool) types.Map {
	if len(vals) == 0 && nullIfEmpty {
		return types.MapNull(types.StringType)
	}
	elems := make(map[string]attr.Value, len(vals))
	for k, v := range vals {
		elems[k] = types.StringValue(v)
	}
	return types.MapValueMust(types.StringType, elems)
}

// sameTime reports whether two RFC 3339 times are the same instant.
func sameTime(a, b string) bool {
	ta, errA := time.Parse(time.RFC3339Nano, a)
	tb, errB := time.Parse(time.RFC3339Nano, b)
	return errA == nil && errB == nil && ta.Equal(tb)
}
