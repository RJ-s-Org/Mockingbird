# mb-ext

The extension contract of [Mockingbird](https://github.com/rj-s-org/mockingbird): the traits a build of the server can replace, and the self-hosted defaults the open-source binary uses. `mockingbird-cloud` builds against this crate and nothing else.

| Trait | Called | Default |
| --- | --- | --- |
| `TenantResolver` | on every data-plane call: who is calling? | `SingleTenant` (tenant and workspace `default`) |
| `QuotaGuard` | after the tenant is known: may they call? | `AllowAll` |
| `UsageSink` | after every call: status, source, latency, tokens | `PrometheusUsage` (`mb_requests_total`, `mb_request_duration_seconds`, `mb_llm_tokens_total`) |
| `SecretStore` | when a secret reference is resolved | `DefaultSecrets`: `env:`, `file:` and, with `VAULT_ADDR`, `vault:` |
| `ProviderPolicy` | when an LLM profile is chosen for a call | `ConfiguredProfile` (the mock's own profile) |
| `AdminHooks` | at startup: extra Admin API routes | `NoHooks` (none) |

`Extensions::default()` wires the defaults together; the `with_*` builders replace one each:

```rust
use std::sync::Arc;
use mb_ext::Extensions;

let ext = Extensions::default()
    .with_tenants(Arc::new(MyTenants))
    .with_quota(Arc::new(MyQuota::new(100)));
```

[`examples/custom-ext`](../../examples/custom-ext) is a complete binary built this way: the stock server, flags and all, with a header-based tenant resolver and a per-tenant quota.

## Stability policy

`mb-ext` follows [semantic versioning](https://semver.org/) from 1.0, independently of the Mockingbird release version. Within 1.x:

- **Traits only gain methods with a default body.** An implementation written against 1.0 keeps compiling against every 1.x.
- **Types the core builds and hands to you are `#[non_exhaustive]`**: `CallInfo`, `UsageEvent`, `Extensions` and `ExtError`. They may gain fields or variants in a minor release. Build them with their constructors (`CallInfo::new`, `UsageEvent::new`, `Extensions::default()` plus the `with_*` builders), and match `ExtError` with a wildcard arm.
- **Types you build and return to the core stay exhaustive**: `Rejection` and `Tenancy`. You may build them with struct literals (or `Rejection::new`, `Tenancy::new`); adding a field to either is a 2.0 change.
- **Re-exported dependency types are part of the API.** `axum` 0.8 (the `AdminHooks::routes` router) and `http` 1 (the `CallInfo::headers` map) appear in public signatures, so a major upgrade of either is a major release of `mb-ext`.

Every pull request is checked by `scripts/semver-check.sh` (`cargo semver-checks` against the last release tag) in CI, so a breaking change cannot ship in a 1.x release by accident.

## Secret references

`DefaultSecrets` understands:

- `env:NAME`: the environment variable `NAME`;
- `file:/path`: the file's contents, surrounding whitespace trimmed;
- `vault:<mount>/<path>#<field>`: field `<field>` of the HashiCorp Vault KV v2 secret `<path>` under mount `<mount>`, when `VAULT_ADDR` is set. The token comes from `VAULT_TOKEN` or the file named by `VAULT_TOKEN_FILE`; `VAULT_NAMESPACE` and `VAULT_CACERT` are optional. Values are cached for 5 minutes, and errors never contain the value.

AWS, GCP and Azure secret managers are not implemented natively: mount their secrets as files with the Secrets Store CSI driver and use `file:` (see the [Kubernetes guide](../../docs/deploy/kubernetes.md#secrets)).

## License

MIT OR Apache-2.0.
