package provider

import (
	"encoding/json"
	"testing"

	"github.com/hashicorp/terraform-plugin-framework/types"
)

func TestSplitImportID(t *testing.T) {
	parts, err := splitImportID("staging/orders", 2, "")
	if err != nil || parts[0] != "staging" || parts[1] != "orders" {
		t.Fatalf("got %v, %v", parts, err)
	}
	parts, err = splitImportID("orders", 2, "")
	if err != nil || parts[0] != "" || parts[1] != "orders" {
		t.Fatalf("default workspace: got %v, %v", parts, err)
	}
	for _, bad := range []string{"a/b/c", "a/", ""} {
		if _, err := splitImportID(bad, 2, ""); err == nil {
			t.Errorf("%q: expected an error", bad)
		}
	}
}

func TestRedactedURL(t *testing.T) {
	cases := map[string]string{
		"https://h.test/in":                "https://h.test/in",
		"https://h.test/in?token=abc":      "https://h.test/in?<redacted>",
		"https://h.test/in#frag?token=abc": "https://h.test/in?<redacted>",
	}
	for in, want := range cases {
		if got := redactedURL(in); got != want {
			t.Errorf("redactedURL(%q) = %q, want %q", in, got, want)
		}
	}
}

func TestJSONFromServerKeepsFormatting(t *testing.T) {
	prior := types.StringValue("{ \"b\": 1, \"a\": [1, 2] }")
	if got := jsonFromServer(json.RawMessage(`{"a":[1,2],"b":1}`), prior, false); !got.Equal(prior) {
		t.Errorf("equal documents should keep the configured spelling, got %s", got)
	}
	if got := jsonFromServer(json.RawMessage(`{"a":[1]}`), prior, false); got.ValueString() != `{"a":[1]}` {
		t.Errorf("a changed document should show, got %s", got)
	}
	if got := jsonFromServer(json.RawMessage(`{}`), types.StringNull(), true); !got.IsNull() {
		t.Errorf("an empty object should be null, got %s", got)
	}
}

func TestShortestFloat32(t *testing.T) {
	if got := shortestFloat32(float64(float32(0.2))); got != 0.2 {
		t.Errorf("got %v", got)
	}
}

func TestSecretRefPattern(t *testing.T) {
	re := regexpMust(`^(env|file|vault):.+`)
	for _, ok := range []string{"env:MB_LLM_API_KEY", "file:/run/secrets/k", "vault:kv/llm#key"} {
		if !re.MatchString(ok) {
			t.Errorf("%q should be accepted", ok)
		}
	}
	for _, bad := range []string{"sk-123", "env:", "MB_LLM_API_KEY"} {
		if re.MatchString(bad) {
			t.Errorf("%q should be rejected", bad)
		}
	}
}
