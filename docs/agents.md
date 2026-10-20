# Coding agents

Every Mockingbird capability is available to coding agents: installing it, connecting an LLM, running a server, and the whole Admin API. Agents reach it three ways, all from the same binary:

- **MCP server**: `mockingbird mcp` exposes 33 tools (stdio, or streamable HTTP for remote agents such as ChatGPT).
- **Skills**: five `SKILL.md` guides in [`plugins/mockingbird/skills`](../plugins/mockingbird/skills) tell an agent when and how to use them.
- **CLI**: every onboarding and lifecycle command prints JSON (`init`, `llm`, `server`, `doctor`, `agent`), and the Admin API client commands already do.

## Install

| Agent | Install |
| --- | --- |
| Claude Code | `/plugin marketplace add rj-s-org/mockingbird`, then `/plugin install mockingbird@mockingbird`. The plugin installs the binary on first use. |
| Codex (CLI and app) | `codex plugin marketplace add rj-s-org/mockingbird`, then install **Mockingbird** from the plugin browser. Or: `mockingbird agent install codex`. |
| Cursor | `mockingbird agent install cursor` (project) or `--scope user`. The repository is also a Cursor plugin marketplace (`.cursor-plugin/marketplace.json`). |
| Gemini CLI | `gemini extensions install https://github.com/rj-s-org/mockingbird`, or `mockingbird agent install gemini`. |
| Kiro | `mockingbird agent install kiro` (MCP entry in `.kiro/settings/mcp.json`, guides as steering files), or add the `powers/mockingbird` directory of a clone as a custom power in the Powers panel. |
| VS Code (Copilot agent mode) | `mockingbird agent install vscode` (writes `.vscode/mcp.json` and `.github/skills`). |
| Windsurf | `mockingbird agent install windsurf`. |
| Claude Desktop | `mockingbird agent install claude-desktop`. |
| ChatGPT, hosted agents | Run the MCP server over HTTP; see [Remote agents](#remote-agents-chatgpt). |
| Anything else that speaks MCP | Command `mockingbird`, arguments `["mcp"]` (stdio). `mockingbird agent print <agent>` prints the entry. |

Except for the Claude Code plugin, the agent needs the binary first:

```sh
curl -fsSL https://raw.githubusercontent.com/rj-s-org/mockingbird/main/install.sh | sh     # macOS, Linux
irm https://raw.githubusercontent.com/rj-s-org/mockingbird/main/install.ps1 | iex           # Windows
```

The scripts install the release binary for the platform into `~/.mockingbird/bin` after checking its SHA-256 (`MOCKINGBIRD_VERSION` pins a release, `MOCKINGBIRD_INSTALL_DIR` changes the directory), and build from source with `cargo install` when no release binary fits.

`mockingbird agent install <agent>` merges a `mockingbird` server entry into the agent's MCP config, leaving other entries alone, and writes the skills where that agent reads them (`.claude/skills`, `.agents/skills`, `.cursor/skills`, `.gemini/skills`, `.github/skills`, or Kiro steering files). The entry runs this binary by its absolute path. Options: `--scope user` (home directory instead of the project), `--no-skills`, `--no-mcp`, `--dry-run`, `--command <path>`. `mockingbird agent skills <dir>` writes just the skills.

## What an agent can do

Ask in plain words: "mock the API in openapi.yaml", "use OpenRouter for Mockingbird", "make the orders mock return 503s for 10% of calls", "self-host this for the team". The skills route the agent to these tools:

| Area | Tools | CLI |
| --- | --- | --- |
| Setup | `doctor`, `list_llm_providers`, `init_config`, `configure_llm`, `test_llm`, `validate_contract` | `doctor`, `llm providers`, `init`, `llm configure`, `llm test`, `validate` |
| Server | `start_server`, `server_status`, `server_logs`, `stop_server` | `server start/status/logs/stop` |
| Mocks | `list_mocks`, `get_mock`, `create_mock`, `update_mock`, `delete_mock`, `import_contract`, `list_contract_versions`, `activate_contract_version` | `mocks …`, `import` |
| Behaviour | `list_scenarios`, `put_scenario`, `delete_scenario`, `list_llm_profiles`, `put_llm_profile`, `delete_llm_profile` | `scenarios …`, `profiles …` |
| Traffic and state | `call_mock`, `list_requests`, `list_sessions`, `export_session`, `seed_session`, `reset_session` | `sessions …`, `export-fixtures`, `seed-fixtures` |
| Events | `list_channels`, `publish_event` | `channels …` |
| Everything else | `admin_api` (any `/api/v1` request: workspaces, API keys, audit, drafts, imports) | |

Tools carry MCP annotations (`readOnlyHint`, `destructiveHint`), so agents can ask before destructive calls. The server also offers each skill as a resource (`mockingbird://skills/<name>`), the provider catalogue as `mockingbird://providers`, and four prompts (`setup`, `mock-api`, `self-host`, `testing`).

The MCP server talks to the Admin API at `--admin-url` / `MB_ADMIN_URL` (default `http://127.0.0.1:9090`) in workspace `--workspace` / `MB_WORKSPACE`, sending `--token` / `MB_ADMIN_TOKEN` when set, so it can drive a remote Mockingbird too.

## LLM onboarding

`mockingbird llm providers` lists the presets. Each turns a few answers into an LLM profile whose key is a reference to an environment variable:

| Preset | Base URL | Key |
| --- | --- | --- |
| `openrouter` | `https://openrouter.ai/api/v1` | `OPENROUTER_API_KEY`; also sends OpenRouter's attribution headers |
| `google-ai-studio` | `https://generativelanguage.googleapis.com/v1beta/openai` | `GEMINI_API_KEY` |
| `cloudflare-ai-gateway` | `https://gateway.ai.cloudflare.com/v1/<account>/<gateway>/compat` | `cf-aig-authorization: Bearer ${env:CF_AIG_API_KEY}`; `--api-key-env` adds a provider key, `--no-gateway-token` drops the gateway token |
| `cloudflare-workers-ai` | `https://api.cloudflare.com/client/v4/accounts/<account>/ai/v1` | `CLOUDFLARE_API_KEY` |
| `openai`, `anthropic`, `deepseek`, `groq`, `mistral`, `together` | the vendor's OpenAI-compatible endpoint | `<VENDOR>_API_KEY` |
| `azure-openai` | `https://<resource>.openai.azure.com/openai/v1` | `api-key: ${env:AZURE_OPENAI_API_KEY}` |
| `ollama`, `lm-studio` | `http://127.0.0.1:11434/v1`, `:1234/v1` | none |
| `custom` | `--base-url` | `--api-key-env`, or any header via `--header 'name=…${env:NAME}…'` |

```sh
mockingbird init --provider openrouter --contract openapi.yaml           # new mockingbird.yaml
mockingbird llm configure --provider cloudflare-ai-gateway --account-id <id> --default
mockingbird llm configure --provider google-ai-studio --apply           # into a running server
mockingbird llm test -c mockingbird.yaml --profile openrouter            # ok, latency, hint
```

Profiles can carry extra provider headers (`llm.profiles.<name>.headers`, see the [configuration reference](configuration.md#llm)); secrets in them are `${env:NAME}` references, resolved when the mock is built. Keys never pass through the agent: the output lists the variables the user must export.

## Remote agents (ChatGPT)

Agents that cannot start a local process connect over streamable HTTP:

```sh
export MB_MCP_TOKEN=$(openssl rand -hex 32)
mockingbird mcp --transport http --listen 127.0.0.1:7331 --auth-token-env MB_MCP_TOKEN
cloudflared tunnel --url http://127.0.0.1:7331        # or any HTTPS reverse proxy
```

Then add `https://<host>/mcp` as a connector (in ChatGPT: Settings → Apps & Connectors → Advanced → Developer mode → Create). The HTTP transport:

- requires `--auth-token-env` (a token of at least 16 characters, checked as `Authorization: Bearer`) to listen on anything but loopback;
- refuses requests whose `Origin` is not listed with `--allow-origin`;
- hides the tools that act on the host (`doctor`, `init_config`, `configure_llm`, `test_llm`, `validate_contract`, the `server` tools) and refuses local paths in `import_contract` (send the text in `content` instead), unless `--allow-local-tools`;
- with `--read-only`, exposes only tools that change nothing.

## Security notes

- The MCP server has the Admin API rights of its token. Give remote agents an API key with the least role they need (`viewer` for `--read-only` use).
- `call_mock` and `import_contract` with a URL make requests from the machine the MCP server runs on; URL imports go through the server's [URL fetching](../README.md#url-fetching) guards.
- Over the Admin API, an LLM profile's secret references must match `server.api_secret_refs`, and only a sysadmin may point them at a new `base_url`; this applies to `${…}` references in headers too.
