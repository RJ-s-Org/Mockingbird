package client

import "encoding/json"

// Workspace is a workspace view.
type Workspace struct {
	Name        string `json:"name"`
	DisplayName string `json:"display_name"`
	CreatedAt   string `json:"created_at"`
}

// DataPlaneAuth is how callers of a mock's data plane authenticate.
type DataPlaneAuth struct {
	Mode   string `json:"mode"`
	Header string `json:"header,omitempty"`
}

// LatencyJitter is a uniformly random added latency.
type LatencyJitter struct {
	MinMs int64 `json:"min_ms"`
	MaxMs int64 `json:"max_ms"`
}

// Chaos is fault injection.
type Chaos struct {
	ErrorRate      float64        `json:"error_rate"`
	LatencyJitter  *LatencyJitter `json:"latency_jitter,omitempty"`
	DropStreamRate float64        `json:"drop_stream_rate"`
}

// RateLimit is a token bucket.
type RateLimit struct {
	RequestsPerSecond float64 `json:"requests_per_second"`
	Burst             int64   `json:"burst"`
}

// Limits are a mock's per-mock limits.
type Limits struct {
	RateLimit        *RateLimit `json:"rate_limit,omitempty"`
	DailyTokenBudget *int64     `json:"daily_token_budget,omitempty"`
	MaxRequestBytes  *int64     `json:"max_request_bytes,omitempty"`
	MaxResponseBytes *int64     `json:"max_response_bytes,omitempty"`
}

// Webhook is a webhook target of a mock.
type Webhook struct {
	URL     string   `json:"url"`
	Events  []string `json:"events,omitempty"`
	Secret  *string  `json:"secret,omitempty"`
	Signing *string  `json:"signing,omitempty"`
	DelayMs *int64   `json:"delay_ms,omitempty"`
}

// Mock is a mock view.
type Mock struct {
	ID              string        `json:"id"`
	Workspace       string        `json:"workspace"`
	Name            string        `json:"name"`
	RoutePrefix     string        `json:"route_prefix"`
	Persona         *string       `json:"persona"`
	LLMProfile      *string       `json:"llm_profile"`
	HeaderOverrides bool          `json:"header_overrides"`
	DataPlaneAuth   DataPlaneAuth `json:"data_plane_auth"`
	Chaos           *Chaos        `json:"chaos"`
	Limits          Limits        `json:"limits"`
	Consistency     string        `json:"consistency"`
	UnknownIDs      string        `json:"unknown_ids"`
	DefaultScenario *string       `json:"default_scenario"`
	Hosts           []string      `json:"hosts"`
	Webhooks        []Webhook     `json:"webhooks"`
	Source          string        `json:"source"`
	ActiveVersion   *int64        `json:"active_version"`
}

// MockWrite is the body of a mock create (POST) or merge patch (PATCH). Every
// field is sent, so a PATCH replaces the whole managed configuration.
type MockWrite struct {
	Name            string        `json:"name,omitempty"`
	RoutePrefix     *string       `json:"route_prefix,omitempty"`
	Persona         *string       `json:"persona"`
	LLMProfile      *string       `json:"llm_profile"`
	HeaderOverrides bool          `json:"header_overrides"`
	DataPlaneAuth   DataPlaneAuth `json:"data_plane_auth"`
	Chaos           *Chaos        `json:"chaos"`
	Limits          Limits        `json:"limits"`
	Consistency     string        `json:"consistency,omitempty"`
	UnknownIDs      string        `json:"unknown_ids,omitempty"`
	Hosts           []string      `json:"hosts"`
	Webhooks        []Webhook     `json:"webhooks"`
}

// ContractWarning is a non-fatal compilation finding.
type ContractWarning struct {
	Code      string  `json:"code"`
	Operation *string `json:"operation"`
	Message   string  `json:"message"`
}

// UploadResult is the outcome of a contract upload.
type UploadResult struct {
	Version    int64             `json:"version"`
	Status     string            `json:"status"`
	Operations int64             `json:"operations"`
	Channels   int64             `json:"channels"`
	Warnings   []ContractWarning `json:"warnings"`
}

