---
name: mockingbird-setup
description: Install Mockingbird and connect it to an LLM provider - OpenRouter, Google AI Studio (Gemini), Cloudflare AI Gateway or Workers AI, OpenAI, Anthropic, DeepSeek, Groq, Mistral, Together, Azure OpenAI, Ollama, LM Studio, or any OpenAI-compatible endpoint with your own key. Use when Mockingbird is not installed, when choosing or changing the model that generates mock responses, or when responses come back as `X-Mockingbird-Source: fallback` because the LLM fails.
---

# Install Mockingbird and configure an LLM

## 1. Install

Check first: `mockingbird --version` (also try `~/.mockingbird/bin/mockingbird`).

| Platform | Command |
| --- | --- |
| macOS, Linux | `curl -fsSL https://raw.githubusercontent.com/rj-s-org/mockingbird/main/install.sh \| sh` |
| Windows (PowerShell) | `irm https://raw.githubusercontent.com/rj-s-org/mockingbird/main/install.ps1 \| iex` |
| Docker | `docker run --rm -p 8080:8080 -p 9090:9090 -v "$PWD:/work" -w /work ghcr.io/rj-s-org/mockingbird serve --host 0.0.0.0 --insecure-no-auth --offline api.yaml` |
| From source (Rust 1.97+) | `cargo install --locked --git https://github.com/rj-s-org/mockingbird mockingbird` |

The script installs the release binary for the platform into `~/.mockingbird/bin` (override with `MOCKINGBIRD_INSTALL_DIR`; pin with `MOCKINGBIRD_VERSION=v0.1.0`), verifies its SHA-256, and falls back to `cargo install` when no release binary fits. If `~/.mockingbird/bin` is not on `PATH`, call the binary by its full path.

Then set Mockingbird up in the agent you are running in, so its MCP tools and these skills are available next session:

```sh
mockingbird agent install claude-code   # or: codex, cursor, gemini, kiro, vscode, windsurf, claude-desktop
```

Verify with `mockingbird doctor`.

## 2. Pick a provider

`mockingbird llm providers` (MCP: `list_llm_providers`) lists every preset with its default model, key variable and whether that variable is set here. If the user has no preference:

1. a key variable that is already set (`key_env_set: true`) → use that provider;
2. otherwise suggest **OpenRouter** (one key, every model, free models) or **Google AI Studio** (free tier);
3. no key at all → `--offline` (faker only) now, add a provider later.

| Preset | Key variable | Extra answers |
| --- | --- | --- |
| `openrouter` | `OPENROUTER_API_KEY` (https://openrouter.ai/settings/keys) | `--model vendor/model` |
| `google-ai-studio` | `GEMINI_API_KEY` (https://aistudio.google.com/apikey) | |
| `cloudflare-ai-gateway` | `CF_AIG_API_KEY` = a Cloudflare API token with AI Gateway Run, sent as `cf-aig-authorization` | `--account-id` (or `CLOUDFLARE_ACCOUNT_ID`), `--gateway-id` (default `default`); model as `provider/model`, e.g. `workers-ai/@cf/meta/llama-3.3-70b-instruct-fp8-fast`, `openai/gpt-4.1-mini`, `google-ai-studio/gemini-2.5-flash` |
| `cloudflare-workers-ai` | `CLOUDFLARE_API_KEY` (token with Workers AI permission) | `--account-id` |
| `openai`, `anthropic`, `deepseek`, `groq`, `mistral`, `together` | `<VENDOR>_API_KEY` | |
| `azure-openai` | `AZURE_OPENAI_API_KEY` (sent as `api-key`) | `--resource`, `--model <deployment>` |
| `ollama`, `lm-studio` | none | local runtime must be running |
| `custom` (bring your own key) | `--api-key-env NAME` | `--base-url`, `--model`; other auth via `--header 'name=Bearer ${env:NAME}'` |

Cloudflare AI Gateway variants:
- Keys stored in the gateway (BYOK) or Unified Billing: the preset as is (only `cf-aig-authorization` is sent).
- Pass your own provider key through the gateway: add `--api-key-env OPENAI_API_KEY` (sent as `Authorization: Bearer`).
- Gateway with authentication off: `--no-gateway-token --api-key-env OPENAI_API_KEY` (no `cf-aig-authorization`, the provider key only).

## 3. Configure

New project (writes `mockingbird.yaml` with storage, the profile and mocks):

```sh
mockingbird init --provider openrouter --model google/gemini-2.5-flash --contract ./openapi.yaml
```

Existing config file, or a second profile:

```sh
mockingbird llm configure --provider cloudflare-ai-gateway --account-id <id> -c mockingbird.yaml --default
```

A running server without a config file (Admin API; the variable must be set in the server's environment):

```sh
mockingbird llm configure --provider google-ai-studio --apply
```

MCP equivalents: `init_config`, `configure_llm` (`target: "file"` or `"server"`). The output lists `secrets` and `next_steps`; any secret with `set: false` must be exported by the user:

> Please run `export OPENROUTER_API_KEY=…` in the shell that starts Mockingbird (or add it to your shell profile). I will not handle the key itself.

Never ask the user to paste a key into the chat, and never write it into a file or argument.

## 4. Test

```sh
mockingbird llm test -c mockingbird.yaml --profile openrouter
```

(MCP: `test_llm`.) `ok: true` means the provider answered with JSON. Otherwise read `error` and `hint`:

- 401/403: wrong key or variable not exported in this shell.
- 404: wrong model name, account ID or gateway ID.
- 400: the model rejects JSON mode or temperature: try another model or `--json-schema false`.
- `latency_ms` above the profile timeout (30s by default): raise it with `--timeout 60s` or pick a faster model; otherwise the mock falls back to the faker.

Finally serve (`mockingbird server start -c mockingbird.yaml`) and check a response header: `X-Mockingbird-Source: llm`. On `fallback`, `X-Mockingbird-Fallback-Reason` says why.

## Profile fields

A profile in `mockingbird.yaml` (`llm.profiles.<name>`): `base_url`, `model`, `api_key` (secret reference `env:NAME` or `file:/path`), `json_schema`, `timeout`, `temperature`, `max_tokens`, `fallback` (another profile tried on failure), `extra_body` (JSON merged into each request, e.g. OpenRouter `reasoning` settings), `headers` (values may embed `${env:NAME}`; credential-like names must). `llm.default_profile` applies to mocks without `llm_profile`.
