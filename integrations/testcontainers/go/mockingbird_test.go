package mockingbird_test

import (
	"context"
	"encoding/json"
	"net/http"
	"strings"
	"testing"

	"github.com/testcontainers/testcontainers-go"

	mockingbird "github.com/rj-s-org/mockingbird/integrations/testcontainers/go"
)

func TestPayments(t *testing.T) {
	ctx := context.Background()
	mocks, err := mockingbird.Run(ctx, mockingbird.DefaultImage(), mockingbird.WithConfigDir("../fixtures"))
	testcontainers.CleanupContainer(t, mocks)
	if err != nil {
		t.Fatal(err)
	}
	base, err := mocks.BaseURL(ctx, "payments")
	if err != nil {
		t.Fatal(err)
	}

	charge := func(scenario string) *http.Response {
		req, _ := http.NewRequest(http.MethodPost, base+"/v1/charges", strings.NewReader(`{"amount":1999,"currency":"eur"}`))
		req.Header.Set("Content-Type", "application/json")
		if scenario != "" {
			req.Header.Set("X-Mockingbird-Scenario", scenario)
		}
		res, err := http.DefaultClient.Do(req)
		if err != nil {
			t.Fatal(err)
		}
		return res
	}

	res := charge("")
	defer res.Body.Close()
	if res.StatusCode != http.StatusCreated {
		t.Fatalf("create charge: %s", res.Status)
	}
	var body struct{ ID string }
	if err := json.NewDecoder(res.Body).Decode(&body); err != nil || body.ID == "" {
		t.Fatalf("charge without id: %v", err)
	}

	declined := charge("card-declined")
	declined.Body.Close()
	if declined.StatusCode != http.StatusPaymentRequired {
		t.Fatalf("card-declined: %s", declined.Status)
	}

	log, err := mocks.Requests(ctx, "payments")
	if err != nil {
		t.Fatal(err)
	}
	if len(log) < 2 {
		t.Fatalf("request log has %d entries, want 2", len(log))
	}
}
