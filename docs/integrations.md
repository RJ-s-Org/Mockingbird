# Integrations

Mockingbird runs wherever your tests and environments run. Each integration has its own guide:

| Integration | What it gives you | Guide |
| --- | --- | --- |
| GitHub Action, GitLab CI | Start mocks in a job, replay recorded snapshots without an LLM key, fail the build on contract drift (`mockingbird report`, `mockingbird verify`) | [`docs/ci.md`](https://github.com/RJ-s-Org/Mockingbird/blob/main/docs/ci.md), [`integrations/github-action`](https://github.com/RJ-s-Org/Mockingbird/tree/main/integrations/github-action) |
| Terraform / OpenTofu, Pulumi | Mocks, contracts, scenarios and LLM profiles as code, next to the rest of your infrastructure | [Infrastructure as code](iac.md) |
| Helm, Argo CD, Flux | The chart, and GitOps recipes that keep mocks in sync with a repository | [Kubernetes](deploy/kubernetes.md), [`docs/deploy/gitops.md`](https://github.com/RJ-s-Org/Mockingbird/blob/main/docs/deploy/gitops.md) |
| Testcontainers | A Mockingbird container per test suite | [`docs/deploy/gitops.md`](https://github.com/RJ-s-Org/Mockingbird/blob/main/docs/deploy/gitops.md#testcontainers) |
| Coding agents | MCP server, skills and plugins for Claude Code, Codex, Cursor, Gemini CLI, Kiro, VS Code, Windsurf and ChatGPT | [Coding agents](agents.md) |
| Examples | About twenty real APIs, ready to serve | [`docs/examples.md`](https://github.com/RJ-s-Org/Mockingbird/blob/main/docs/examples.md), [`examples/apis`](https://github.com/RJ-s-Org/Mockingbird/tree/main/examples/apis), [gallery](../examples/gallery/README.md) |