// Operation is one operation of a compiled contract.
type Operation struct {
	ID       string `json:"id"`
	Method   string `json:"method"`
	Path     string `json:"path"`
	Protocol string `json:"protocol"`
	Target   string `json:"target"`
}

// ContractChannel is one event channel of a compiled contract.
type ContractChannel struct {
	ID       string `json:"id"`
	Protocol string `json:"protocol"`
}

// ContractVersion is a contract version's metadata, operations and channels.
type ContractVersion struct {
	Version      int64             `json:"version"`
	Status       string            `json:"status"`
	SourceDigest string            `json:"source_digest"`
	Title        string            `json:"title"`
	APIVersion   string            `json:"api_version"`
	Operations   []Operation       `json:"operations"`
	Channels     []ContractChannel `json:"channels"`
}

// Scenario is a scenario view, and (without Name and Source) its PUT body.
type Scenario struct {
	Name                  string            `json:"name,omitempty"`
	Instructions          *string           `json:"instructions,omitempty"`
	Status                *int64            `json:"status,omitempty"`
	LatencyMs             *int64            `json:"latency_ms,omitempty"`
	Set                   json.RawMessage   `json:"set,omitempty"`
	Operations            []string          `json:"operations"`
	StreamItems           *int64            `json:"stream_items,omitempty"`
	StreamItemIntervalMs  *int64            `json:"stream_item_interval_ms,omitempty"`
	Chaos                 *Chaos            `json:"chaos,omitempty"`
	OperationInstructions map[string]string `json:"operation_instructions,omitempty"`
	Source                string            `json:"source,omitempty"`
}

// LLMProfile is an LLM profile view, and (without Name and Source) its PUT
// body.
type LLMProfile struct {
	Name            string            `json:"name,omitempty"`
	BaseURL         string            `json:"base_url"`
	Model           string            `json:"model"`
	SecretRef       *string           `json:"secret_ref,omitempty"`
	JSONSchema      bool              `json:"json_schema"`
	Temperature     float64           `json:"temperature"`
	TimeoutMs       int64             `json:"timeout_ms"`
	MaxTokens       int64             `json:"max_tokens"`
	FallbackProfile *string           `json:"fallback_profile,omitempty"`
	Headers         map[string]string `json:"headers"`
	ExtraBody       json.RawMessage   `json:"extra_body,omitempty"`
	Source          string            `json:"source,omitempty"`
}

// APIKey is an API key view.
type APIKey struct {
	KeyID      string   `json:"key_id"`
	Name       string   `json:"name"`
	Role       string   `json:"role"`
	Scopes     []string `json:"scopes"`
	Sysadmin   bool     `json:"sysadmin"`
	CreatedBy  string   `json:"created_by"`
	CreatedAt  string   `json:"created_at"`
	ExpiresAt  *string  `json:"expires_at"`
	LastUsedAt *string  `json:"last_used_at"`
	RevokedAt  *string  `json:"revoked_at"`
}

// CreateAPIKey is the body of an API key creation.
type CreateAPIKey struct {
	Name      string   `json:"name"`
	Role      string   `json:"role"`
	Scopes    []string `json:"scopes"`
	ExpiresAt *string  `json:"expires_at,omitempty"`
	Sysadmin  bool     `json:"sysadmin"`
}

// CreatedAPIKey is a new key with its token, shown only once.
type CreatedAPIKey struct {
	Key   APIKey `json:"key"`
	Token string `json:"token"`
}

// Channel is a channel of a mock's active contract with its effective
// settings.
type Channel struct {
	ID             string            `json:"id"`
	Direction      string            `json:"direction"`
	Protocol       string            `json:"protocol"`
	Address        string            `json:"address"`
	Enabled        bool              `json:"enabled"`
	Triggers       []json.RawMessage `json:"triggers"`
	SettingsSource string            `json:"settings_source"`
}

// PutChannel is a channel's settings.
type PutChannel struct {
	Enabled  bool              `json:"enabled"`
	Triggers []json.RawMessage `json:"triggers,omitempty"`
}

// EventSettings are a mock's event settings.
type EventSettings struct {
	Broker *string `json:"broker"`
	Source string  `json:"source,omitempty"`
}
