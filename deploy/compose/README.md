# Compose stack: Mockingbird with Rauthy

A local stack with OIDC login: Postgres, [Rauthy](https://github.com/sebadob/rauthy) 0.37.1 as the identity provider, a one-shot bootstrap job, and Mockingbird built from this checkout.

```sh
docker compose -f deploy/compose/docker-compose.yml up --build
```

| URL | What |
| --- | --- |
| http://localhost:9090 | Mockingbird console and Admin API (redirects to Rauthy to log in) |
| http://localhost:8080 | Mockingbird data plane (the mocks) |
| http://localhost:8081/auth/v1/admin | Rauthy admin UI |

Log in to the console as **`demo@localhost.test`** / **`Demo-Passw0rd-123!`**. The demo user is in the groups `mb:sysadmin` and `mb:default:admin`, so it is a sysadmin and an admin of the `default` workspace. The Rauthy admin is `admin@localhost.test` with the password in `BOOTSTRAP_ADMIN_PASSWORD_PLAIN`.

> **Development only.** Every secret in `docker-compose.yml` (Rauthy's encryption key, Hiqlite secrets, the bootstrap API key, the admin and demo passwords, the Postgres password) is a published dev value, cookies are not `Secure`, and everything is plain HTTP. Do not expose this stack beyond your machine or reuse these values.

## How it fits together

- **`postgres`** (`postgres:16`) stores Mockingbird's control plane and session state.
- **`rauthy`** runs with its embedded Hiqlite database and `LISTEN_SCHEME=http` on port 8081, so its issuer is `http://localhost:8081/auth/v1/` (with the trailing slash, which `mockingbird.yaml` must repeat exactly). It has no config file, so everything is set through environment variables, including the WebAuthn relying party (`RP_ID=localhost`, `RP_ORIGIN=http://localhost:8081`), without which Rauthy refuses to start. Its metrics listener is turned off because it defaults to 9090, Mockingbird's admin port.
- **`rauthy-bootstrap`** (`curlimages/curl`) waits for Rauthy's discovery document, then runs `rauthy-bootstrap.sh` with Rauthy's bootstrap API key (access to clients, groups, users and roles, plus `Secrets` read/update for the client secret). It creates the groups, the confidential client `mockingbird` (redirect URI `http://localhost:9090/auth/callback`, post-logout redirect URI `http://localhost:9090/`) and the demo user, and writes the client secret to `/bootstrap/client_secret` in the shared `bootstrap` volume. It is idempotent: it looks every object up before creating it (Rauthy answers a duplicate with 400), and the secret is only regenerated when the client was just created or the file is missing.
- **`mockingbird`** starts once Postgres is healthy and the bootstrap job has succeeded. It reads `mockingbird.yaml` (`auth.mode: oidc`, `client_secret: file:/bootstrap/client_secret`) and runs with `--offline`, so it needs no LLM key.

Mockingbird and the bootstrap job share Rauthy's network namespace (`network_mode: "service:rauthy"`). That is why `localhost:8081` is the same issuer for the browser, through the published port, and for the Mockingbird server, and why Rauthy's service publishes Mockingbird's ports 8080 and 9090.

## When the bootstrap fails

The Rauthy admin API calls in `rauthy-bootstrap.sh` were checked against Rauthy 0.37.1; if Rauthy changed, compare them with the Swagger UI at http://localhost:8081/auth/v1/docs. If the job only prints `curl: (7) Failed to connect`, Rauthy itself did not start: read `docker compose -f deploy/compose/docker-compose.yml logs rauthy`. When a call fails, the script prints the response and these manual steps, and exits non-zero (Mockingbird then does not start):

1. Open http://localhost:8081/auth/v1/admin and log in as `admin@localhost.test`.
2. **Groups:** create `mb:sysadmin` and `mb:default:admin`.
3. **Clients:** create the confidential client `mockingbird` with redirect URI `http://localhost:9090/auth/callback` and post-logout redirect URI `http://localhost:9090/`. Allow the scopes `openid`, `profile`, `email` and `groups` (the `groups` claim carries the roles). Copy its secret into the bootstrap volume:

   ```sh
   docker compose -f deploy/compose/docker-compose.yml run --rm --entrypoint sh \
     rauthy-bootstrap -c 'printf %s "<secret>" > /bootstrap/client_secret'
   ```

4. **Users:** create `demo@localhost.test`, set the password `Demo-Passw0rd-123!`, and add the user to both groups.
5. Start Mockingbird without the bootstrap job:

   ```sh
   docker compose -f deploy/compose/docker-compose.yml up -d --no-deps postgres rauthy mockingbird
   ```

## Resetting

`docker compose -f deploy/compose/docker-compose.yml down -v` removes the containers and the `postgres`, `rauthy` and `bootstrap` volumes; the next `up` bootstraps from scratch.

## End-to-end test

CI's `e2e` job starts this stack and runs `crates/mb-console/web/e2e-rauthy/full-flow.spec.ts`. It logs in through Rauthy, creates a mock, uploads a contract, calls it, watches the live tail, resets a session and checks the audit log. To run it locally against a running stack:

```sh
cd crates/mb-console/web
npm ci && npx playwright install --with-deps chromium
npx playwright test -c playwright.rauthy.config.ts
```
