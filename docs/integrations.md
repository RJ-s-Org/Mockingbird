# Integrations

Mockingbird runs wherever your tests and environments run. Each integration has its own guide:

| Integration | What it gives you | Guide |
| --- | --- | --- |
| GitHub Action, GitLab CI | Start mocks in a job, replay recorded snapshots without an LLM key, fail the build on contract drift (`mockingbird report`, `mockingbird verify`) | [CI](ci.md), [`integrations/github-action`](../integrations/github-action/README.md) |
| Terraform / OpenTofu, Pulumi | Mocks, contracts, scenarios and LLM profiles as code, next to the rest of your infrastructure | [Infrastructure as code](iac.md) |
| Helm, Argo CD, Flux | The chart, and GitOps recipes that keep mocks in sync with a repository | [Kubernetes](deploy/kubernetes.md), [GitOps](deploy/gitops.md) |
| Testcontainers | A Mockingbird container per test suite | [`integrations/testcontainers`](../integrations/testcontainers/README.md), [GitOps](deploy/gitops.md#4-throwaway-mocks-for-a-test-run) |
| Coding agents | MCP server, skills and plugins for Claude Code, Codex, Cursor, Gemini CLI, Kiro, VS Code, Windsurf and ChatGPT | [Coding agents](agents.md) |
| Examples | About twenty real APIs, ready to serve | [Examples catalog](examples.md), [`examples/apis`](https://github.com/RJ-s-Org/Mockingbird/tree/main/examples/apis), [gallery](../examples/gallery/README.md) |
