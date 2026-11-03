# Smart import

Not every API comes with a contract. Smart import turns what you do have into a **draft** contract version that you review before it serves anything:

| Input | `kind` | How it is read | What it produces |
| --- | --- | --- | --- |
| Markdown, HTML, PDF (at most 500 pages) or plain text documentation | `markdown`, `html`, `pdf`, `text` | Split into chunks of about 6000 tokens (400 overlapping) along headings; an LLM extracts the operations of each chunk; the results are merged and deduplicated. Needs an LLM profile. | Operations with summaries, parameters, request and response schemas, and examples quoted from the text. |
| A HAR 1.2 recording (browser dev tools, proxies) | `har` | Deterministic: the JSON traffic of the most common origin; static assets and other origins are dropped; ids in paths become parameters (`/pets/42` → `/pets/{petId}`). | Operations with schemas inferred from the recorded bodies and up to two examples each. |
| A Postman v2.0 or v2.1 collection | `postman` | Deterministic: every request, folders as tags, saved responses as examples, `:var` and `{{var}}` path segments as parameters. | As for HAR. |
| An http(s) URL to any of the above | (`url`) | Fetched once (see [URL fetching](#url-fetching)), then read according to its content type. | As above. |
| An http(s) URL to an OpenAPI 3.x or Swagger 2.0 contract (JSON or YAML) | (`url`) | Deterministic: taken as written, like an [embedded definition](#embedded-openapi); its relative `$ref`s are resolved against the URL and fetched with the same guards ([`$ref`s of a fetched contract](#refs-of-a-fetched-contract)). | Operations with confidence 1.0; the source kind is `text`. |
| Up to 20 URLs, or up to 100 Markdown, HTML or text documents | (`urls`, `documents`) | Each source as above, under one token budget; operations of all sources are merged into one draft. A source that cannot be fetched or read is skipped with a `source_skipped` warning. | One draft. |

HAR and Postman imports do not need an LLM. When one is available it only writes missing summaries. Passwords, tokens, API keys, cookies, `Authorization` headers and similar values in recordings are replaced by `<redacted>` before anything is stored. Every input is at most 20 MiB.

Every draft is an OpenAPI 3.1 document compiled by the same importer as an uploaded contract, so a draft is always a valid contract. OpenAPI, GraphQL, protobuf and WSDL files are contracts already: upload them to `…/contracts` as before (an OpenAPI or Swagger contract can also be imported from its URL as a draft, see above; a WSDL split across files with `wsdl:import` or `xs:import` goes up as one archive; see [WSDL known limits](protocols.md#known-limits)).

## Embedded OpenAPI

Many documentation pages carry their own OpenAPI definition: a fenced ```` ```yaml ```` or ```` ```json ```` block, a `<script type="application/json">` blob, or the page data of a ReadMe site (`script#ssr-props`, `script#__NEXT_DATA__`, `data-initial-props`, including definitions stored as JSON strings such as `oasDefinition`). An OpenAPI 3.x definition found there is taken as written: its operations (with every operation of the definition, not only the one the page describes), components and servers go into the draft with confidence 1.0 and the provenance `embedded OpenAPI`, and the block is removed from the text the LLM reads. When the definitions cover every endpoint the page names, no LLM is called (and none is needed). On a page with both, the block wins: the LLM only fills in operations without a block. A block operation that does not compile is skipped with an `invalid_operation` warning; a block that does not parse stays in the text for the LLM. A Swagger 2.0 block is converted to OpenAPI 3.0 first (definitions, parameters, `in: body` and `formData` parameters as request bodies, `consumes` and `produces`, response headers, security definitions, and `host` with `basePath` as the server) and then taken as written the same way.

## How a resource's identity is read

The member a route's record goes by comes from the contract, in this order.

1. **The answer has an `id`**, or a member named by the path's last parameter (`/orders/{number}` → `number`, `{MessageSid}` → `sid`).
2. **A create's answer has a `sid` or a `uuid`.**
3. **A member named for the type**: `user_id` or `userId` for `users`, `category_id` for `categories`. It must be the only such member, and not an object or a list. The type needs a route ending in a parameter, and each such route must have an id-like one (`{id}`, `{userId}`, `{user_id}`): `{handle}` names something else. With no such route (`POST /tokens`) nothing could find the record again.

With none of these a route has no identity, so a create stores nothing and a read is generated each time. That includes an answer in an envelope (`{"user": {...}}`) or an array. Where the contract's names do not give the identity, [`x-mockingbird-resource`](#correcting-it-x-mockingbird-resource) sets it.

A read with no path parameter, of a type no other route has, reads as a type when every array it lists holds what that type's `GET` by path answers: `/users-by-email` beside `/users/{id}`. Another shape beside them leaves it as it was, as does a second type serving the same items.

## How a `POST` under a resource is read

The compiler decides from the contract, not the words of the path, whether `POST <static>/{param}/<static>` adds a record of its own or acts on the resource `{param}` names. When the contract does not decide, it acts on the parent, which is harmless: the answer is served, the parent is shown as context, and the parent changes only through validated effects. OpenAPI 3 and Swagger 2.0 read it the same way, in this order:

1. **No success response declares a body** (`204`, or `200` or `202` with no content, for every 2xx it lists): it acts on the parent and stores nothing, even with a `GET` beside it or under a longer static tail (`/x/{id}/y/z`). A declared `201` does not count, because it says something was made. Such a call sends no webhook unless the contract gives it a payload.
2. **A route below it** (`<path>/{...}`), or **a `GET` on its path that gives back what it answers**: a collection. That is the same schema, or the same identity member, directly or as the items of a list or an envelope (`{"data": [...]}`). A schema with no members on either side cannot be compared and also counts. A `GET` that reads something else does not.
3. **The parent's `GET` answers the same record**: it acts on the parent. With the very schema of that `GET` it is an update, [decided at runtime](#an-update-that-answers-another-record).
4. **Another resource's `GET` by path serves the same record**: a record of that route's type, with its identity (`POST /captures/{id}/refund` under `refunds`, because `GET /refunds/{id}` answers it). If `GET`s of several types serve it, the contract does not say which, and this rule does not apply.
5. **A declared `201`**, among any other successes: a record of its own, under the last segment. With no [identity](#how-a-resources-identity-is-read) it stores nothing.
6. **Anything else**: it acts on the parent and stores nothing.

The same record means the same schema, through a one-member envelope that holds an object (on either side), or a shorter form the other composes with `allOf`. Members in common are not enough: `{id, status}` beside a `Refund` that does not compose it does not match. An envelope with other members beside its record (`{data: User, links: ...}`) is not unwrapped, because nothing but the member's name says which one is the record.

Where a `POST` lists several successes, these rules read the `201`. The mock answers with the lowest success unless the call forces another status, so a `POST` listing an empty `200` and a `201` with a body answers empty and stores nothing.

### An update that answers another record

`POST /comments/{id}/replies` and `POST /orders/{id}/capture` can both answer their parent's schema, and the contract cannot tell them apart. So with a stored parent, the prompt asks the model which the call did, and its answer decides: a non-empty identity that differs from the parent's is a new record of the parent's type, stored under its own id, with the parent untouched. If a record already holds that id, the new record gets a fresh id and a number past those stored, as a create does; if no id can be drawn, the answer is served and nothing else comes of it: no state, no effects and no webhook, with a warning in the log and a rejected entry under `effects` in the request record. A request that names the id itself (`POST /orders/o1/capture {"id": "o2"}`) chooses what gets overwritten. An effect naming the id the answer moved from is dropped. The webhook is the `created` event of that type, also for a contract payload. The parent's identity, or none, updates the parent. A faker's answer, a call with no stored parent, and a status of another schema also update what the path names.

This choice depends on the model. Measured for a reply with a stored parent, a new id came back 8 times out of 8 with Luna and the default model, and at most 2 out of 8 with Lightning, so on a model like that a reply can overwrite its parent. When every call of a route makes a new record, say so with `x-mockingbird-resource: {action: false, type: <the parent's type>}`.

A tail of two or more segments with a body, a `PUT` or `PATCH` on a tail, and two parameters in a row (`/repos/{o}/{r}/merge`) give no such signal and keep what their shape implies. A `POST` whose last segment is a read verb (`/databases/{id}/query`) with a body stays a read.

### Correcting it: `x-mockingbird-resource`

Inference needs no help from a contract. When it is wrong, an OpenAPI 3 or Swagger 2.0 operation can say so:

```yaml
paths:
  /jobs/{id}/sync:
    post:
      x-mockingbird-resource:
        action: true    # acts on what its path names; false: it writes a record of its own
        type: batches   # the resource type
        identity: key   # the member that identifies it
```

All three members are optional, and each overrides what inference decides for that operation and no other: an action and the read beside it are corrected one by one.

- `action: true` needs a path ending `<static>/{param}/<static>...` and a method other than `GET`, `HEAD` or `OPTIONS`. It takes any tail, also on `PUT` and `PATCH`, and it beats a read verb in the last segment.
- `action: false` makes a `POST` a write of its own, also when its last segment is a read verb. On anything else (another method, a stream, a `POST` ending in a parameter) it is ignored with a warning.
- `type` is an identifier-like name: letters, digits, `_`, `-` and `.`, starting with a letter, digit or `_`. On a `GET`, rule 4 stores records under it. It does not change the webhook event type of a route ending in a parameter, which is named after its last static segment.
- `identity` must be a member of what the operation answers, in its `201` or its lowest success, or for an action of what the parent's `GET` answers. An answer that declares no members cannot contradict it. It sets that operation's identity field after inference, so it does not decide whether that route is an action or a record. On a `GET`, what is inferred from that `GET` takes it: an action on that resource, and a record that `GET` serves.

A value that is not valid gives an `invalid_resource_extension` warning, and inference stands for that member: the extension's other valid members still apply. That covers a value that is not an object, a member of the wrong type, an unknown member, an impossible `action`, and an `identity` the operation does not declare.

## Merge or replace

An import **merges** into the mock's newest draft or active version when it has one: new operations are added, an operation with the same method and path (parameter names aside) is replaced, and nothing is removed. Base components win over incoming ones with the same name (a differing one gives a `conflicting_component` warning), and an incoming operation id already used by a kept operation gets a `_2` suffix (the original goes in `x-original-operation-id`). Re-importing the same documentation therefore gives the same operations and ids. `mode: replace` makes the draft hold only what the import found. When the base is not an OpenAPI 3 document (a GraphQL or protobuf mock, say), a default import is stored unmerged with a `merge_skipped` warning, while an explicit `mode: merge` fails the job. Two imports running at once each merge into the same base; the later draft does not include the earlier one's operations.

## Draft quality

- Object schemas get `required`: the properties present in every example, or, without examples, those the documentation marks required.
- A property is nullable (`"type": ["string", "null"]`) when an example shows it as `null` or the documentation says it may be null or is optional. This applies to schemas the LLM wrote as well as inferred ones.
- The documentation's own operation ids are kept; an id that is not a valid identifier is converted (`movie-details` → `movie_details`, camel case kept) and the original stays in `x-original-operation-id`.
- A `default` the documentation does not state verbatim is dropped (`invented_default`).
- Operations below 0.4 confidence are left out, each with a `low_confidence` warning saying why.

## Review flow

1. **Import.** `POST …/imports` queues a job; a pool of in-process workers runs it (`queued` → `running` → `done`, `failed` or `canceled`), reporting the phase (`fetching`, `extracting`, `analyzing`, `summarizing`, `assembling`) and chunk progress. A `done` job names the new contract version in `result_version`, with status `draft`. A draft never serves traffic.
2. **Edit.** `GET …/contracts/{version}/draft` returns the document, each operation with its confidence and provenance, the warnings and a diff against the active version. `PUT …/draft` replaces the document; it must compile, otherwise a `422` lists each problem with a JSON pointer. Edited operations are marked `edited`. `POST …/preview` generates a sample response for one operation without touching any session state (it spends LLM tokens).
3. **Approve.** `POST …/approve` turns the draft into an ordinary version and activates it; it can no longer be edited. With `{"activate": false}` it stays `approved` (the console approves this way and offers activation as its own step).
4. **Activate.** `POST …/activate`, as for any version, swaps the mock to it.

Every step is audited: `import.create` and `import.cancel` (target `import:<mock>/<job>`), `import.complete` (target `contract:<mock>@<version>`, actor kind `system`), `contract.draft_update`, `contract.approve` and `contract.activate`.

## Confidence and provenance

Each operation carries a **confidence** between 0 and 1. Operations seen in HAR or Postman traffic or taken from an embedded OpenAPI block get 1.0 (HAR and Postman 0.8 when a path parameter was inferred from a slug-like segment). Document operations get the LLM's own estimate (0.5 when it gives none), less 0.2 when no response shape was found, and halved when a literal path segment does not occur in the chunk it came from; below 0.4 they are left out of the draft. The console shows it as High, Medium or Low.

**Provenance** says where an operation came from: up to three entries, each with a location (the chunk and heading, or the HAR entry / Postman item) and an excerpt of at most 300 characters. For documents the excerpt is quoted from the source text, so you can check each operation against the documentation.

## Warnings

A draft lists what the importer was unsure about. Warnings never fail a job; check them before you approve.

| Code | Meaning |
| --- | --- |
| `chunk_skipped` | The LLM call for a chunk failed or answered something unusable; that part of the document contributed nothing. |
| `invalid_operation` | The LLM proposed operations without a valid method or path, or an operation (also one from an embedded block) kept the draft from compiling; they were dropped. |
| `invalid_schema` | A schema the LLM proposed does not compile; it was dropped and a schema inferred from the examples was used. |
| `ungrounded_operation` | A path segment of an operation does not occur in the text it supposedly came from; its confidence was halved. Check that it exists. |
| `token_budget_exhausted` | The job's token budget ran out: the draft is **partial** and covers only the chunks processed so far. |
| `conflicting_schemas` | Several sources describe the same operation differently; the richer schema was kept. |
| `no_responses` | No response was described; a bare `200` was added. |
| `missing_path_parameter` | A `{param}` in the path was not described; it was added as a string. |
| `entries_skipped` | HAR entries (static assets, non-JSON traffic, unreadable entries) or Postman items without a usable method or URL were skipped. |
| `other_origins` | The HAR file recorded several origins; only the most common one was imported. |
| `summaries_skipped` | Summaries for HAR or Postman operations could not be generated. |
| `low_confidence` | An operation's confidence was below 0.4, so it was left out; the message gives the reason. |
| `invented_default` | `default` values the documentation does not state were dropped. |
| `schema_too_large` | A response schema is larger than the prompt budget (64 KiB of compact JSON); the generation prompt shows a trimmed schema. |
| `source_skipped` | One source of a several-source import could not be fetched or read. |
| `merge_skipped` | The import could not merge into the mock's version (not OpenAPI 3, or the merged draft did not compile); the draft holds only this import. |
| `conflicting_component` | A component of the import differs from the version's component of the same name; the version's was kept. |

## Token budget

The LLM calls of one job may spend at most `import.max_tokens` tokens (default 200 000), counted from the provider's reported usage. Before each chunk the importer estimates its cost (the chunk, the prompt and the profile's `max_tokens`); when the budget cannot cover it, the job stops early and assembles a partial draft with a `token_budget_exhausted` warning instead of failing. Extraction calls use the mock's LLM profile (or `import.llm_profile`) with a timeout of at least 120 s and at least 8192 output tokens, at temperature 0 and in JSON-object mode (the schema is in the prompt). Document text is passed to the model only as delimited data, never as instructions; the model has no tools, and every answer is validated before use. With `--offline` there is no LLM, so only HAR and Postman imports work.

Imports and draft previews go through the same extension points as data-plane calls. Their tenant is the one the tenant resolver gives a call of operation `import` (for a preview, `draft`) with the Admin API request's headers, in the request's workspace; the self-hosted resolver gives `default`, and a resolver's refusal is answered as its problem. The import job's LLM calls are billed to the tenant resolved when it was created, which is stored with the job, so a job picked up by another node or after a restart is billed the same way (a job queued before migration `1100_import_job_tenant`, to `default`). Creating an import job is admitted by the quota guard (operation `import`, that tenancy); a refusal is answered with its own status, `429` by default, and `Retry-After` when the guard sets one. Every LLM call an import makes is reported to the usage sink (operation and source `import`, status `502` for a failed call), whether the job finishes, fails or is canceled, and `mb_import_tokens_total` counts the tokens of all of them. Previews are admitted and reported as calls of the previewed operation. `import.llm_profile` is passed through the provider policy like a mock's own profile, for the resolved tenancy; a policy's refusal is answered as its problem, with its status and `Retry-After`.

## Configuration

The `import:` section (workers, the per-job token budget, the extraction profile, the private-URL allowlist and the per-mock job limit) is described in the [configuration reference](configuration.md#import). It is read at startup.

## URL fetching

URL imports are fetched by the server, so they are guarded against server-side request forgery:

- Only `http` and `https` URLs with a host and without credentials.
- The host is resolved once and every address is checked before connecting; the connection is pinned to the checked addresses, so the name cannot be re-resolved elsewhere. Blocked: `0.0.0.0/8`, `10.0.0.0/8`, `100.64.0.0/10`, `127.0.0.0/8`, `169.254.0.0/16`, `172.16.0.0/12`, `192.0.0.0/24`, `192.0.2.0/24`, `192.168.0.0/16`, `198.18.0.0/15`, `198.51.100.0/24`, `203.0.113.0/24`, `224.0.0.0/4`, `240.0.0.0/4`, and for IPv6 `::`, `::1`, `fc00::/7`, `fe80::/10`, `fec0::/10`, `ff00::/8` and `2001:db8::/32` (IPv4-mapped and NAT64 addresses are judged by their IPv4 address).
- `import.url_allowlist` exempts a host name, a host suffix (`*.corp.example`) or an address range. A blocked fetch fails with a generic "destination not allowed" message that names neither the host nor the address it resolved to, so URL imports cannot probe internal DNS; the server logs both at `warn` level.
- Query values and the fragment are redacted (`?sig=REDACTED`) in everything stored or audited: the job's `source_name`, the draft's `source_name` and `source_url`, and the `import.create` audit event. The real URL is used only for the fetch.
- At most 5 redirects, and each hop is validated and resolved again, exactly like the first request.
- No proxy is used (proxy environment variables are ignored), the whole fetch takes at most 20 s and the body is at most 10 MiB.
- The content type picks the reader: `text/html`, `text/markdown`, `text/plain`, `application/pdf`, or JSON that is a HAR file or a Postman collection. A body served as anything but HTML, Markdown or PDF that is an OpenAPI 3.x or Swagger 2.0 contract (JSON or YAML) is read as a contract instead; other contracts (AsyncAPI, GraphQL, protobuf, WSDL) are refused and go to `…/contracts` as uploads.

### `$ref`s of a fetched contract

A contract fetched from a URL may be split into several documents (`$ref: ./schemas/pet.yaml`, `$ref: ../common.yaml#/Error`). Its `$ref`s are resolved against the URL of the document holding them (after redirects), the documents they name are fetched, and the contract is bundled into one document exactly as a [multi-file upload](cli.md#archive-uploads) is: schemas become named definitions (recursive ones included), other objects are copied in place.

- Every referenced document is fetched through the same guards as the contract: the address checks and `import.url_allowlist`, the redirect checks, at most 10 MiB and 20 s per document. A ref to a blocked host fails the import with the same "destination not allowed" message.
- Only refs of the contract's own scheme are followed (`http` or `https`, including absolute and `//host/…` refs to other hosts, which are checked like any other URL); `file:`, `ftp:` and every other scheme, and a switch between `http` and `https`, fail the import, naming the ref.
- Each document is fetched once, so cyclic refs end. At most 64 documents (`mb_import::smart::MAX_REF_DOCUMENTS`), at most 16 documents deep (`MAX_REF_DEPTH`), at most 20 MiB with the contract, and at most 20 s for all of them together; the bundled contract is capped at 32 MiB, as for uploads. Passing a limit fails the import, before the documents past it are fetched.
- Errors name the ref and the document holding it, with query values redacted.

## Admin API

| Endpoint | Role | Result |
| --- | --- | --- |
| `POST /mocks/{mock}/imports` | Edit | `202` with the job and a `Location` header. `409 managed_by_file` for a mock from the config file; `422` for an unknown kind, a contract, or a document import without an LLM; `429` when the mock already has `import.max_active_jobs_per_mock` queued or running jobs. |
| `GET /mocks/{mock}/imports`, `GET …/imports/{job}` | Read | The newest 50 jobs; one job. Finished jobs are kept for 7 days. |
| `DELETE …/imports/{job}` | Edit | Cancels a queued or running job; `409` once it has finished. |
| `GET …/contracts/{version}/draft`, `PUT …/draft` | Read, Edit | The draft for review; replace its document (`409` unless the version is a draft). |
| `POST …/contracts/{version}/approve` | Edit | The approved version, activated unless the body is `{"activate": false}` (the body may be empty or `{}`). |
| `POST …/contracts/{version}/preview` | Edit | A generated response for one operation. |

The JSON body takes exactly one of `url`; `urls` (1 to 20 URLs); `kind` with `text` (`markdown`, `html`, `text`, `har`, `postman`); `content_base64` with an optional `kind` and `filename`; or `documents` (1 to 100 of `{filename?, kind?, text}`, Markdown, HTML or text, kind from `kind` or the file name, at most 20 MiB in all). `mode` is `merge` or `replace` (absent: merge when the mock has a draft or active version). The draft's `meta` names its `sources` (several-source imports) and the version it was `merged_from`. A file can also be uploaded as `multipart/form-data` with a `file` part and an optional `kind` part; the request must carry an `X-Mockingbird-Upload` header (any value), which a cross-site form cannot send. The request body is at most 28 MiB.

```sh
ADMIN=http://127.0.0.1:9090/api/v1/workspaces/default/mocks/pets
# Upload a file.
curl -X POST "$ADMIN/imports" -H 'X-Mockingbird-Upload: 1' -F file=@petstore.har
# Or send JSON.
curl -X POST "$ADMIN/imports" -H 'content-type: application/json' \
  -d '{"url": "https://example.com/docs"}'
curl -X POST "$ADMIN/imports" -H 'content-type: application/json' \
  -d "$(jq -Rs '{kind: "markdown", text: .}' petstore.md)"
# Several pages into one draft, replacing the current contract.
curl -X POST "$ADMIN/imports" -H 'content-type: application/json' \
  -d '{"urls": ["https://example.com/docs/pets", "https://example.com/docs/owners"], "mode": "replace"}'
# Follow the job, then review the draft it names.
curl "$ADMIN/imports/<job id>"
curl "$ADMIN/contracts/<version>/draft"
```

## CLI

`mockingbird import` recognises documentation, HAR files and Postman collections (by content, and by file name for Markdown, HTML and text), http(s) URLs (several at once), and folders holding only `.md`, `.markdown`, `.html`, `.htm` and `.txt` files (imported as documentation, one source per file; any other folder uploads as an archive). It creates the mock if needed, waits for the job (`--timeout`, default 10 minutes) and prints the draft's operations and warnings:

```sh
mockingbird import petstore.har --as pets
mockingbird import https://example.com/docs/pets https://example.com/docs/owners --as pets
mockingbird import ./docs --as pets --mode replace
# Approve (and activate) in one go, but only if the draft has no warnings.
mockingbird import petstore.har --as pets --approve
```

With `--approve`, a draft with warnings is left unapproved: the command prints the summary, says how many warnings there are and exits 1, so you can review it in the console and then approve it with `mockingbird drafts approve <mock> [<version>]` (`--no-activate` to approve without serving). An approved draft is activated unless `--no-activate`; `--activate` is still accepted with `--approve`. `--mode merge|replace` picks the [merge mode](#merge-or-replace).

## Console

A mock's **Import** tab starts an import from a file, a URL or pasted text, and follows the job's progress. The **draft review** page (opened from a draft on the Contracts tab, or when an import finishes) lists the operations with their confidence and provenance, edits an operation, previews a response, shows the diff against the active version and the warnings, and approves and activates the draft.
