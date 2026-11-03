# Upgrading

Mockingbird is 0.x: minor releases may change behaviour, and each one is described here and in the [changelog](../CHANGELOG.md). The `mb-ext` crate is versioned on its own and is semver-stable from 1.0.

For the mechanics of a zero-downtime upgrade (control nodes first, then data nodes), see [rolling upgrades](operations/runbook.md#rolling-upgrades).

## Migrations

Migrations run at startup under an advisory lock, for SQLite and Postgres alike, and are forward-only: back up the control store before upgrading (see [backup and restore](operations/runbook.md#postgres-backup-and-restore)), and do not start an older binary against a migrated database.

| Migration | Release | Change |
| --- | --- | --- |
| `0001_init` | P2 | Mocks, contract versions, scenarios, LLM profiles, request log. |
| `0002_workspaces` | P3 | Workspaces and per-mock data-plane auth. |
| `0003_auth_audit` | P3 | API keys, admin sessions and the audit log. |
| `0100_events` | P5 | `request_log.kind`, channel and event settings, scheduler leases. |
| `0200_smart_import` | P6 | `contract_versions.draft_meta` and import jobs. |
| `0300_hardening` | P7 | `mocks.chaos`, `mocks.limits`, the `token_usage` table. |
| `0900_admin_session_grants` | Unreleased | Nullable `admin_sessions` columns (`claims`, `mapping_hash`, `refresh_token`, `refreshed_at`) so console roles can be re-derived after login. Additive; sessions from before keep their login roles. |
| `1000_profile_output_mode` | Unreleased | Nullable `llm_profiles.output_mode` (`none`, `json_object`, `json_schema`). Additive; a profile written before reads its mode from `json_schema`, which is still written, so older nodes keep working during a rolling upgrade. |
| `1100_import_job_tenant` | Unreleased | Nullable `import_jobs.tenant`: the tenant an import job is billed to, so whichever node runs it bills that tenant. Additive; a job queued before (or by an older node during a rolling upgrade) has none and is billed to `default`, as before. |

### `0300_hardening` is additive

`0300_hardening` only adds: a nullable `mocks.chaos` JSON column, a `mocks.limits` JSON column that defaults to `{}`, and the table `token_usage(mock_id, day, tokens)`. No existing column changes, and stored JSON from earlier releases reads back unchanged, so:

- it runs quickly on a large database (no table rewrite);
- nodes of the previous release keep working against the migrated schema during a rolling upgrade (they ignore the new columns);
- no data needs converting. Existing mocks have no chaos and no limits until you set them.

## Upgrading past 0.1.0 (unreleased)

### Creates can be refused, and prompts show declared headers and parameter descriptions

No migration or configuration.

- **A create can answer a declared conflict**: the refusal channel (see [below](#prompts-show-where-a-minted-value-came-from-and-a-write-can-be-refused)) now applies to a create too, when its operation declares a `400`, `409`, `412` or `422` by code and its prompt shows a stored record it could conflict with: its target, a parent its path names among the related resources, or a record of the type it creates that creates put in the same collection (an earlier refund under `/captures/C1/refunds`, as `mb_engine::GenerationContext::alongside` lists them). A record of that type that the request only names by value (a comment's `in_reply_to`) is no conflict, and a create shown nothing is offered none.
- **A create is shown the records it repeats**: when its operation declares one of those statuses, a create is shown, first among the related resources and listed with the member in a `<same_values>` block of its refusal note (`mb_engine::GenerationContext::same_values`), up to 3 stored records of the type it creates that hold a top-level string of its body under the same member, and is offered the refusal. A value that identifies what holds it (a member of `format` `email`, `uuid`, `uri` or `hostname`, a value with an `@`, or of 8 or more with letters and digits both) matches up to 4 holders; a plain string of 6 to 128 bytes with a letter matches only when one live record holds it and that record agrees with every other member the request sets that it holds (objects by their members, numbers by value). Enum, `const`, date and time values, `writeOnly` and credential members never count. The value index now also keeps up to 8 such plain values per record, used for nothing else: references by value still match only identifying values. As with references by value, only records this runtime stored are found, and member values only with a store this process alone writes to. It costs no store read for a create that declares none of the statuses, has a forced status or a scenario, or holds no string a stored record of its type holds; otherwise at most 8.
- **Which statuses a create is offered**: those of `409`, `412` and `422` it declares; `400` only when it is the only one of the four it declares, as a `400` beside them most often says the body is malformed. Updates and other writes are offered every one they declare, as before.
- **Refusal notes quote the contract**: every refusal note lists each status with the contract's description of that response, cut to 160 characters and escaped, in a `<refusal_statuses>` block framed as data, not instructions, and says to refuse only when the resources shown contradict the request.
- **Declared headers reach `<request>`**: a header parameter the operation declares and the request carries is shown as `"headers"` in the `<request>` block, under its declared name, its value cut to 128 characters (`mb_engine::prompt::MAX_HEADER_VALUE_CHARS`). Never shown: a header any API-key security scheme of the contract names (declared or referenced), `Authorization`, a header whose name the context credential checks hide, whose schema is `writeOnly` or `format: password` (written beside a `$ref` too, and through `$ref`, `allOf`, `anyOf` and `oneOf`) or whose value looks like a credential, transport headers (`Content-Type`, `Accept`, `Host`, `User-Agent` and the like) and `X-Mockingbird-*`. A request carrying no such header gets the `<request>` block it always did. A bundle compiled before this release (no `api_key_names`) shows no request header at all until it is recompiled, as it cannot tell a header an unreferenced scheme uses.
- **The read cache keys by those headers**: a read carrying a shown header is cached under them too (see `mb_engine::prompt::shown_headers`), so a read with another `X-Locale` or version generates its own answer. A read carrying none keeps its key.
- **Secret query values stay out of the prompt**: the value of a query parameter that an API-key scheme names, whose schema is `writeOnly` or `format: password`, or whose name the context credential checks hide (except one hidden only by a word as often an identity's, such as `project_key`), is left out of `<request>`; API-key names are left out of `<request_defaults>` too. This holds whatever `data_plane_auth` is; the request itself and its validation are unchanged. A bundle compiled before this release goes by the names its compiled requirements use.
- **API keys stay out of the request record**: with any `data_plane_auth` mode, including `none`, a header or query parameter that any API-key scheme of the contract names (`Bundle::api_key_names`, referenced or not) is left out of the request record, on HTTP, SSE, GraphQL and SOAP alike, as credential-named ones already were. A bundle compiled before this release records as it did.
- **Parameter descriptions reach the prompt**: the contract's description of each path, query and shown header parameter the request sends goes in a `<parameter_descriptions>` block after `<request>` (and `<request_defaults>`), framed as data, each cut to 160 characters and all together at most 1200 (`mb_engine::prompt::MAX_PARAMETER_DESCRIPTIONS_CHARS`). Credential-named, secret-marked and API-key parameters are left out.
- **Recompile for descriptions and scheme names**: the OpenAPI 3 and Swagger 2.0 importers now keep a parameter's own `description` on its schema when the schema has none of its own, beside a `$ref` when the schema is one (an annotation, which validates as the reference alone did). Smart imports compile through the OpenAPI 3 importer and get the same. They also keep every API-key name the security schemes declare as `Bundle::api_key_names`, whether or not a requirement the mock compiled references it: `Some` list, possibly empty, and `None` (left out of the JSON) for a bundle compiled before, or by an importer with no such schemes (GraphQL, gRPC, SOAP, AsyncAPI). Both forms deserialize. Stored bundles get descriptions and names after a recompile; until then they show no request headers, and their query and records go by the names their compiled requirements use.
- **Recorded prompts**: a recording of a call that sends a declared header, an API-key query parameter or a described parameter, or of a refusal note, may not replay.
- **Rust API**: `GenerationContext` gains public fields `alongside` and `same_values`, and `Bundle` gains `api_key_names: Option<Vec<ApiKeyName>>`, which break code constructing them in full (`None` keeps the old behaviour). `mb_mcm::credentials::schema_marks_secret`, `Bundle::knows_api_key_names`, `Bundle::declares_api_key`, `Bundle::is_security_name` and `mb_engine::prompt::shown_headers` and `mb_engine::guards::declares_a_refusal` are new.

### Keys from path parameters, types from record schemas

No migration or configuration. A compiled bundle can change, also for offline mocks ([how it is read](import.md#how-a-resources-identity-is-read)).

- **A path parameter names its key by its words**: the longest group of its last words that names a member, not an object or a list, separators and case aside. The fixed endings (`Number`, `Sid`, `Key`, `IdOrKey`, `Uuid`) are gone, and GitHub's, Jira's and Twilio's messages and calls compile as before. In the examples and benchmark contracts, the changes are Asana's tasks and projects (`gid`, in a `{data}` envelope), GitLab's issue read, edit and delete (by `iid`, beside the `id` they are stored by), Sentry's project read and update (`path_field: slug`; `{project_id_or_slug}` names the `id` too, so the path's value still goes in the `id`, as for Jira), and Twilio's `FetchPhoneNumber`, which now reads by `phone_number`: a looked-up number is stored, and the same lookup is answered from state.
- **A create goes by the member its own type's reads by path use**, when its answer has no `id`: Asana's `POST /tasks` by `gid`. Another type's reads teach it nothing. A `DELETE` of the type on the same parameter, which answers nothing, goes by the same identity when the path names another member than the key: GitLab's `deleteIssue` by `iid`. A `sid` or a `uuid` counts only where no read by path names a key, so a create of a type nothing reads by path is still stored.
- **A read by another member than the key puts the path's value in that member, never in the key**, generated or stored: GitHub's `GET /issues/7` by `issue_number`, GitLab's by `issue_iid` and a team's by `team_slug` answer `number`, `iid` or `slug` from the path and keep the answer's own `id`. The record is stored under that `id`, so the same read is then served from state. It took the path's value as its `id` and stored nothing before. A record the path's value finds as its key is another record when that member says otherwise. A generated read by such a member is stored only when it shows the path's owner (`project_id: 1` under `/projects/1/...`), where the route has one. A deleted one stays gone though another record shares its member: a read of `/projects/1/issues/2` after its delete answers as `unknown_ids: 404` does, whichever other issue has `iid: 2`. A parameter whose words name the key too (Jira's `issueIdOrKey`) may hold either, so its value still goes in the key, and a generated read of it stores nothing, as before. A recorded prompt or snapshot of such a read may not replay.
- **Rust API**: `mb_mcm::Identity::path_member`, `mb_mcm::words`, `mb_mcm::joined_name` and `mb_mcm::names_member` are new.
- **`unknown_ids: 404` can answer 404 where it generated before**, on these routes, as a read now knows it is by id and that the session does not hold it.
- **A create can change type**: when its own type has no `GET` by path and another type reads exactly its record, by a `GET` by path, else by its only lists. Freshdesk's `addNoteToTicket` and `replyToTicket` are `conversations`, Trello's `addCardComment` is `actions`, and Asana's `createSubtaskForTask` is a create of `tasks`, where it was an update of the parent. A list of each such type now shows what the create stored.
- **A version segment is never a type**, when another static segment follows it: Mailchimp's `searchMembers` is `search-members` (was `3.0`), and Spotify's `search` is `search` (was `v1`). A version-shaped segment the path ends in or a parameter follows (`/v2/{id}/cancel`) is a type as before.
- **Not covered**: a parameter whose words name nothing in the record (`/users/{handle}` over `login`), and a create beside two lists of other types listing its record, which keeps its type.

### Related writes bring their records up to date

No migration or configuration. With an LLM and `consistency: on`, on HTTP routes:

- **A create referring to a held record makes that record's next read generate once.** A value of the create's request body, or of its path beyond the parent, that holds a digit or a separator (or is a whole number of at least 100000) and is a held record's stored id (or a path field known to hold it) marks that record: `POST /refunds {"payment_id": "pay_1…"}` for the payment, a reply naming its message under any member. Values only the generated answer holds mark nothing. Where that read answered `state` before, it now costs one LLM call, once. It is offered the effects envelope; only the effects its answer lists on that record apply, and it answers the stored copy as it then stands, so nothing else of the record changes. A parent under the create's path keeps its catch-up, as before. A create-then-read-parent flow under the parent's path costs what it did; one that refers to the record by value costs one call more per affected read (five refunds by `payment_id`, each followed by a read of the payment: 11 calls, 6 before). At most 4 records per create are marked; not one an applied effect of the answer changed. A booking naming the offer it books, for instance, now makes the offer's next read generate once, and the offer stays as stored unless that answer lists an effect on it.
- **More store reads**: at most 8 per create, for the values of its request that could be an id, and one per request a catch-up is told of that created a record. A create whose request holds no such value pays none.
- **The catch-up shows what was created**: the records those creates made come in a `<new_resources>` block after the rest, before `<earlier_requests>`, with a one-line preface (about 30 tokens beside the records), and are taken out of `<related_resources>`. Kept requests gain an internal `created` member, which no prompt shows. A prompt with nothing created to show is the same to the byte.
- **A settling update is told the rule**: the user message of an update whose answer settles on its stored target (an LLM's, with the read's schema, not an action, told of no earlier requests) gains one sentence, about 20 tokens: what it changes beyond what the request names is kept only when listed as effects. Other prompts are unchanged.
- **Query members are named**: such an update's query members count as named, as its body's do, and the query's value of each is applied after settling, so `?idList=list_1` is stored whatever the answer says; a body member of the same name wins. Updates sent only as JSON or form bodies settle as before.
- **Error bodies say their status**: a 4xx or 5xx body whose schema declares an integer or number `status` allowing that status carries it, from the faker and the LLM alike.
- **Recorded prompts**: a recording of a settling update, or of a read after a create that refers to a record, may not replay.
- **Rust API**: `GenerationContext` gains public fields `created_since` and `changes_by_effects`, which break code constructing it in full; add `..Default::default()`.

### Prompts show where a minted value came from, and a write can be refused

No migration or configuration. With an LLM and `consistency: on`, on HTTP routes:

- **A value the mock minted is traced back**: a generated 2xx create or action whose answer holds a string of 8 to 512 bytes under a member named as a credential (`access_token`, `public_token`, `link_token`, not `key` or `sid`), or shaped like one, which its request did not send (its body, query, path, or `Authorization: Bearer` header, which is hashed and never kept), is remembered by a SHA-256 hash of that value. A later request sending it as a bearer token matches too. A later request carrying it is shown, in an `<earlier_origins>` block, that answer and the request that got it, as far as their schemas declare them and past the wider name and shape checks, so the value itself, any other token and the client's `secret` are left out, with any string holding one of them (`"issued <token>"`). One hop only: the request that minted the value it carried is not followed further.
- **What is kept**: one document per session, of the reserved type `__mockingbird.earlier_requests` with id `#minted`, holding the newest 8 answers, each up to 4 KiB, and 3 are shown at most. It is not exported or counted and a session reset clears it. A value the client sent is never kept, nor hashed, even when the answer repeats it.
- **More store reads**: one per generated call carrying a string of that length, on a contract with a create or action declaring such a member in its answer. Other contracts pay none, and a prompt with nothing to show is the same to the byte.
- **A write can answer a declared error**: a command other than a create, on the target record the model was shown (a create too, [since](#creates-can-be-refused-and-prompts-show-declared-headers-and-parameter-descriptions)), whose operation declares a `400`, `409`, `412` or `422` response, is offered `"refusal": <status>` in its effects envelope, with those statuses listed in the prompt and as an integer `enum` in the schema sent. A refusal naming one of them is answered as that declared error, faker-filled, with no effects and no second LLM call, tagged with the LLM's source, or `fallback` with its reason when the faker could not make the body valid. A status the operation does not declare is ignored and the answer stands. `401`, `403`, `404` and `429` are never offered.
- **An operation declaring none of those statuses is asked for exactly what it was**, and so is any call with a forced status or a scenario, a read, a write shown nothing, or an answer whose own schema has a `refusal` member.
- **Recorded prompts**: a snapshot or recording taken before this release may not replay for a call that is now offered a refusal or shown an origin. `without_effects_schema` still unwraps the envelope, with or without the refusal.
- **Rust API**: `GenerationContext` gains a public field, `earlier_origins`, which breaks code constructing it in full; add `..Default::default()`. `StateStore` gains `touch_live`, defaulted, which a store should implement so that a read never makes a listed session, and a store's `sessions` and `session_count` should leave out a session holding `mb_state::UNLISTED_ID` and no resource. **Rolling upgrades**: an older node lists such a session, with no resources, as before this release.

### Write memory

No migration or configuration. With an LLM and `consistency: on`, on HTTP routes ([what it does](concepts.md#sessions-and-state)):

- **Reads can make an extra LLM call** after a write that answered little, where they answered `state` before. One call catches up however many writes are waiting; a failing catch-up tries twice, each up to two calls, then stops until a later write. `consistency: off` and `--offline` make none.
- **A `GET` can publish an upsert**: a catch-up that changes the stored copy replaces it, so `on_state_change` triggers, subscriptions and change feeds can fire on a read, and the resource moves to the front of its type's list order, as any write does. One that changes nothing publishes nothing and keeps its place.
- **A remembered action publishes a change of its target at write time**, with the copy as it stood, where it published none, and moves it to the front of the list order. A create or a body-less action that tells a parent does **not** publish a change of that parent or move it, as nothing it holds changed: another node's read looks in the shared store before it serves. The exception is a read a node has cached that the stored copy does not answer: a query view of a shorter copy, or a non-detail view such as `/posts/{id}/summary`. It stays until something else invalidates it, so another node can answer it stale, as on earlier releases. An effect that changes a parent publishes as it always did.
- **A 204 update merges** the members its read's schema declares into the stored copy, instead of replacing the copy with the request. An update naming its target by a field its reads go by (a key) now finds it, instead of storing a second resource under that key. The merge leaves identity and path fields and `readOnly` members alone, which is intended: `PUT /pets/p1 {"id":"p2"}` used to make `/pets/p1` answer `id: "p2"`, and now it stays `p1`.
- **Recorded prompts**: a snapshot or recording taken before this release may not replay after a write, as the read now generates with an `<earlier_requests>` block. Reads with no remembered request send the same prompt as before, to the byte.
- **Rolling upgrades**: documents of the reserved type `__mockingbird.earlier_requests` are kept in the session's state. A node of an older release exports and counts them as resources, and a fixture exported through one may hold them; newer nodes drop them when seeding. Export sessions from upgraded nodes.
- **Rust API**: `GenerationContext` gains a public field, `earlier_requests`, which breaks code constructing it in full; add `..Default::default()`. `Engine::scrub` is public. `GenerationContext` also gains `earlier_own_body`. `RequestRecorder` gains a defaulted `redact`, which an implementor keeping request bodies should provide, or only the built-in credential and PII scrubbing applies.
- **Credential names**: one predicate, `mb_mcm::credentials::is_credential_name`, now decides for request headers, import redaction and the memory. It adds `session`, `credential`, `passwd`, `privatekey`, `sshkey`, `signingkey`, `cvv`, `bearer`, whole-word `auth` and `pin` to what headers treat as credentials, so a header such as `x-session-id` drops from recordings (prompts never carried request headers). `X-Mockingbird-*` overrides are exempt. A contract declaring a header does not exempt it, since contracts routinely declare `Authorization`.
- **A 204 update with a valid body plus a member the request's schema declares and the read's does not costs one extra LLM call** on the next read, as intended: the member cannot go in the stored copy, so the read is told of it. A remainder holding only undeclared members, such as `code` or `value` under an open request schema, or only redacted values, such as `password`, is not remembered and costs no call. A body that is only told, an action's or a child's to its parent, is redacted by every name rule, so `session_title` in an action's body shows as `[redacted]`.
- **The path says whose a resource is**: a 204 update's member contradicting the owner in its path (`PUT /orgs/octo/teams/core {"organization":"acme"}`) is neither applied nor remembered.

### Prompts show what a request refers to and who the caller is

No migration or configuration. With an LLM and `consistency: on`, on HTTP routes ([what it does](concepts.md#sessions-and-state)):

- **A write can be offered effects where it was not**: a body or query value equal to a held record's id puts that record among the related resources, so the call is asked for `{"effects": [...], "response": ...}`. Effects are checked as before and go on the stored copy, and nothing else changes a record. A value must be at least 6 bytes and hold a digit or a separator, or be a number of at least 100000, so a word-only id such as `closed` or `general` is not found this way. A date can still match, as can a word with a separator (`in-review`), and the model could then state a change to that record, if the change validates.
- **More store reads per generated call**: at most 32 by value, 12 on Slack's `reactions.add`; one more for `<earlier_answers>` on a contract with a `GET` route with no parameters answering a single object, and one more on a list read whose route also takes a write. A served or cached answer pays none. On a contract with more types than that reaches, the types the session wrote through this node are looked up first, so a reference to a type written on another node can be missed.
- **More prompt tokens when one applies**: about 100 for a typical remembered answer, up to 1,500 for three at the size cap. Slack's `reactions.add` with its message shown goes from 574 to 1,443 bytes, and the effects schema joins the response format. With nothing to show, a prompt is the same to the byte.
- **These are advisory**: `<earlier_answers>` and `<earlier_writes>` say what may have changed, and no answer has to follow them. A route is told its own earlier answer too, so a `/me` keeps its identity after a write, and a `/status` tends to stay as it was. An indirect change, such as `PATCH /users/{id}` on the user `/me` answered, is not tracked.
- **What a list read is told of a write**: its route and operation, and the members of its body that its request schema or the list's items declare, past the name and shape checks. Auth0's `roles` and Spotify's `uris` are told; an undeclared `pass`, `otp`, `code` or `value` is not, and neither is a declared `password`. A member the contract declares under an ordinary name (`code`) is told. A query is never kept.
- **Wider checks on these only**: what they show leaves out more names than a request record does (`pass`, `pwd`, `passphrase`, `jwt`, `otp`, `hmac`, `signature`, `license_key`, `card_number`, `credit_card`, `ssn`, `salt`, `mfa`, `recovery_code`, `connection_string`, `dsn`, and as a last word `key` and `sid`), URLs with a password or a credential-named parameter, JWTs, `Bearer ` values and pairs labelled as a credential (`{"key": "X-Api-Key", "value": ...}`, whole), omitted and not shown as a marker. `key` or `sid` stays where it is the type's identity, unless the type is itself a credential (`api_keys`). Imports, headers, recordings and the older write memory go by the names they did.
- **An earlier answer keeps only what its read's schema declares**: a route whose read names no members has nothing to keep.
- **A record matched by value follows no references of its own**, so what it names is not shown unless the request names it too.
- **An effect holding the redaction marker** leaves that member as stored and applies the rest, for every effect on a mock with an LLM. One setting only the marker is refused.
- **Recorded prompts**: a snapshot or recording taken before this release may not replay where one of these applies.
- **Not covered**: a deleted parent forgets its routes' writes only where the path below it has no other parameter, and a deleted child only the writes naming it. A caller's answer is kept per session, not per token. An answer of a `POST`-only route is not kept. A `GET` of such a route, on a session with nothing yet, keeps its answer in the shared store under an internal session that is not listed or counted (`mb_state::UNLISTED_ID`), until a write or a stored resource lists it.
- **Rust API**: `GenerationContext` gains public fields, `earlier_answers` and `earlier_writes`, which breaks code constructing it in full; add `..Default::default()`.

### The output budget keeps what a request asked for

No migration or configuration. With `limits.output_budget_tokens` on, for a response whose estimated answer is over budget:

- **More of an answer is the model's**: `type=track` now keeps `tracks`, and a page's items stay at the page size asked for while other members can go instead. What goes instead is required members of its items, which the faker fills.
- **A page size counts in the estimate**: `limit=50` can shape an answer that fit at the default of about four items, and `limit=1` can leave one unshaped that did not.
- **What counts as a page size**: an integer query parameter whose `maximum` is the `maxItems` of an array in the answer, and of that array. Otherwise, as a last resort, one whose name holds `limit`, `size`, `per_page`, `first` or `top` in any case or separator (`pageSize`, `$top`), which covers the array with the heaviest items in each object: an array of words, or a lighter list beside it, is capped as any other.
- **Answers can be shorter**: shaping used to stop over budget when a member it left out held another it had counted. It now goes on until the estimate fits.
- **Recorded prompts**: a snapshot taken before this release may not replay for such a call, as the schema sent differs.
- **Rust API**: `budget::shape` takes the request's page size as a `budget::PageLimit`, as `budget::requested_limit` reads it from the query schema, the answer's schema and the call.

### Prompts carry response descriptions and request defaults

No migration or configuration.

- **Recompile to show descriptions**: a bundle stored before this release has no response descriptions, so its prompts stay as they were until the contract is imported again.
- **More prompt tokens**: the system message's array rule is about 20 tokens longer on every call. A response description adds a `<response_description>` block of about 25 tokens plus up to 160 characters to the system message, and request defaults a `<request_defaults>` block of about 20 tokens plus the values to the user message. Page sizes and what the credential checks hide are never shown as defaults.
- **A create can answer differently**: a body member it leaves out that declares a `default` now answers the default, where the model or the faker picked any value. An update's body is never defaulted.
- **Lists can be empty**: a search that nothing matches can now answer an empty array where the model gave 3 to 5 invented items.
- **The output budget can fill different members**: deeper required members go first, and an array item's only object member with a required `id` or `uri` stays in the schema while anything else can go, so a shaped answer may keep more of each item and fill more of what lies below it. The estimate fits the budget wherever it did before.
- **New ids avoid the held records a request names**: a create's id equal to a value of its path or body that is the id of a record of another type the session holds is redrawn, so an answer that reused its channel's id is stored under a fresh one. Such a create can cost up to 32 more store reads with `consistency: on`. An id that the request does not carry, or that no held record has, is kept.
- **Recorded prompts**: the system message changed for every call, so a snapshot or recording keyed on the exact prompt and taken before this release no longer replays.
- **Rust API**: `mb_mcm::OutputVariant` gains `description`, which breaks code constructing it in full.

### An update's answer settles on the stored copy by what the request named

No migration or configuration. For an update (`PUT` or `PATCH`, not an action route) answered by an LLM with the schema of the resource's read, on a stored resource:

- **The stored copy stands, except at a path the request names.** A member of the model's answer wins only if the request body names the same path: `{"properties": {"lifecyclestage": "opportunity"}}` names `properties.lifecyclestage`, and `{"name": "x"}` names `name` but not `owner.name`. The stored copy used to go over the whole answer, which discarded the new values of a nested request. A value the model changes unasked is still dropped.
- **A request nested differently from its answer gets no benefit from this change.** `{"ticket": {"status": "solved"}}` on a response with a top-level `status`, or `{"status": "solved"}` on one with `ticket.status`, names neither: the stored copy stands, as before.
- **What the stored copy lacks comes from the answer at the top level**, as before. Below it a member goes in only at a path the request names, so a model's invention, a new map key or a faker's value does not stick.
- **Identity, path, `readOnly` and owner members never take the answer's value.** An answer showing another owner leaves the stored copy as it was and loses what it added, unless the schema requires that member: then the answer's value stays, as an answer must be valid.
- **An array goes whole, only at a path the request names, and with the request's value**, as an output budget's cap or an empty answer can cut the answer's. An object or branch under a `oneOf` or `anyOf` goes whole at a path the request names, or not at all.
- **A null in the answer is ignored unless the request names that path as null**: `{"properties": {"Due": null}}` clears it. A request's null on a member that cannot be null keeps the stored value.
- **`PATCH` merges the request at any depth**, so `{"a": {"b": 1}}` keeps `a`'s other members. It used to replace `a` when that validated. When the merged object is invalid the request's member goes whole, as a switch of a union's branch needs. `PUT` still replaces `a`, and never removed members it does not name.
- **An effect on the update's own target** applies when the merged resource validates, as before, and now without what the resource does not declare, a path member, a member the request names at the top level, or what would show another owner. It is how a model states what follows from an update, so it can state a `closed_at` the contract marks `readOnly`, which only the answer cannot change. It can also still set other members, such as a `created_at` or an array.

An update whose answer has another schema, as an `Ack {status, count}` or Zendesk's `{ticket, audit}` has, behaves as before, and so do faker answers, offline mocks, GraphQL, action routes and a catch-up of an update told of earlier requests: the stored copy, then each member of the request whole.

### Deleted records stay deleted

No migration or configuration. With an LLM and `consistency: on`, except on GraphQL:

- **A read of a deleted id is a 404 with no LLM call**, whatever `unknown_ids` says, where `generate` used to invent a replacement. As with `unknown_ids: 404`, it is the contract's 404 if declared, else `application/problem+json`, which such a contract does not describe.
- **Deleting an id the session never held counts**: the read after it is a 404.
- **A read naming a seed is a 404 too**: `X-Mockingbird-Seed` on a deleted id used to answer that seed's 200, and no longer does.
- **A delete holds for its parent's path**: `DELETE /projects/1/tasks/5` says nothing of `/projects/2/tasks/5` or `/projects/2/tasks`, whatever the parameters are called, and removing a user from `/x/1/members/{user_id}` leaves their membership of `/x/2`. A read through another route to the same record, such as a flat `/tasks/5`, is another path and is not marked, so it generates as before.
- **A mention from another route can bring it back, as before**: after `DELETE /projects/1/tasks/5`, an answer on `/digest` or under `/projects/2`, or a list in project 2, that holds task 5 stores it, and `GET /projects/1/tasks/5` then answers it from state. A mention under the same parent does not.
- **A generated list drops a deleted id** when the answer stays valid, so a `minItems` can keep it in the answer, though it is not stored. A count beside the list goes down only when the contract declares it an integer and it is named `total`, `count`, `total_count`, `total_results`, `total_items`, `<list>_count` or `total_<list>`, with one such member to choose from.
- **An update or a create storing that id brings it back**, including a `PATCH` of a deleted id, which generates and stores as it does for any unknown id.
- **Reads can make extra store calls**: one or two for a read that would generate, and one per item or mention seeded from a generated answer. A served read makes none, and a delete adds a few writes.
- **Rolling upgrades**: markers are documents of the reserved type `__mockingbird.earlier_requests`, with ids starting `#deleted`. A node of an older release exports and counts them as resources, as it does the write memory's, and does not honour them.

The oldest markers go past 1024 per session, and then what they marked can come back. Offline mocks and `consistency: off` are as they were.

### Lists and lookups agree with the session

No migration or configuration. With an LLM and `consistency: on`, on HTTP routes except GraphQL:

- **A list answer can hold fewer items than the model wrote.** An item goes when a query parameter names one of its members and the item holds another value there, so `GET /Messages.json?To=+1555` answers only that recipient's messages. The parameter and the member must declare the same one type (text, integer or boolean), and the value must be one the member's `enum` and `format` allow: `state=all` beside `enum: [open, closed]` filters nothing. A comma-separated value is a set only when every part is in the `enum`. A real number (`lat`), a `const` member, a bare `id`, a parameter whose own `enum` has no member `enum` beside it (`state=all` against a plain-text `state`), a search's `q`, a range and an item without the member are left alone. A text member with no `enum`, `format` or `pattern` keeps the items whose value contains the requested one, ignoring case: `name=Ann` keeps `Anna`. A value holding a digit matches whole, ignoring case, so `owner_id=u1` does not keep `u10`. Values are trimmed first, and one left empty filters nothing. An unencoded `to=+1555` arrives as ` 1555`, so a value with a leading space and a digit also matches `+1555`.
- **A page size caps the list's main array**, by the signal the output budget already reads (a parameter whose `maximum` is the array's `maxItems`, else one named for a page size).
- **A page size goes by a whole name**: without a `maximum`, a parameter counts only when its normalized name is exactly `limit`, `per_page`, `page_size`, `max_results`, `size`, `first` or `top`. A word inside a name used to count, so `topic_id`, `desktop`, `first_seen` and `image_size` no longer shape the output budget, and `maxResults` now does.
- **Neither goes below `minItems` or leaves an invalid answer**: then the answer stays as it was. A count clearly the list's own goes down by what a filter took out; a page size leaves it, as a total counts every page.
- **A lookup does not bring a deleted record back under a new id.** A delete of a stored record also marks, by hash, the text of its identity and of the members a lookup of its type keys on: those a list read's required query parameter names (`/users-by-email` requires `email`). An optional filter such as `state` or `status` never marks. Value markers keep the newest 1024 under their own bound, so id markers keep theirs. A generated item of that type the session does not hold, carrying a marked value the request names, is dropped. Storing a record with that value again clears the mark, so a recreated user is found by email. A record the session never held marks only its id, as before.
- **A create or a read under a parent path is shown what was created there**: the newest 4 records creates put under the same path, scrubbed as other related resources are. A generated read of the parent record itself (`GET /captures/C1`, a catch-up included) is shown the newest 4 created under its path (`/captures/C1/refund`) the same way. Nothing it is shown is stored or changed by being shown. Prompts can be longer, and a recorded prompt may not replay. Which creates went where is kept per node, as the owner hint is: on another node or after a restart, the prompt is as before.
- **Rolling upgrades**: the new markers are documents of the reserved type, as the deleted markers are, and an older node ignores them.

Offline mocks and `consistency: off` are as they were.

### A member named for its type is its identity

No migration or configuration. A contract whose routes already have an identity is as it was, with the same LLM calls and store calls.

- **What changes:** with no other identity, a member named for the type becomes one: `user_id` or `userId` for `users`, `category_id` for `categories`. It must be the only such member, and not an object or a list. The type needs a route ending in a parameter, and each such route must have an id-like one: `{id}`, `{userId}`, `{user_id}`. So `/users/{handle}` gets none, and `GET /users/alice` keeps the `user_id` it is answered. A compiled bundle can change, also for offline mocks.
- **What follows:** the Auth0 example's user is now stored by `POST`, served by `GET`, updated by `PATCH` and marked by `DELETE`.
- **`unknown_ids: 404` can answer 404 where it generated before**: `GET /accounts/7` over `account_id`, and Auth0's users, as a read now knows it is by id and that the session does not hold it.
- **Not covered**: any other name is not guessed. [`x-mockingbird-resource`](import.md#correcting-it-x-mockingbird-resource) sets the identity where the contract's names do not give it. Twilio's lookup is as it was. Envelopes and arrays are below.

### A record in an envelope or an array has an identity

No migration or configuration. A contract whose routes already have an identity is as it was, with the same LLM calls ([how it is read](import.md#how-a-resources-identity-is-read)).

- **What changes in a compiled bundle:** an identity can say where the answer holds the record, as `record: {member: "payment"}` or `record: item`; a bundle without one serializes as before. Square's payment, customer and refund routes, Zendesk's ticket and user routes, and SendGrid's `GET` and `DELETE /v3/suppression/bounces/{email}` gain an identity. Square's `CompletePayment` becomes an update of the payment, where it stored nothing.
- **What follows:** these records are stored as what the envelope holds, served from state in their envelope, and `unknown_ids: 404` can answer 404 for them. A SendGrid bounce read by address answers `[{...}]` from state, and `[]` once deleted, where a deleted record answers 404 elsewhere.
- **Not covered**: an envelope with several members none of which, or more than one of which, is the type's record schema (`{data: User, links}` with no other route answering a `User`), and an array whose items do not require the member the parameter names.
- **An enveloped record is checked as the record**: catch-ups, effects, write-memory merges and mentions check a stored ticket or bounce against its own schema, not its read's envelope or array.
- **Rust API**: `mb_mcm::Identity` gains the public field `record`, which breaks code constructing it in full, and `mb_mcm::RecordAt` and `Identity::record_in`, `record_in_mut` and `answer_holding` are new.

### Records and lists by their schemas

No migration or configuration.

- **A list is typed by its items** under a path parameter or beside its own type's routes, when every array it lists holds exactly what one type's `GET` by path serves. In the examples: Notion's `query-data-source` and `post-database-query` are `pages`, Jira's `GET /search/jql` is `issue`, Spotify's `get-an-artists-top-tracks` is `tracks`, and GitHub's `/installation/repositories` and `/search/repositories` are `repos`. They are shown that type's stored records and store what they list under it. A shorter or longer form of the record, as a search result composing it with `allOf`, keeps the route's type.
- **A single answer with an identity of its own is stored as itself**: OpenWeatherMap's `GET /data/2.5/weather` stores the reading by its city `id`, where it stored each `weather` condition as a `weather` record. A route with a path parameter that is not the identity (TMDB's `movie-credits`) stores nothing. The lists a record's answer holds are stored only where another read serves exactly their schema, under its type: an order's `lines` with no read of their own, stored as `orders` before, are not stored.
- **A read below a record that answers exactly one of its members is served from it**: Spotify's `GET /playlists/{id}/items` answers the stored playlist's `tracks` from state, one LLM call fewer, when the request has no query and no write the playlist does not show yet is remembered for it or for that path. A write there still reaches the playlist through the write memory, and the next read of the items generates, told of it.

### A lookup reads as the type of its items

No migration or configuration. A read whose path has no parameter, whose type has no other route, and every listed array of which holds what one type's `GET` by path answers, now has that type: Auth0's `/users-by-email` is `users`, Zendesk's `/search` is `tickets` and Notion's `/search` is `pages`.

- **It is shown that type's stored records, and stores what it lists under them**, instead of nothing. A prompt can be longer, and a recorded prompt may not replay.
- **Another shape beside them keeps the route's own type**: `{users: [Hit], tickets: [Ticket]}` would store a hit as a ticket. An array of strings does not count. Items with no declared members do, as they could be anything.
- **A read of one record (`/me`) keeps its type**, as does a read whose items two types' `GET`s answer, or one set by `x-mockingbird-resource`. A list under a path parameter (`/teams/{team}/members`) or beside its type's routes keeps its type unless its items are exactly a type's record (see [records and lists by their schemas](#records-and-lists-by-their-schemas)).

### Action routes act on their parent resource

A `POST <static>/{param}/<verb>` the contract shows acting on its parent ([how that is read](import.md#how-a-post-under-a-resource-is-read)), such as `POST /v1/payment_intents/{intent}/confirm`, used to compile as a create on a resource type named after `<verb>`. It now acts on `<static>`: an update when it answers what `GET <static>/{param}` answers, otherwise a command that stores nothing. Operation ids and routes stay the same. These change:

- its operation's `kind`, `resource_type` and `identity` in the compiled bundle, while `source_digest`, which hashes the source, does not;
- its webhook event type, from `confirm.created` to `payment_intent.updated`, or for a command that answers a record of its own, from `cancel.created` to `booking.updated`;
- what state it writes: resources stored under the old type (`confirm`) in a session or a session snapshot are no longer read by anything, and `on_state_change` triggers on that type stop firing.

A bundle takes this on recompile, and stored bundles are not recompiled on their own. For a mock managed through the API, upload the contract again. For a mock from the config file, restart: with an in-memory control store that is enough, but with a persistent one a mock keeps its active version while the contract's digest matches it, so also change a byte of the contract, such as a comment.

### Action routes come from the contract's structure

`POST <static>/{param}/<static>` used to be an action only if its last segment did not end in `s`, it had no `GET` on its path and no route below it. The spelling no longer counts, and [the contract decides](import.md#how-a-post-under-a-resource-is-read). On recompile:

- A `POST` whose successes declare no body acts on its parent and stores nothing, even with a `GET` beside it or a longer static tail: Jira's `POST /issue/{issueIdOrKey}/transitions` and Auth0's `POST /users/{id}/roles`. It sends no webhook unless the contract gives it a payload.
- A bodyless `POST` whose last segment is a read verb (`POST /jobs/{id}/sync` answering `204`) was a read and is now an action on its parent.
- A plural tail that answers what the parent's `GET` answers (`POST /orders/{id}/items` answering the order, with nothing below it) was a create of `items` and is now an update of the order.
- An update on an action route whose LLM answer carries an identity other than its target's is [a new record of the parent's type](import.md#an-update-that-answers-another-record), stored under its own id, or a fresh one if a record holds that id, with the target untouched, and sends `<type>.created`. If no fresh id can be drawn, the answer is served and not stored. The id used to be overwritten with the target's. A faker answer, an answer with no identity, or a call with no stored target updates the target as before. This depends on the model: for a reply with a stored parent, a new id came back 8 of 8 times with Luna and the default model and at most 2 of 8 with Lightning, so a reply can still overwrite its parent. Where every call makes a new record, set `x-mockingbird-resource: {action: false, type: <the parent's type>}`.
- An action answering `{type: object}` beside a parent's `GET` answering `{type: object}` moves from an update to `Effect::Other`: it stores nothing, since two open schemas do not say the answer is the parent.
- A `POST` answering `200`, whose `GET` beside it reads something else or that has no `GET` and nothing else reading its answer, acts on its parent whatever its spelling: `POST /v1/customers/{id}/sources`, `/{id}/process`, `/{id}/address`.
- A singular tail declaring a `201`, which acted on its parent unless it answered the parent's record, is now a create of its own, even with no identity (when it stores nothing). It loses its parent as a target: with no identity path parameter it has no stored target, and its prompt shows the parent only if the path parameter is named for a type (`{booking_id}`, not `{id}`).
- A `POST` whose answer another resource's `GET` serves by path is a create under that route's type: PayPal's `POST /captures/{capture_id}/refund` is a create of `refunds`, so `GET /refunds/{id}` serves it. It was a command on `captures` that stored nothing, and it now has no capture as its target.
- A `POST` declaring a `201` beside a `200` is judged by the `201`.

For each operation that changes, these follow:

- its `kind`, `resource_type` and `identity` in the compiled bundle. Routes, operation ids and `source_digest` stay the same;
- its webhook event type, from `roles.created` to nothing at all for a bodyless action, from `capture.updated` to `refund.created`, and to `<type>.created` for an action's new record. `Bundle::webhook_for` also finds a contract's payload by event type, so a webhook the contract names after the old event type (`capture.updated`) stops matching this operation: the event goes out as the default envelope unless a callback of the operation or a webhook named after its id applies;
- what state it writes: resources stored under the old type in a session or snapshot are no longer read by these operations, and `on_state_change` triggers on that type stop firing for them.

Set [`x-mockingbird-resource`](import.md#correcting-it-x-mockingbird-resource) on an operation to keep what it was. It is per operation. Contracts without the extension behave as inference says.

Stored bundles take this the way they take action routes, with the same steps: upload the contract again, or restart a mock from the config file, changing a byte of the contract if the control store is persistent.

### Resources composed with `allOf` have an identity

A resource whose schema is an `allOf` at its root used to compile with no identity. The importer now reads the members of its `allOf` branches, so every such resource has one on recompile. On those contracts:

- reads by path start serving stored resources, where they were generated knowing nothing stored;
- `unknown_ids: 404` starts answering `404` where it generated;
- webhook and `on_state_change` trigger behaviour follows the identity.

Stored bundles take this the way they take action routes, with the same steps: upload the contract again, or restart a mock from the config file, changing a byte of the contract if the control store is persistent.

### A resource answers only for its owner

On a route that names its target after another path parameter, such as `/repos/{owner}/{repo}` or `/orgs/{org}/teams/{slug}`, a read must now be able to tell a stored resource is the route owner's, unless the parameter is named as an id (`order_id`). It can when the resource was listed or written under the same route template and collection path. Otherwise the resource must show the owner: a relative path ending in the owner and the name (`full_name`, `path_with_namespace`), a member tied to the parameter (`namespace`, `owner.login`, `org_id`, `organization`), or a link ending in them. Nothing needs recompiling, and on these routes:

- a resource a search, a mention, a fixture or a session seed stored without showing its owner is no longer served by name. The read is generated, and the response cache repeats it. Give such a fixture a member that shows its owner;
- only a member named for the parameter, `name`, `login`, `slug` or `key`, or a path holding the owner too, identifies a resource by name. Any other string does not;
- `unknown_ids: 404` does not answer a value some owner's resource goes by. A value none goes by is still a 404;
- a write acts unless the resource is known to be another's, by a record, by what it shows or by a link elsewhere. Then a delete leaves it and an update does not change it. Another owner's create can still replace a resource with the same id.

The collection record lives in the node's memory, like path aliases, and ends when the resource changes without it. On another node or after a restart a read relies on what the resource shows, except that a number showing nothing is served, and a resource written through the route under its name, with an `id` of its own, reads back for any owner. A string id no name member holds, such as `txn_1` below `/customers/{customer}`, reads back as before unless it was listed under another collection or a member tied to the parameter names another owner.

## Upgrading to 0.1.0

### Relative paths in the config file follow the file

In a config file, relative `sqlite:` paths in `storage.url` and `storage.state_url`, and relative `server.tls.cert_file`, `server.tls.key_file`, `outbound.ca_file` and `brokers.<name>.ca_file`, now resolve against the config file's directory, like `mocks.<name>.contract` and `proxy.ca_dir` already did. Before, they resolved against the working directory.

Nothing changes when you start Mockingbird from the config file's directory (`mockingbird run`, `serve -c mockingbird.yaml`), which is what `mockingbird init` sets up. If you start it from elsewhere with `-c some/dir/mockingbird.yaml` and `storage.url: sqlite://./mockingbird.db`, it now opens `some/dir/mockingbird.db` instead of `./mockingbird.db`: move the database next to the config, or write an absolute path. `--storage-url`, `MB_STORAGE_URL`, `MB_STATE_URL` and runs without a config file still resolve against the working directory.

## Upgrading to the P7 release

New, all off or neutral by default:

- **Roles** (`server.role`, `--role`, `MB_ROLE`). The default `all` is the previous behaviour. `control` and `data` need Postgres storage and Postgres or Redis state.
- **The bus** (`bus.url`, `MB_BUS_URL`). With Postgres storage, nodes notify each other of changes over `LISTEN/NOTIFY` by default, so every node reloads a changed mock. Nothing to configure.
- **Leader election.** With a shared bus, only the leader reconciles `mockingbird.yaml` and runs maintenance (session expiry, audit and token-usage retention). The result is the same as before; the work is just not repeated on every node.
- **Redis** session state (`storage.state_url: redis://…`). It needs `maxmemory-policy noeviction`.
- **Limits** (`limits:` and per-mock `limits`), **chaos** (per mock and per scenario), the **PII scrubber** (`pii:`), **TLS** termination (`server.tls`) and outbound **CA/proxy** settings (`outbound:`).

Changes that may need attention:

- **Request size cap.** Requests over 1 MiB (`limits.max_request_bytes`) are now answered 413. Raise the limit if your tests send larger bodies.
- **Response size cap.** Generated responses over 1 MiB (`limits.max_response_bytes`) are replaced by faker output.
- **Generation concurrency.** At most 64 LLM generations run at once per node (`limits.max_concurrent_generations`); calls beyond that wait up to their LLM timeout and then fall back to the faker.
- **GraphQL caps.** Operations may select at most 20 root fields, a WebSocket connection may hold at most 16 subscriptions and a mock at most 1000 connections (`protocols.graphql.*`).
- **`--watch` on a data node** is a usage error.
- **Range checks.** `protocols.stream.item_interval` and a scenario's `stream_item_interval` must be at most 30 s, now also through the Admin API.
- **TLS and `auth.external_url`.** With `server.tls`, `auth.external_url` must be `https://`, in every auth mode.

## `mb-ext` 1.0 (extension authors)

`mb-ext` 1.0 freezes the extension API. Code written against the 0.x crate needs these changes:

- **Constructors instead of struct literals** for the types the core builds and hands to you, which are now `#[non_exhaustive]`:
  - `CallInfo::new(mock, operation_id, headers)`;
  - `UsageEvent::new(tenancy, mock, operation_id)` (status 0, empty source, zero latency and tokens; the fields are still public, so assign the rest);
  - `Extensions::default()` plus the builders `with_tenants`, `with_quota`, `with_usage`, `with_secrets`, `with_providers` and `with_admin_hooks`, each replacing one extension:

    ```rust
    let ext = Extensions::default()
        .with_quota(Arc::new(MyQuota::new()))
        .with_usage(Arc::new(MyUsage));
    ```

- **`match` on `ExtError` needs a wildcard arm**: it is `#[non_exhaustive]` and gained no variant you must handle (`SecretNotFound`, `BadSecretRef`, `Other`).
- **New helpers:** `Rejection::new(status, title, detail)` with `.with_retry_after(duration)`, and `Tenancy::new(tenant, workspace)`. `Rejection` and `Tenancy` stay exhaustive, so struct literals keep working.
- **Secrets:** `Extensions::default()` now uses `DefaultSecrets`, which adds `vault:<mount>/<path>#<field>` references when `VAULT_ADDR` is set. A custom `SecretStore` is unaffected.

The full stability policy is in the [`mb-ext` README](../crates/mb-ext/README.md). [`examples/custom-ext`](../examples/custom-ext) is a complete binary with a custom tenant resolver and quota built on 1.0.
