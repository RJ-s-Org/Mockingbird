# Coding agents

Every Mockingbird capability is available to coding agents: installing it, connecting an LLM, running a server, and the whole Admin API. Agents reach it three ways, all from the same binary:

- **MCP server**: `mockingbird mcp` exposes 53 tools (stdio, or streamable HTTP for remote agents such as ChatGPT).
- **Skills**: six `SKILL.md` guides in [`plugins/mockingbird/skills`](../plugins/mockingbird/skills) tell an agent when and how to use them.
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

The scripts install the release binary for the platform into `~/.mockingbird/bin` after checking its SHA-256 (`MOCKINGBIRD_VERSION` pins a release, `MOCKINGBIRD_INSTALL_DIR` changes the directory; with `install.sh`, `MOCKINGBIRD_VERSION=main` builds the main branch with cargo), and build from source with `cargo install` when no release binary fits.

The skills come from `main` and may be newer than a release binary. `doctor` reports the binary's `skills_level`, and the `mockingbird` skill names the level it was written for; when the binary's is lower or missing, the skill tells the agent to install a binary built from `main`.

`mockingbird agent install <agent>` merges a `mockingbird` server entry into the agent's MCP config, leaving other entries alone, and writes the skills where that agent reads them (`.claude/skills`, `.agents/skills`, `.cursor/skills`, `.gemini/skills`, `.github/skills`, or Kiro steering files). The entry runs this binary by its absolute path. Options: `--scope user` (home directory instead of the project), `--no-skills`, `--no-mcp`, `--dry-run`, `--command <path>`, `--config <file>` and `--pass-env <NAME>`. `mockingbird agent skills <dir>` writes just the skills.

### Key passthrough

Desktop agents and IDEs often start MCP servers without the user's shell environment. `agent install` therefore adds the **names** of the variables the default LLM profile of `--config` (default `./mockingbird.yaml`) references (`api_key: env:NAME`, `${env:NAME}` in headers), plus any `--pass-env NAME`, to the entry in each agent's own interpolation syntax, so the agent hands its value to the server. Values are never written. The output lists the names under `passthrough`.

| Agent | What `agent install` writes |
| --- | --- |
| Claude Code (project `.mcp.json`) | `"env": {"NAME": "${NAME:-}"}` (empty rather than the literal text when Claude Code lacks the variable; the MCP server then takes it from the login shell) |
| Claude Code (`--scope user`, a printed `claude mcp add`) | nothing (a shell would expand the value into `~/.claude.json`); `passthrough_unsupported` suggests `--scope project` or `mockingbird keys setup` |
| Codex | `env_vars = ["NAME"]` in `[mcp_servers.mockingbird]` |
| Cursor, VS Code, Windsurf | `"env": {"NAME": "${env:NAME}"}` |
| Gemini CLI | `"env": {"NAME": "${NAME}"}` |
| Kiro | `"env": {"NAME": "${NAME}"}`; Kiro expands only names approved in its settings (Mcp Approved Env Vars), which the output's next steps list |
| Claude Desktop | nothing: it cannot reference variables, and the output's `passthrough_unsupported` points to the key setup page |

An existing Codex `[mcp_servers.mockingbird]` table is left as it is.

### Key setup page

When the agent cannot see the key at all, `mockingbird keys setup [name]` (MCP `open_key_setup`) opens a one-shot page on `127.0.0.1:<random port>` whose URL carries a random token. The user types the key there; Mockingbird writes it to `~/.mockingbird/secrets/<name>` (file `0600`, directory `0700`; an owner-only ACL on Windows) and sets the profile's `api_key` to `file:<that path>` (`-c`, `--profile`; default the default profile, and its name as the key name). The page accepts one submission from its own origin, refuses other `Host` and `Origin` headers, and closes after 10 minutes (`--timeout`). The MCP tool opens the browser and waits up to 45 s (under agents' tool-call timeouts) for the key, then returns `{"saved": name, "profile", "config"}`, never the key; when the user is slower, with `open: false`, or when no browser opens, it returns the URL instead and the page stays open. It updates the config `start_server` used unless given `config`. `doctor` lists the stored keys under `key_store` (names and paths, never values), and a `file:` reference into the store is refused when the file is readable by group or others.

