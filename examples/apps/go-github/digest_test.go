package digest

import (
	"context"
	"crypto/tls"
	"crypto/x509"
	"encoding/json"
	"errors"
	"net/http"
	"net/url"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/google/go-github/v66/github"
)

const (
	port, admin, grpcPort, proxyPort = "49080", "49090", "49051", "49888"
)

// client starts `mockingbird serve --offline` with its proxy and returns an
// HTTP client configured from `mockingbird proxy env --json`, as the app
// would be with HTTPS_PROXY and the CA bundle exported. (Go on macOS ignores
// SSL_CERT_FILE, so the CA is loaded explicitly.)
func client(t *testing.T) *http.Client {
	t.Helper()
	bin := os.Getenv("MOCKINGBIRD_BIN")
	if bin == "" {
		bin = "mockingbird"
	}
	caDir := t.TempDir()
	env := append(os.Environ(), "MB_CA_DIR="+caDir)
	cmd := exec.Command(bin, "serve", "-c", "mockingbird.yaml", "--offline",
		"--port", port, "--admin-port", admin, "--grpc-port", grpcPort, "--proxy-port", proxyPort)
	cmd.Env = env
	cmd.Stdout, cmd.Stderr = os.Stderr, os.Stderr
	if err := cmd.Start(); err != nil {
		t.Fatalf("starting mockingbird: %v", err)
	}
	t.Cleanup(func() { _ = cmd.Process.Kill(); _ = cmd.Wait() })

	deadline := time.Now().Add(30 * time.Second)
	for {
		resp, err := http.Get("http://127.0.0.1:" + admin + "/readyz")
		if err == nil && resp.StatusCode == http.StatusOK {
			resp.Body.Close()
			break
		}
		if time.Now().After(deadline) {
			t.Fatalf("mockingbird was not ready: %v", err)
		}
		time.Sleep(200 * time.Millisecond)
	}

	penvCmd := exec.Command(bin, "proxy", "env", "--json", "--port", proxyPort)
	penvCmd.Env = env
	out, err := penvCmd.Output()
	if err != nil {
		t.Fatalf("proxy env: %v", err)
	}
	var penv struct {
		Vars   map[string]string `json:"vars"`
		CAPath string            `json:"ca_path"`
	}
	if err := json.Unmarshal(out, &penv); err != nil {
		t.Fatal(err)
	}
	proxyURL, err := url.Parse(penv.Vars["HTTPS_PROXY"])
	if err != nil {
		t.Fatal(err)
	}
	pem, err := os.ReadFile(filepath.Clean(penv.CAPath))
	if err != nil {
		t.Fatal(err)
	}
	roots, err := x509.SystemCertPool()
	if err != nil {
		roots = x509.NewCertPool()
	}
	roots.AppendCertsFromPEM(pem)
	return &http.Client{
		Timeout: 30 * time.Second,
		Transport: &http.Transport{
			Proxy:           http.ProxyURL(proxyURL),
			TLSClientConfig: &tls.Config{RootCAs: roots},
		},
	}
}

func TestDigest(t *testing.T) {
	httpClient := client(t)

	t.Run("posts a digest issue", func(t *testing.T) {
		d, err := Run(context.Background(), httpClient, "ghp_test123", "fernwood-labs", "trellis")
		if err != nil {
			t.Fatal(err)
		}
		if d.PostedNumber == 0 {
			t.Errorf("posted issue has no number: %+v", d)
		}
		if !strings.Contains(d.Body, "## Open pull requests") {
			t.Errorf("digest body: %q", d.Body)
		}
		t.Logf("%s: %d issues, %d pull requests, posted #%d", d.Repo, d.OpenIssues, d.OpenPulls, d.PostedNumber)
	})

	t.Run("surfaces GitHub's rate limit", func(t *testing.T) {
		limited := *httpClient
		base := limited.Transport
		limited.Transport = roundTripper(func(r *http.Request) (*http.Response, error) {
			r = r.Clone(r.Context())
			r.Header.Set("X-Mockingbird-Scenario", "rate-limited")
			return base.RoundTrip(r)
		})
		_, err := Run(context.Background(), &limited, "ghp_test123", "fernwood-labs", "trellis")
		var ghErr *github.ErrorResponse
		if !errors.As(err, &ghErr) || ghErr.Response.StatusCode != http.StatusForbidden {
			t.Fatalf("want a 403 ErrorResponse, got %v", err)
		}
		if !strings.Contains(ghErr.Message, "rate limit") {
			t.Errorf("message: %q", ghErr.Message)
		}
	})
}

type roundTripper func(*http.Request) (*http.Response, error)

func (f roundTripper) RoundTrip(r *http.Request) (*http.Response, error) { return f(r) }
