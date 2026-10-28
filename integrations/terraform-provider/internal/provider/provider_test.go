package provider

import (
	"context"
	"fmt"
	"net"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/hashicorp/terraform-plugin-framework/providerserver"
	"github.com/hashicorp/terraform-plugin-go/tfprotov6"
	"github.com/hashicorp/terraform-plugin-testing/helper/acctest"

	"github.com/RJ-s-Org/terraform-provider-mockingbird/internal/client"
)

// testAccProviders serves the provider in-process to Terraform or OpenTofu.
var testAccProviders = map[string]func() (tfprotov6.ProviderServer, error){
	"mockingbird": providerserver.NewProtocol6WithError(New("test")()),
}

// testClient talks to the server the acceptance tests run against.
var testClient *client.Client

// TestMain starts `mockingbird serve --offline` on free ports for the
// acceptance tests (TF_ACC=1) unless MB_ADMIN_URL points at a running
// server. The binary is MOCKINGBIRD_BIN, else the workspace's debug build.
func TestMain(m *testing.M) {
	os.Exit(run(m))
}

func run(m *testing.M) int {
	if os.Getenv("TF_ACC") == "" || os.Getenv("MB_ADMIN_URL") != "" {
		testClient = client.New(firstNonEmpty(os.Getenv("MB_ADMIN_URL"), DefaultEndpoint), os.Getenv("MB_ADMIN_TOKEN"), "acc-test")
		return m.Run()
	}
	stop, err := startServer()
	if err != nil {
		fmt.Fprintln(os.Stderr, "starting mockingbird:", err)
		return 1
	}
	defer stop()
	return m.Run()
}

func freePort() (int, error) {
	l, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		return 0, err
	}
	defer l.Close()
	return l.Addr().(*net.TCPAddr).Port, nil
}

func startServer() (func(), error) {
	bin := os.Getenv("MOCKINGBIRD_BIN")
	if bin == "" {
		bin = filepath.Join("..", "..", "..", "..", "target", "debug", "mockingbird")
	}
	if _, err := os.Stat(bin); err != nil {
		return nil, fmt.Errorf("no mockingbird binary at %s (build it with `cargo build -p mockingbird` or set MOCKINGBIRD_BIN): %w", bin, err)
	}
	dir, err := os.MkdirTemp("", "mb-acc-")
	if err != nil {
		return nil, err
	}
	cfg := filepath.Join(dir, "mockingbird.yaml")
	if err := os.WriteFile(cfg, []byte("{}\n"), 0o600); err != nil {
		return nil, err
	}
	var ports [3]int
	for i := range ports {
		if ports[i], err = freePort(); err != nil {
			return nil, err
		}
	}
	logf, err := os.Create(filepath.Join(dir, "server.log"))
	if err != nil {
		return nil, err
	}
	cmd := exec.Command(bin, "serve", "--offline", "-c", cfg,
		"--storage-url", "sqlite::memory:",
		"--port", fmt.Sprint(ports[0]), "--admin-port", fmt.Sprint(ports[1]), "--grpc-port", fmt.Sprint(ports[2]))
	cmd.Stdout, cmd.Stderr = logf, logf
	// The offline server never resolves it, but profiles may reference it.
	cmd.Env = append(os.Environ(), "MB_LLM_API_KEY=")
	if err := cmd.Start(); err != nil {
		return nil, err
	}
	stop := func() {
		_ = cmd.Process.Kill()
		_ = cmd.Wait()
		logf.Close()
		if os.Getenv("MB_ACC_KEEP_LOGS") == "" {
			os.RemoveAll(dir)
		} else {
			fmt.Fprintln(os.Stderr, "server log:", logf.Name())
		}
	}
	admin := fmt.Sprintf("http://127.0.0.1:%d", ports[1])
	deadline := time.Now().Add(30 * time.Second)
	for {
		resp, err := http.Get(admin + "/healthz")
		if err == nil {
			resp.Body.Close()
			if resp.StatusCode == http.StatusOK {
				break
			}
		}
		if time.Now().After(deadline) {
			stop()
			return nil, fmt.Errorf("server did not become healthy (log in %s)", logf.Name())
		}
		time.Sleep(100 * time.Millisecond)
	}
	os.Setenv("MB_ADMIN_URL", admin)
	os.Setenv("MB_DATA_PLANE_URL", fmt.Sprintf("http://127.0.0.1:%d", ports[0]))
	testClient = client.New(admin, "", "acc-test")
	return stop, nil
}

func testAccPreCheck(t *testing.T) {
	t.Helper()
	if os.Getenv("MB_ADMIN_URL") == "" {
		t.Fatal("MB_ADMIN_URL must be set for acceptance tests")
	}
}

// randName is a unique lowercase resource name.
func randName(prefix string) string {
	return prefix + "-" + strings.ToLower(acctest.RandString(8))
}

// testdata is the absolute path of a file in testdata/.
func testdata(t *testing.T, name string) string {
	t.Helper()
	p, err := filepath.Abs(filepath.Join("testdata", name))
	if err != nil {
		t.Fatal(err)
	}
	return p
}

// apiDo calls the Admin API from a test step.
func apiDo(t *testing.T, method, path string, body, out any) {
	t.Helper()
	if _, err := testClient.Do(context.Background(), method, path, body, out); err != nil {
		t.Fatalf("%s %s: %v", method, path, err)
	}
}