## What an agent can do

Ask in plain words: "mock the API in openapi.yaml", "use OpenRouter for Mockingbird", "make the orders mock return 503s for 10% of calls", "self-host this for the team". The skills route the agent to these tools:

| Area | Tools | CLI |
| --- | --- | --- |
| Setup | `doctor`, `list_examples`, `init_example`, `list_llm_providers`, `init_config`, `configure_llm`, `test_llm`, `validate_contract` | `doctor`, `examples list`, `examples init`, `llm providers`, `init`, `llm configure`, `llm test`, `validate` |
| Server | `start_server` (`proxy: true` also starts the HTTP(S) proxy), `server_status`, `server_logs`, `stop_server`, `proxy_env`, `project_hosts` ([hosts the project can call](ci.md#hosts-the-project-can-call)), `verify_run` ([the app's tests against the mocks](ci.md#run-the-apps-tests-verify---run)) | `server start/status/logs/stop`, `proxy env`, `run -- <command>`, `hosts`, `verify --run "<command>"` |
| Mocks | `list_mocks`, `get_mock`, `create_mock`, `update_mock`, `delete_mock`, `import_contract`, `list_contract_versions`, `activate_contract_version` | `mocks list/get/create/update/delete`, `import`, `contracts list/activate` |
| Drafts | `get_draft`, `edit_draft`, `approve_draft` | `import --approve`, `drafts approve` |
| Contract fixes | `edit_contract` (merge-patch the active OpenAPI contract, activated as a new version) | `import <edited file> --as <mock> --activate` |
| World descriptions ([guide](world.md); the `mockingbird-world` skill) | `get_world`, `get_world_schema`, `put_world`, `patch_world`, `list_world_revisions`, `get_world_revision`, `restore_world_revision`, `diff_world` | `world show/put/patch/revisions/revision/restore/diff`, `world schema` |
| Behaviour | `list_scenarios`, `put_scenario`, `delete_scenario`, `list_llm_profiles`, `put_llm_profile`, `delete_llm_profile` | `scenarios …`, `profiles …` |
| Traffic and state | `call_mock`, `list_requests` (summaries; `include_bodies` for bodies), `get_request`, `list_sessions`, `export_session`, `seed_session`, `reset_session` | `sessions …`, `export-fixtures`, `seed-fixtures` |
| Events | `list_channels`, `publish_event` | `channels …` |
| Everything else | `admin_api` (any `/api/v1` request: workspaces, API keys, audit, imports) | `api <METHOD> <PATH>` |

Tools carry MCP annotations (`readOnlyHint`, `destructiveHint`), so agents can ask before destructive calls. The server also offers each skill as a resource (`mockingbird://skills/<name>`), the provider catalogue as `mockingbird://providers`, and five prompts (`setup`, `mock-api`, `self-host`, `testing`, `world`).

The MCP server talks to the Admin API at `--admin-url` / `MB_ADMIN_URL` in workspace `--workspace` / `MB_WORKSPACE`, sending `--token` / `MB_ADMIN_TOKEN` when set, so it can drive a remote Mockingbird too.

Without `--admin-url` / `MB_ADMIN_URL`, a stdio MCP server (the one an agent plugin starts in a project) acts on that project's server only. On each call it uses the background server that `start_server` (or `mockingbird server start` in the project) recorded under the project's `.mockingbird/`, else the admin listener of `./mockingbird.yaml` (read again on every call, so a config written after the agent started counts), else `http://127.0.0.1:9090`. Before a tool talks to a server it did not record, it reads that server's `/readyz`, which reports the `config` file and working `dir` the server was started with. The server counts as the project's when that config lies inside the folder the MCP server started in, or is the config `start_server` used; a server started without a config counts when its folder lies inside the project. When the MCP server starts in the filesystem root or the home folder, only a config (or folder) right there counts, not every project below it. Any other server, such as another project's on the default port 9090, is refused: the tool sends nothing and its error names that server's config and how to proceed. Either start this project's server on free ports with `start_server` (`port`, `admin_port`, `grpc_port`, or `server.*` in its config), or, to use that server on purpose, set `MB_ADMIN_URL` to it. A server that does not report its origin (one older than this release) is refused the same way. When a port `start_server` needs is held by a Mockingbird server, its error says whose server holds it. `doctor` lists a refused server under `problems`.

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

Over MCP, `put_llm_profile` saves a profile into a running server in one call in the config file's shape: `{"name": "openrouter", "profile": {"provider": "openrouter", "model": "deepseek/deepseek-v4.1-flash-1017", "api_key": "env:OPENROUTER_API_KEY"}}`. `provider` fills what the profile leaves out from the preset (`base_url`, the default model, `output_mode`, headers and the key's reference; `account_id`, `gateway_id` and `resource` fill a preset's base URL; `no_gateway_token` drops Cloudflare AI Gateway's token header), and `api_key` is an alias of `secret_ref`. Either holds a reference such as `env:OPENROUTER_API_KEY`, never the key: a literal key is refused.

Profiles can carry extra provider headers (`llm.profiles.<name>.headers`, see the [configuration reference](configuration.md#llm)); secrets in them are `${env:NAME}` references, resolved when the mock is built. Keys never pass through the agent: the output lists the variables the user must export.

Agents usually start `mockingbird mcp` from a desktop app or an IDE whose environment lacks what the shell profile exports. So at startup the MCP server runs the login shell (`$SHELL -l -i -c env`, for at most 15 s; a shell that takes longer imports nothing and the MCP server writes a warning to stderr) and takes the variables it lacks; `doctor` lists their names under `shell_env.imported`, and its problems say when the MCP server process still cannot see a key. Set `MB_NO_SHELL_ENV=1` to skip the import. `start_server` refuses a config whose default profile's key is missing, since every response would be faker output; pass `offline: true` to start anyway. `mockingbird init` takes `--port`, `--admin-port` and `--grpc-port` (written under `server:`) so several projects can run side by side, and `mockingbird llm test` with no flags tests the default profile of `./mockingbird.yaml`.

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
- hides the tools that act on the host (`doctor`, `init_config`, `init_example`, `configure_llm`, `test_llm`, `validate_contract`, the `server` tools) and refuses local paths in `import_contract` (send the text in `content` instead), unless `--allow-local-tools`;
- with `--read-only`, exposes only tools that change nothing.

## Security notes

- The MCP server has the Admin API rights of its token. Give remote agents an API key with the least role they need (`viewer` for `--read-only` use).
- API keys never pass through the agent: profiles hold `env:` or `file:` references, `agent install` writes variable names only, and the key setup page sends nothing back.
- `call_mock` returns the answer's status, `x-mockingbird-*` headers and body, and `expr_failed` when a [world expression](world.md#expressions) failed in the call. It reads that from the call's request record, which it finds by an `x-mockingbird-call` header it sends (so the header shows in the request log and in `request.headers`; a `redact_fields` entry matching it hides the record from `call_mock`). When the record is not among the session's 20 newest within about a second, the answer says `expr_failed_unknown: true`. `call_mock` and `import_contract` with a URL make requests from the machine the MCP server runs on; URL imports go through the server's [URL fetching](import.md#url-fetching) guards.
- Over the Admin API, an LLM profile's secret references must match `server.api_secret_refs`, and only a sysadmin may point them at a new `base_url`; this applies to `${…}` references in headers too.
