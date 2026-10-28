// Package client is a small Mockingbird Admin API client: JSON in and out,
// a bearer token, and RFC 9457 problem documents turned into errors.
package client

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"strings"
	"time"
)

// Client talks to one Admin API.
type Client struct {
	// BaseURL is the admin origin, e.g. http://127.0.0.1:9090.
	BaseURL string
	// Token is sent as a bearer token when set.
	Token string
	// UserAgent identifies the caller.
	UserAgent string
	HTTP      *http.Client
}

// New returns a client for the admin origin baseURL.
func New(baseURL, token, userAgent string) *Client {
	return &Client{
		BaseURL:   strings.TrimRight(baseURL, "/"),
		Token:     token,
		UserAgent: userAgent,
		HTTP:      &http.Client{Timeout: 2 * time.Minute},
	}
}

// Problem is an RFC 9457 problem document returned by the Admin API.
type Problem struct {
	Status int    `json:"status"`
	Title  string `json:"title"`
	Detail string `json:"detail"`
	Type   string `json:"type"`
}

func (p *Problem) Error() string {
	switch {
	case p.Detail != "" && p.Title != "":
		return fmt.Sprintf("%d %s: %s", p.Status, p.Title, p.Detail)
	case p.Title != "":
		return fmt.Sprintf("%d %s", p.Status, p.Title)
	default:
		return fmt.Sprintf("HTTP %d", p.Status)
	}
}

// IsNotFound reports whether err is a 404 problem.
func IsNotFound(err error) bool {
	var p *Problem
	return errors.As(err, &p) && p.Status == http.StatusNotFound
}

// Seg escapes one path segment.
func Seg(s string) string { return url.PathEscape(s) }

// WsPath is the path of a workspace resource: /api/v1/workspaces/{ws}{rest}.
func WsPath(ws, rest string) string {
	return "/api/v1/workspaces/" + Seg(ws) + rest
}

// Do sends a JSON request (body may be nil) and decodes a JSON response into
// out (which may be nil). It returns the status code.
func (c *Client) Do(ctx context.Context, method, path string, body, out any) (int, error) {
	var rd io.Reader
	if body != nil {
		b, err := json.Marshal(body)
		if err != nil {
			return 0, fmt.Errorf("encoding request: %w", err)
		}
		rd = bytes.NewReader(b)
	} else if method != http.MethodGet && method != http.MethodDelete {
		// Every mutating request needs a JSON content type, even an empty one.
		rd = strings.NewReader("{}")
	}
	return c.raw(ctx, method, path, "application/json", rd, out)
}

// Upload POSTs raw bytes with the given content type.
func (c *Client) Upload(ctx context.Context, path, contentType string, data []byte, out any) (int, error) {
	return c.raw(ctx, http.MethodPost, path, contentType, bytes.NewReader(data), out)
}

func (c *Client) raw(ctx context.Context, method, path, contentType string, body io.Reader, out any) (int, error) {
	req, err := http.NewRequestWithContext(ctx, method, c.BaseURL+path, body)
	if err != nil {
		return 0, err
	}
	if body != nil {
		req.Header.Set("Content-Type", contentType)
	}
	req.Header.Set("Accept", "application/json")
	if c.Token != "" {
		req.Header.Set("Authorization", "Bearer "+c.Token)
	}
	if c.UserAgent != "" {
		req.Header.Set("User-Agent", c.UserAgent)
	}
	resp, err := c.HTTP.Do(req)
	if err != nil {
		return 0, fmt.Errorf("%s %s: %w", method, path, err)
	}
	defer resp.Body.Close()
	data, err := io.ReadAll(io.LimitReader(resp.Body, 64<<20))
	if err != nil {
		return resp.StatusCode, fmt.Errorf("%s %s: reading response: %w", method, path, err)
	}
	if resp.StatusCode >= 300 {
		p := &Problem{Status: resp.StatusCode}
		if json.Unmarshal(data, p) != nil || p.Status == 0 {
			p.Status = resp.StatusCode
			p.Detail = strings.TrimSpace(string(data))
		}
		return resp.StatusCode, p
	}
	if out != nil && len(data) > 0 {
		if err := json.Unmarshal(data, out); err != nil {
			return resp.StatusCode, fmt.Errorf("%s %s: decoding response: %w", method, path, err)
		}
	}
	return resp.StatusCode, nil
}
