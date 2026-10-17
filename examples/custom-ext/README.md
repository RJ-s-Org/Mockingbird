# Custom extension example

A worked example of building a Mockingbird binary with your own
[`mb-ext`](../../crates/mb-ext) extensions. It is what `mockingbird-cloud`
does at a larger scale: the open-source server, unchanged, with different
answers to "who is calling?" and "may they call?".

| Extension | Trait | Behaviour |
|---|---|---|
| `HeaderTenants` | `TenantResolver` | The tenant is the `X-Tenant` request header (`anonymous` when it is missing); the workspace is always `default`. |
| `FixedQuota` | `QuotaGuard` | Each tenant may make `per_tenant` calls; after that it gets `429 Quota exceeded` with `Retry-After: 60`. Counts are in memory, per process. |

`extensions(per_tenant)` starts from `Extensions::default()` (the self-hosted
defaults) and swaps in these two with `with_tenants` and `with_quota`. The
binary passes `extensions(100)` to `mockingbird::server::run_with`, so it takes
exactly the same flags and environment as `mockingbird serve`.

## Run it

```sh
cargo run -p mockingbird-custom-ext-example -- \
  fixtures/openapi/orders-3.1.yaml --offline

curl -H 'X-Tenant: acme' http://127.0.0.1:8080/orders
```

The 101st call from tenant `acme` is answered 429; other tenants are
unaffected.

## Test it

```sh
cargo test -p mockingbird-custom-ext-example
```

## API stability

`mb-ext` follows semver from 1.0. `scripts/semver-check.sh` compares its
public API with the last `v*` release tag using
[`cargo-semver-checks`](https://github.com/obi1kenobi/cargo-semver-checks)
(it prints `skipped: no release tag yet` before the first release), so code
like this example keeps compiling across `mb-ext` 1.x releases.
