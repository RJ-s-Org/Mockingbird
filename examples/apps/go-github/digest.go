// Package digest builds a weekly triage digest of a GitHub repository with
// go-github: open issues and pull requests, posted back as a new issue.
//
// It knows nothing about Mockingbird; the tests reach https://api.github.com
// through `mockingbird proxy`.
package digest

import (
	"context"
	"fmt"
	"net/http"
	"strings"

	"github.com/google/go-github/v66/github"
)

// Digest is what one run found and posted.
type Digest struct {
	Repo         string
	OpenIssues   int
	OpenPulls    int
	Body         string
	PostedNumber int
	PostedURL    string
}

// Run collects open issues and pull requests of owner/repo and files the
// digest as an issue. httpClient may be nil (http.DefaultClient).
func Run(ctx context.Context, httpClient *http.Client, token, owner, repo string) (*Digest, error) {
	gh := github.NewClient(httpClient).WithAuthToken(token)

	issues, _, err := gh.Issues.ListByRepo(ctx, owner, repo, &github.IssueListByRepoOptions{
		State:       "open",
		ListOptions: github.ListOptions{PerPage: 50},
	})
	if err != nil {
		return nil, fmt.Errorf("listing issues: %w", err)
	}
	pulls, _, err := gh.PullRequests.List(ctx, owner, repo, &github.PullRequestListOptions{
		State:       "open",
		ListOptions: github.ListOptions{PerPage: 50},
	})
	if err != nil {
		return nil, fmt.Errorf("listing pull requests: %w", err)
	}

	d := &Digest{Repo: owner + "/" + repo, OpenPulls: len(pulls)}
	var b strings.Builder
	fmt.Fprintf(&b, "## Open issues\n\n")
	for _, issue := range issues {
		// The issues endpoint lists pull requests too.
		if issue.IsPullRequest() {
			continue
		}
		d.OpenIssues++
		fmt.Fprintf(&b, "- #%d %s\n", issue.GetNumber(), issue.GetTitle())
	}
	fmt.Fprintf(&b, "\n## Open pull requests\n\n")
	for _, pr := range pulls {
		fmt.Fprintf(&b, "- #%d %s (@%s)\n", pr.GetNumber(), pr.GetTitle(), pr.GetUser().GetLogin())
	}
	d.Body = b.String()

	title := fmt.Sprintf("Triage digest: %d issues, %d pull requests", d.OpenIssues, d.OpenPulls)
	posted, _, err := gh.Issues.Create(ctx, owner, repo, &github.IssueRequest{
		Title:  &title,
		Body:   &d.Body,
		Labels: &[]string{"triage"},
	})
	if err != nil {
		return nil, fmt.Errorf("posting the digest: %w", err)
	}
	d.PostedNumber = posted.GetNumber()
	d.PostedURL = posted.GetHTMLURL()
	return d, nil
}
