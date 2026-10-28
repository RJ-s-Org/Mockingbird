// Package mockingbird is a Testcontainers module for Mockingbird: it starts
// the image with a contract (zero-config) or a mockingbird.yaml and its
// contracts, waits for /readyz and exposes the mocks' base URL and the
// request log.
package mockingbird

import (
	"context"
	"encoding/json"
	"fmt"
	"net/http"
	"os"
	"path/filepath"

	"github.com/docker/go-connections/nat"
	"github.com/testcontainers/testcontainers-go"
	"github.com/testcontainers/testcontainers-go/wait"
)

const (
	httpPort  = "8080/tcp"
	adminPort = "9090/tcp"
)

// DefaultImage is the image Run starts unless MOCKINGBIRD_IMAGE is set.
func DefaultImage() string {
	if image := os.Getenv("MOCKINGBIRD_IMAGE"); image != "" {
		return image
	}
	return "ghcr.io/rj-s-org/mockingbird:0.1.0"
}

// Container is a started Mockingbird.
type Container struct {
	testcontainers.Container
}

// Option configures Run.
type Option func(*settings) error

type settings struct {
	files   []testcontainers.ContainerFile
	serve   []string
	env     map[string]string
	offline bool
}

// WithContract serves one contract zero-config: the mock is named after the
// file and mounted at `/`.
func WithContract(file string) Option {
	return func(s *settings) error {
		target := "/contracts/" + filepath.Base(file)
		s.files = append(s.files, testcontainers.ContainerFile{HostFilePath: file, ContainerFilePath: target, FileMode: 0o644})
		s.serve = []string{target}
		return nil
	}
}

// WithConfigDir serves a directory holding mockingbird.yaml and the contracts
// it names; mocks are at `/<name>`.
func WithConfigDir(dir string) Option {
	return func(s *settings) error {
		entries, err := os.ReadDir(dir)
		if err != nil {
			return err
		}
		for _, e := range entries {
			if e.Type().IsRegular() {
				s.files = append(s.files, testcontainers.ContainerFile{
					HostFilePath:      filepath.Join(dir, e.Name()),
					ContainerFilePath: "/etc/mockingbird/" + e.Name(),
					FileMode:          0o644,
				})
			}
		}
		s.serve = []string{"-c", "/etc/mockingbird/mockingbird.yaml"}
		return nil
	}
}

// WithLLM lets the mocks call an LLM. Pass the key from the environment
// (os.Getenv("MB_LLM_API_KEY")), never a literal.
func WithLLM(apiKey, baseURL, model string) Option {
	return func(s *settings) error {
		s.offline = false
		for name, value := range map[string]string{"MB_LLM_API_KEY": apiKey, "MB_LLM_BASE_URL": baseURL, "MB_LLM_MODEL": model} {
			if value != "" {
				s.env[name] = value
			}
		}
		return nil
	}
}

// Run starts Mockingbird and waits until /readyz answers 200.
func Run(ctx context.Context, image string, opts ...Option) (*Container, error) {
	s := settings{env: map[string]string{}, offline: true}
	for _, opt := range opts {
		if err := opt(&s); err != nil {
			return nil, err
		}
	}
	cmd := append([]string{"serve"}, s.serve...)
	cmd = append(cmd, "--host", "0.0.0.0", "--insecure-no-auth")
	if s.offline {
		cmd = append(cmd, "--offline")
	}
	c, err := testcontainers.GenericContainer(ctx, testcontainers.GenericContainerRequest{
		ContainerRequest: testcontainers.ContainerRequest{
			Image:        image,
			Cmd:          cmd,
			Env:          s.env,
			Files:        s.files,
			ExposedPorts: []string{httpPort, adminPort},
			WaitingFor:   wait.ForHTTP("/readyz").WithPort(adminPort).WithStatusCodeMatcher(func(code int) bool { return code == http.StatusOK }),
		},
		Started: true,
	})
	var container *Container
	if c != nil {
		container = &Container{Container: c}
	}
	return container, err
}

func (c *Container) url(ctx context.Context, port string) (string, error) {
	host, err := c.Host(ctx)
	if err != nil {
		return "", err
	}
	mapped, err := c.MappedPort(ctx, nat.Port(port))
	if err != nil {
		return "", err
	}
	return fmt.Sprintf("http://%s:%s", host, mapped.Port()), nil
}

// BaseURL is the data plane's URL, or a mock's (`/<mock>`) when mock is set.
func (c *Container) BaseURL(ctx context.Context, mock string) (string, error) {
	url, err := c.url(ctx, httpPort)
	if err != nil || mock == "" {
		return url, err
	}
	return url + "/" + mock, nil
}

// AdminURL is the Admin API and ops endpoints (/readyz, /api/v1/...).
func (c *Container) AdminURL(ctx context.Context) (string, error) {
	return c.url(ctx, adminPort)
}

// Requests returns the calls a mock received, newest first, with their bodies.
func (c *Container) Requests(ctx context.Context, mock string) ([]map[string]any, error) {
	admin, err := c.AdminURL(ctx)
	if err != nil {
		return nil, err
	}
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, admin+"/api/v1/mocks/"+mock+"/requests", nil)
	if err != nil {
		return nil, err
	}
	res, err := http.DefaultClient.Do(req)
	if err != nil {
		return nil, err
	}
	defer res.Body.Close()
	if res.StatusCode != http.StatusOK {
		return nil, fmt.Errorf("requests of %s: %s", mock, res.Status)
	}
	var log []map[string]any
	return log, json.NewDecoder(res.Body).Decode(&log)
}
