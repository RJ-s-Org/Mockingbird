# World descriptions

A **world description** says how a contract behaves as a world: which member holds a record's id, where a request or an answer holds the record, what a delete does, which members must be unique, how a list is scoped, filtered, ordered and paged. Every contract version has one. Mockingbird's import heuristics write it automatically, and your coding assistant (or you) can improve it from the API's documentation. Mockingbird validates every entry against the contract before it applies.

A description is optional: a contract alone still works. It is not a dataset and holds no records, no values from traffic and no credentials.

The mock answers from the description wherever it says something and keeps its heuristics wherever it is silent, key by key; a write returns once the running mock serves the new revision ([At runtime](#at-runtime)).

## Who writes it

| Tier | Author | When |
| --- | --- | --- |
| 0 | `heuristic` | Always, from what the importer already puts in the compiled contract: each operation's kind, type and target, where its answer holds the record, each type's identity member. |
| 1 | `agent` | Your coding assistant, through MCP or the CLI. The `mockingbird-world` skill tells it how, and the `mockingbird-mock-api` skill tells it to review the description after every import. |
| 2 | `llm` | Mockingbird's configured LLM, when asked (planned). |
| 3 | `user` | A person, through the CLI, the Admin API or a config file's `world:` file. |

A later tier overrides an earlier one entry by entry. `get_world` reports the author of each entry a revision set; every entry it does not list is `heuristic`.

## Reading it

```sh
mockingbird world show orders              # the active version: gaps first, then each entry
mockingbird world show orders --version 2 --json
mockingbird world show orders --operation getOrder --operation listOrders   # only these, with their gaps
mockingbird world show orders --type order # the type, its operations, and their gaps
mockingbird world show orders --called     # only what the request log names, with its gaps
mockingbird world schema                   # the format, as JSON Schema
```

On a large contract, narrow the answer to the operations an app uses: `--operation <id>` and `--type <resource type>` (each repeatable; `operations` and `types` arrays on `get_world`; `?operation=` and `?type=` on the Admin API) keep only the operations named, the types named and the operations of those types, and the gaps, refused entries and authors about them. Gaps and refusals about the description as a whole are kept. A name the version does not have gives an empty answer with `warnings` listing it, not an error.

Once the app has run against the mock, `--called` (`called: true` on `get_world`, `?called=true` on the Admin API) narrows the answer to what it used: the operations the mock's request log names (any session; a GraphQL request names each root field it answered), the types those operations touch, the gaps naming one of those operations, naming no operation and one of those types, or skipping a route of one of those types (a skipped route is never served, so the log cannot name it), and the refused entries and authors about them. Gaps and refusals about the description as a whole are kept. The other gaps are left out and counted in `gaps_left_out`. A log that names no operation of the version gives an empty answer with a warning. It combines with `--operation` and `--type`. On a contract with hundreds of gaps, run the app's flows first, then review this list; the `next_steps` of an import or `start_server` answer says so when a version has more than 40 gaps. The request log keeps the newest `observability.request_log_retention` calls per mock, so an operation called only before them no longer counts.

The MCP tool is `get_world {mock, version?, operations?, types?, called?}`, and `get_world_schema` answers the format's JSON Schema as `mockingbird world schema` prints it; the Admin API is `GET /api/v1/workspaces/{ws}/mocks/{mock}/contracts/{version}/world`, with `version` a number or `active`. The answer:

| Field | What it holds |
| --- | --- |
| `version` | The contract version it describes. |
| `world` | The effective description: tier 0 overlaid with the newest revision's entries that validate. |
| `authors` | The author of each entry the newest revision sets, keyed by JSON pointer (`/types/folder/identity`). |
| `revision` | The newest revision's number; `0` when only the heuristics wrote it (every version stored before world descriptions starts there). |
| `refused` | Entries of the newest revision that did not validate; each falls back to tier 0. |
| `gaps` | What the description leaves open. |
| `warnings` | The `operation` and `type` filters naming nothing in the version, and with `called` a request log naming no operation of it; absent when there is none. |
| `gaps_left_out` | With `called`, how many gaps it left out; absent without it. |

Gaps. The first three codes also mark the recorded calls of each operation they concern (see [Gaps in the request log](#gaps-in-the-request-log)). The last four point at behaviour the contract signals and the description does not state yet. `undescribed_param` is something the executor cannot answer from the records; the three marked `advisory: true` are checks: read the docs for each, and write an entry only when they state the behaviour, otherwise leave the gap.

| Code | Meaning |
| --- | --- |
| `no_identity` | A type that operations name has no `identity` their calls can use (a list needs `identity.member`; another kind can use its own identity or a read by id of its type), so the model answers those calls. |
| `no_target` | A read, update, replace, delete or action does not say what names its record. |
| `no_record` | A create or update answer in which no member fits the type, and no `answer.record` says where it is. |
| `skipped_route` | An HTTP route of the contract that is not served (it matches the same requests as an earlier one, or is not a valid route pattern). |
| `refused` | An entry refused at validation. |
| `undescribed_param` | A list's query parameters the executor reads nothing for: no described filter, scope, paging, `fields` or `ignores` parameter, no key bracketed under a `deep_object` filter, no soft delete's `listed_with`, no name-matched filter on a member of the items and, with no described paging, no page size. A request that sends one with a value that asks something is answered by the model, not from the records. A sort parameter has no description yet. |
| `no_effect` (advisory) | An operation of kind `action` with no `set`, `transition` or `queue`. An update whose route only looks like an action is merged as an update and is not one. |
| `no_unique` (advisory) | A type whose create can answer `409` and that has no `unique`. |
| `no_soft_delete` (advisory) | A type a delete removes whose record has a member such as `deleted`, `is_trashed` or `deleted_at`, with no `lifecycle.soft_delete`. |

## Writing it

A write sends an **overlay**: only the entries you want to set. The effective description is computed on every read as tier 0 overlaid with the newest revision, entry by entry; a type's `identity` and an operation's `answer` merge field by field, so setting `answer.identity_header` keeps tier 0's `answer.record`. An operation's `target` replaces tier 0's whole: a member you leave out is not inherited, so `target: {member: key}` drops the `path_param` tier 0 took, and `target: {}` names nothing. An overlay that makes an operation a `create` without a `target` drops the `path_param` tier 0 took when the route does not name the new record by it: a `POST`'s path parameter names the collection or parent it posts to (RFC 9110). A `PUT` whose last path segment is that parameter keeps it, since a `PUT`'s URI is the record itself (`PUT /buckets/{name}` stores bucket `name`).

| MCP tool | CLI | What it does |
| --- | --- | --- |
| `put_world {mock, version?, world, author?, expect_revision?}` | `world put` | Replaces the overlay. |
| `patch_world {mock, version?, patch, author?, expect_revision?}` | `world patch` | Merges a JSON merge patch (RFC 7396) into the overlay: `null` removes a key, arrays are replaced whole. |
| `list_world_revisions {mock, version?}` | `world revisions` | Lists the revisions. |
| `get_world_revision {mock, version?, revision}` | `world revision <mock> <r>` | One revision with its overlay as stored. |
| `restore_world_revision {mock, version?, revision}` | `world restore` | Adds a new revision holding an older one's document, validated again. |
| `diff_world {mock, version?, from, to}` | `world diff` | The JSON pointers added, removed and changed between two revisions. |

The Admin API takes `PUT` and `PATCH` (`application/merge-patch+json`) on `…/contracts/{version}/world`, `GET …/world/revisions`, `GET …/world/revisions/{r}`, `POST …/world/revisions/{r}/restore` and `GET …/world/diff?from=a&to=b`.

`author` is `agent`, `llm` or `user` (the default); `heuristic` is refused.

A write that is accepted answers `{revision, applied, refused}`: the entries that validated are stored as the new revision, and each refused entry is reported with its JSON pointer, a code and a message, and is not stored.

| Entry refusal | Meaning |
| --- | --- |
| `unknown_key` | The format has no such key at that place. Unknown keys are refused, not ignored. |
| `unresolved` | It names an operation, type, member, parameter, header, status or schema location the contract does not have. |
| `not_applicable` | It does not fit the operation's protocol, e.g. a header or a status on a GraphQL, gRPC or SOAP operation. |
| `invalid` | The entry is malformed: a pattern without exactly one `{id}`, a state not among the states, a `from_request` pointer that does not parse, a pointer that does not reach into the schema. |

Some documents are refused whole: the write fails with an `application/problem+json` answer whose `code` names the reason and whose `path` names the first entry at fault, and the server stores nothing.

| Status | `code` | When |
| --- | --- | --- |
| 413 | `too_large` | The document is over 256 KiB. Describe structure, never data. |
| 422 | `invalid` | The document is nested deeper than 32, has the wrong shape (a string where an object belongs, an unknown `kind`), or has a `request` or `answer` pointer that does not parse. |

### Revisions

Each write adds a numbered revision (from 1) attached to its contract version, with its author. The newest revision applies once the mock has rebuilt on it, which the write waits for (`runtime` in its answer). A change never recompiles the contract or adds a contract version.

Two writers can meet. Send the revision you read as `expect_revision` (the `If-Match` header in the Admin API). If someone wrote since, the write is refused with `412 revision_conflict`: read again and redo the change. Without it, the write adds a revision on top of the newest.

### A new contract version

A new contract version (an upload, a config-file change, an approved import draft) starts from the previous version's newest overlay. Entries that still validate carry forward with the author that set them. Entries that no longer resolve (a member, operation or status the new contract dropped) are listed under `refused` with code `unresolved` and as gaps; everything else applies, and reads never fail.

## The format

YAML or JSON, `version: 1`. Every key is optional; an absent key falls back to the heuristics. `get_world_schema` (MCP) and `mockingbird world schema` give the full JSON Schema.

**Types** (`types.<resource type>`):

| Key | Shape | Meaning |
| --- | --- | --- |
| `identity` | `{member, pattern?, alternates?}` | The member holding the id; `pattern` (one `{id}`) when the id is embedded in a name; `alternates`, other members a path value may name the record by (an alias next to the id), `[]` for none. Described alternates are tried after the members the runtime guesses from a path parameter's name or description (`templateIdOrAlias`); they add to that guess but cannot remove it. An alternate may also be a member a create of the type answers beside the record that the record itself does not declare: a value the server issues for it (Plaid's `access_token` for an Item). `member` may be left out of an overlay that sets only `pattern` or `alternates` on a type that already has one; elsewhere that identity is refused. |
| `parent` | `{type, member?, path_param?, pattern?, embedded?}` | The parent record and where it is named. `embedded`: the parent's array member the type's records live in, when they are held inside their parent rather than on their own (an incident's `incident_updates`): a read, update or delete of one by its own route acts on that item of the held parent ([Embedded records](#embedded-records)). |
| `lifecycle` | `{member, states?, initial?, soft_delete?}` | The status member and its states; `soft_delete: {state, listed_with?: {param, value}, unless?: {param, value}}` for a delete that only moves the record to a state, the list parameter that still shows it, and the delete's query parameter that removes it instead (`unless: {param: force, value: true}`). |
| `unique` | `[{members, scope?, conflict?}]` | Member sets that must be unique, optionally within a member's scope, with the declared conflict status. |
| `concurrency` | `{member?, header?, conflict?}` | The body member and/or header holding the version or ETag, and the declared conflict status. |
| `same_as` | `[<type name>]` | The names this type goes by in the other mocks of a `shares` group, where they differ beyond case and a plural `s` (REST `pulls` beside GraphQL `pullrequest`). They name another contract's stored types as written (`check-runs`, not `checkruns`), so only their form is checked. Without it, the type's names in the group are inferred from the contracts (a short form with the same identity members, see [`mocks.<name>.shares`](configuration.md#mocks)); a `same_as` replaces that inference for the type in this mock's reads (the other mocks still infer theirs), so it also tells a mock two similar names are not the same type. |
| `computed` | `{<member>: <expression>}` | Members computed whenever a record of the type is answered (a read, a list, a write's answer), each the value of an [expression](#expressions) over the record, and never stored: `line_count: "count('lines', it.order_id == record.id)"`. `derived` (`{kind: count, of}` or `{kind: sum, of, member, where?}`), which `computed` replaces, still loads ([below](#expressions)). |

**Operations** (`operations.<operation id>`):

| Key | Shape | Meaning |
| --- | --- | --- |
| `kind` | `create`, `read`, `list`, `update`, `replace`, `delete`, `action` | What the operation does. |
| `type` | string | Its resource type. |
| `target` | `{path_param}` or `{member}` | What names the record it acts on. A dotted `member` is a member inside an object member of the request (`input.issueId` of a GraphQL mutation's input object). A `POST` create's record is named by the request member when the body holds it, before any path parameter (which names where the record goes, such as its parent). A `PUT` create whose last path segment is its `path_param` is named by that parameter. |
| `request`, `answer` | `{record}` or `{records}` (JSON pointers) | Where the body holds the record or records: nested envelopes, one-item arrays, bulk arrays. `answer.record: ""` (and `request.record: ""`) says the answer (the request body) is itself the record. `answer.long_running: {done, result}` for long-running operations. `answer.identity_header`: the 2xx response header a create names its id in (a declared header name, HTTP only), or `false` for none. |
| `request.instructions` | `{at, kind, kinds: {<kind>: {changes}}}` | An update whose request sends instructions instead of a record (a semantic patch): `at`, the request body's array of instructions (in any media type the operation declares); `kind`, the member naming an instruction's kind; each kind's `changes`, `{<member>: {value} \| {from_instruction: <pointer>} \| {from_request: <pointer>} \| {append: <pointer>} \| {remove: <pointer>}}`, `append` and `remove` adding or taking out the instruction's value (each item of an array) in an array member. A braced step (`environments.{environmentKey}.on`) is the value of that member of the instruction, else of the request body. |
| `transition` | `{<member>: <value>}` | Members an action sets; the type needs a `lifecycle`. |
| `set` | `{<member>: <expression>}` | Members of the record a create, update, replace or action stores, each the value of an [expression](#expressions) (`request.body.name`, `held('lists', request.body.list_id).currency`, `'k_' + lower(request.body.name)`). A dotted member sets a member inside an object. |
| `refuse` | `[{when: <expression>, status, code?, message?}]` | Refusals checked in order before anything is stored or generated: the first whose `when` is `true` answers its `status` (one the operation declares, by that code or, for a 4xx or 5xx, by a `default` response). `code` and `message` are text, or `{expr: <expression>}`. |
| `effects` | `[{op: create \| update \| delete, type, id?, set?, when?}]` | Writes to other records after a create, update, replace, delete or action stored its target, in order, at most 8: `create` a record of `type` holding `set`, `update` the held record `id` names with `set`, `delete` it; each only when `when` is `true`. Values are [expressions](#expressions). |
| `scope` | `{param, member?, kind?, recursive?}` | The parameter that limits a list to one scope. With no `member`, `param` is a value a create of the items' `parent` type issues (an issued alternate of the parent, Plaid's `access_token` for an Item's accounts): the list holds that parent's records, and every list of the type scoped so (`/accounts/get`, `/auth/get`) shares them; the first one generates and the rest answer from state; a value no parent was issued answers the list's `not_found`. `kind`: `eq` (the default), the member holds the parameter's value; `prefix`, the member is a `/`-separated path directly under the parameter's (`/a/b` under `/a`; any one-step path under a blank or `/` root), or anywhere below it when the `recursive` parameter is `true`; a member value holding no upper-case letter (`path_lower`) is compared with the path in lower case. |
| `filters` | `[{param, member?, kind}]` or `[{param, grammar, member_prefix?}]` | `kind`: `eq`, `contains`, `gte`, `lte`, `flag`, `text`. `grammar`, for a parameter whose value is a filter expression: `scim`, `odata`, `field_value`, `deep_object` ([Filter grammars](#filter-grammars)); its attribute paths are member paths, below `member_prefix` when set. |
| `order` | `{member, descending?}` | The order of a list. |
| `list` | `{where?, sort?: {param, by}}` | A list's [expressions](#expressions): `where`, the default item filter (`it` each record as stored); `sort`, a sort parameter whose `by` turns the value sent into `[{member, desc?}]`. |
| `paging` | `{style, request?, response?, response_header?, size?}` | `style`: `cursor`, `page`, `offset`, `next_url`, `header_cursor`. A `response_header` named `Link` carries an RFC 8288 link to the next page's URL, the cursor or position under `request` (`<https://…/bookmarks?limit=1&max_id=…>; rel="next"`), and no `next` link on the last page, nor where no URL can be made (off HTTP, or a query holding an object); any other `response_header` carries the next page's URL with `next_url`, else the bare cursor or position. |
| `ignores` | `[param]` | Request parameters that ask nothing of a list's items (an API version, a detail level, an include switch): a list from state reads past them. One the list's `filters`, `scope` or `paging` reads, or named as an item member, is refused. A `deep_object` filter's `param` is a query parameter, never a body pointer. |
| `idempotency` | `{header}` | The header that makes a replay answer as the first call did. |
| `not_found` | status | The declared status of a missing record (a `default` response declares any 4xx or 5xx). |
| `fields` | `{param, separator?, always?, default?}` | A read's or list's sparse fieldset query parameter (`fields=id,name`, `$select=name`): a record from state, or each listed item, answers only the members it names (split on `separator`, `,` by default; `a/b` or `a.b` a member inside an object), plus the `always` members (`[id]`). A request not sending it answers the `default` members (`[]`: the `always` ones alone), or the whole record when `default` is absent. `*`, or a value in a grammar of its own (`items(id)`), answers the whole record; a named member the record does not hold is left out. A list takes the parameter as described, never as an undescribed one. |
| `queue` | `{effect: append \| take, count?, requeue?, redelivered?}` | An action's messages on its target, first in first out ([Message queues](#message-queues)). |

A list's `param` (of `scope`, `filters`, `paging.request`, `paging.size`, `ignores`) names a path or query parameter, or a member of the request body: a JSON pointer (`/q`, `/ActiveList/Pagination/PageNumber`), or a plain name the body declares and the path and query do not (`limit` of a `POST` search).

The description is keyed by operation id, so GraphQL, gRPC, SOAP and AsyncAPI contracts share the format. On those, HTTP-only keys (headers, statuses) are refused as `not_applicable`, and there are no skipped routes.

### Example

```yaml
version: 1
types:
  folder:
    identity: { member: name, pattern: "folders/{id}" }   # id embedded in a resource name
    parent: { type: folder, member: parent, pattern: "folders/{id}" }
    lifecycle:
      member: state
      initial: ACTIVE
      soft_delete: { state: DELETE_REQUESTED, listed_with: { param: showDeleted, value: true } }
    unique: [{ members: [displayName], scope: parent, conflict: 409 }]
    concurrency: { member: etag, header: If-Match, conflict: 412 }
operations:
  CreateFolder:
    kind: create
    type: folder
    answer: { long_running: { done: done, result: response } }
  MoveFolder:              # POST /v3/{name}:move
    kind: action
    type: folder
    target: { path_param: name }
    set: { parent: "request.body.destinationParent" }
  DeleteFolder:
    kind: action
    type: folder
    transition: { state: DELETE_REQUESTED }
  ListFolders:
    kind: list
    type: folder
    scope: { param: parent, member: parent }
    order: { member: displayName }
    paging: { style: cursor, request: pageToken, response: nextPageToken, size: pageSize }
    # no filter on showDeleted: soft_delete.listed_with already reads it
  PutContacts:             # PUT /Contacts with {Contacts: [...]}
    kind: create
    type: contact
    request: { records: /Contacts }
    answer: { records: /Contacts }
```

### Expressions

`set`, `refuse` (and its `code` and `message`), `computed`, `effects` and a list's `list` (`where` and `sort.by`) take expressions in a subset of [CEL](https://cel.dev) (the Common Expression Language Kubernetes admission policies use): literals (numbers, text in `'…'` or `"…"`, `true`, `false`, `null`, lists `[…]`, maps `{'k': v}`), members (`a.b`, `a['b-c']`, `a[0]`), `?:`, `&&`, `||`, `!`, comparisons, `+ - * / %`, `in`, `has(a.b)`, and the macros `list.all(x, p)`, `list.exists(x, p)`, `list.map(x, e)`, `list.filter(x, p)`. Text is joined with `+`; there is no other template syntax. An expression is compiled once, when the world is loaded, and evaluated with a step budget.

| Name | Value |
| --- | --- |
| `request` | `{body, path, query, headers}`: the parsed body, the path and query parameters, and the request headers, their names in lower case (`request.headers['x-actor']`). Credential headers (API keys, tokens, `Authorization`) are there too, so a `refuse` can turn away an unknown key; they still never reach the model, the request record or the response cache key, and a failing expression's note never quotes a value. A value an expression copies (into `set`, `computed` or a refusal's `message`) is not withheld. |
| `record` | In `set` and `refuse`, the target as stored before the call, or `null` (a target the session does not hold, a create's `refuse`). In a create's `set`, the record as built: the request with the identity the executor minted, or the one an identity entry of the same `set` gives (evaluated first), so `'https://x/' + record.id` reads the new record's id. In `computed`, the record answered, as stored (a list item cut down to the list's item schema still reads the whole record). In `effects`, the target as the call's write left it (as it was held, for a delete). |
| `it` | In `effects`' `when` and `set`, the record an `update` or `delete` found (`null` for a `create`); in `list.where`, each record of the list as stored; inside `count`, `exists` and `sum`, each item. The list macros name their own item: `record.items.map(x, x.price)`. |
| `value` | In `list.sort.by`, the sort parameter as the request sent it (a deep object parameter, `sort[0][field]=x`, as the object its keys make). |
| `now` | The request time, RFC 3339 text: the seeded time when the call's draws are seeded (as generated dates are), else the clock. |

| Function | What it does |
| --- | --- |
| `held(type, id)` | The session's record of `type` stored under that id (in its identity pattern's forms too), however many the type holds, else the one record among its first 500 whose identity member or described alternate holds it, else `null`: `held('payment_intents', request.body.payment_intent).currency`. |
| `count(type, cond)`, `exists(type, cond)` | How many of the session's records of `type` (at most 500) `cond` holds for, with `it` bound to each: `exists('incidents', it.key == request.body.key && it.status != 'resolved')`. |
| `sum(list, expr)` | The sum of `expr` over `list`, with `it` bound to each item. |
| `seq(scope)` | A counter per session and scope, from 1: `seq('incident')`, `seq('pr:' + request.path.repo)`. The counters are the session's state: a reset clears them, a snapshot carries them, and a world patch or a restart does not start them again. They are kept as one record of type `__mockingbird.seq`, which counts among the session's resources and shows in its exports and change feed like any other. |
| `mint(pattern)` | A fresh id matching a regular expression, drawn from the seeded minting: `mint('[0-9]{12}')`. |
| `md5`, `sha1`, `sha256`, `lower`, `upper`, `slug`, `size`, `string`, `int`, `double`, `startsWith`, `endsWith`, `contains`, `matches`, `trim` | Text and conversion helpers: `md5(lower(request.body.email))`. |
| `split(s, sep)` | The parts of `s` between each `sep`, as a list (each character for an empty `sep`); more than 1000 parts is an error. |
| `substring(s, start, end?)` | The characters of `s` from `start` up to `end` (the end when left out), counted in characters and clamped to the text: `substring('-price', 1)` is `price`. |
| `replace(s, from, to)` | `s` with every `from` replaced by `to`; what it would build is charged to the step budget before it is built. |

Every function is also a method of its first argument: `request.query.sort.split(',')`, `p.substring(1)`.

Deliberate differences from CEL:

- A member of `null`, a missing member and an index past the end of a list are `null`, not errors; `has(a.b)` tells whether one is there. A `set` entry whose value is `null` sets nothing on an update, replace or action; on a create, which has nothing to leave as it was, it sets `null` where the type's reads give the member a schema that admits it (`closed_at: null`), and nothing elsewhere.
- A number is an integer when its value is integral (`2` and `2.0` alike). Integer with integer stays an integer (overflow is an error), any fraction makes a double, and a `/` that does not divide evenly gives a double. Integers and doubles compare with each other, and `null == null`.
- The type of `held`, `count` and `exists` is a literal resource type name.
- A value is written as its member's schema declares it: an integral number into an `integer` member as an integer, a number into a `string` member as its text, text holding a number into a `number` or `integer` member as that number, `'true'`/`'false'` into a `boolean` member. An object or list gets each required member its schema fixes with a `const` and the value leaves out, at every level: a GraphQL object's `__typename`, so `{'edges': [{'node': {'id': 'l1'}}]}` needs no typenames. A value where the schema offers a choice of several shapes (a union) is written as it is.

Validation refuses an expression that does not compile, with its line and column, and one that reads what the contract does not declare: a `request.body` member the request body does not have, a `request.query`, `request.path` or `request.headers` name the operation does not declare as a parameter of that place, a member of `record` or `it` that the records of the operation's type (or the type named in `held`, `count`, `exists`, or an effect's `type` for its `it`) do not have, and a type that is not a resource type. A `set` member must be a member of the type's records, and only a create, update, replace or action takes `set`. A `computed` member must be a member of the type's records; its `request` is whichever call answers the record, so its members are not checked against one operation, `it` is not bound, it calls neither `seq` nor `mint`, which would change on every answer, and it is never the type's identity member, which would rename the record in its answers. An operation takes at most 8 `effects`, and only a create, update, replace, delete or action takes them; an effect's `type` must be a resource type, not one held inside its parent (`parent.embedded`), and each `set` member a member of its records, an `update` or `delete` names its record in `id` and a `create` does not (`it` is not bound in `id`, nor anywhere in a `create`), and a `delete` sets nothing. An expression that fails when it runs (a type mismatch, its step budget) never fails the call: the value stays as the engine would make it (fill writes a create's member), a `refuse` entry whose `when` fails does not fire, and the request record says so under `expr_failed` ([request log](#gaps-in-the-request-log)).

**`set`.** Applied to the record a create, update, replace or action stores, after the request is merged and before the record is checked against the type's reads; on a create the model and the faker never fill a member `set` names, one that sets the identity member names the record by it (`id: md5(lower(request.body.email))`), and an alternate key it sets is refused with the create's declared conflict when another record holds the value, as a sent one is. A create the executor leaves to generation (a seeded call, a record that does not fit) applies `set` to the generated record too, in the answer and the stored record. The values are evaluated once per call, so the stored record and the answer hold the same `seq()` or `mint()`.

**`refuse`.** Checked in order, on every call with no scenario and no forced status, whether the executor answers it or it is generated (a seeded call, `consistency: off`), before anything is stored or generated. The first entry whose `when` is `true` answers its `status` with the operation's declared error body for it, written as the mock's other error bodies are (an entry that gives both `code` and a message that is not blank asks no model when the faker's body holds both where they go and the schema has no other free text for the model to write), with `code` and `message` placed in the members the error schema has for them: a code in `code`, `error_code`, `errorCode` or a code-like `type`, a message in `message`, `detail`, `error_description` or `title`, at the top level, under an `error` object or in the first item of an `errors` array. A value the schema refuses is not placed. The mock's own refusals (a missing record, a taken unique value, a stale token) place a code the same way, when the code member's `enum` or `const` has a value naming the cause (`NOT_FOUND`, `ALREADY_EXISTS`, `FAILED_PRECONDITION`); otherwise their codes are written as before.

```yaml
operations:
  addMember:
    kind: create
    set:
      id: "md5(lower(request.body.email))"
      currency: "held('lists', request.body.list_id).currency"
      tier: "request.body.amount > 100 ? 'gold' : 'basic'"
    refuse:
      - when: "exists('members', lower(it.email) == lower(request.body.email))"
        status: 409
        code: member_exists
        message: {expr: "'member ' + request.body.email + ' exists'"}
```

**`computed`.** Evaluated whenever a record of the type is answered, and never stored: a read from state, each item of a list from state, the answer of a create, update, replace or action the executor makes, and a generated answer about a held record. A value is written as its member declares it, as `set`'s are; a value of `null`, one the member's schema refuses, and a failed expression (listed under `expr_failed`, with the pointer `/types/<type>/computed/<member>`) leave the member as the answer had it, and an item a list's schema refuses once its members are set is answered as it was. An expression that fails is listed once per call, however many items of a list it failed on. A write's answer is computed after the operation's `effects` ran, so a count reads what they wrote. Counting and summing read the session's records of a type (at most 500).

```yaml
types:
  orders:
    computed:
      # A count of children.
      line_count: "count('lines', it.order_id == record.id)"
      # A total from line items the record holds.
      total: "sum(record.items, it.price * it.quantity)"
      # A template.
      label: "record.customer + ' #' + record.id"
```

**`effects`.** Writes to other records, in order, after the call's own write of its target. They run whenever this call stored (or removed) its target, whichever path stored it: the executor's, or the generated one when the generated answer is written to state (a create, update, replace, action or delete left to generation, a seeded call or a forced status among them). A refused call, a generated error, a generated answer that stores nothing (a delete of a record the session does not hold, a create whose id is held elsewhere, an update answering nothing, which write memory keeps), and a scenario or forced status whose answer stores nothing run none. On the generated path a write's answer computes its members after its effects ran, as the executor's does. Each effect:

- `update` and `delete` find their record by the id `id` gives, as `held` finds one by key; an effect whose record the session does not hold, or holds soft-deleted, is skipped;
- `when`, when given, must be `true`;
- `set` is evaluated with `record` the target as the write left it and `it` the record found (`null` for a `create`), each value written as its member declares it, `null` setting nothing;
- a `create` stores a new record of `type` under the id `set` gives its identity member, else one minted as a create of the type mints it (seeded, like every minted id), in the collection its parent's path makes (the parent its `parent.member` names, else the target when the target is of the parent type), as created in the session, its concurrency token at its first value as a create's; an id the session already holds skips it, and so does a record a read of the type would not render (a required member missing), listed under `expr_failed` with the effect's pointer;
- an `update` stores the record found with the `set` members, its concurrency token (`types.<T>.concurrency`, and the member `If-Match` was seen paired with) advanced as an update the executor answers advances it, unless `set` sets it: a later write sending the old value is refused with its declared conflict;
- a `delete` moves it to its type's soft-deleted state, or removes it and marks it deleted, so a later read of it answers as a read of a deleted record does.

An expression that fails skips its effect and is listed under `expr_failed` (`/operations/<op>/effects/<i>/set/<member>`). Effects do not cascade: a record an effect writes is stored directly, never through the `refuse`, `set` or `effects` of the operation that would write it.

```yaml
operations:
  createLine:
    effects:
      # A side-effect create of a child: the line's first event.
      - op: create
        type: events
        set:
          id: "'ev-' + record.id"
          order_id: "record.order_id"
          number: "seq('events')"
      # A conditional status update on the parent.
      - op: update
        type: orders
        id: "record.order_id"
        when: "it.status == 'draft'"
        set: {status: "'open'"}
  deleteLine:
    effects:
      - {op: delete, type: events, id: "'ev-' + record.id"}
```

**`list`.** A list's expressions, beside its `paging`, `filters`, `scope` and `order`, applied when the list is answered from state; only a list takes them.

- `where` is its default item filter: only the records it is `true` for are listed, `it` bound to each record as stored (the whole record, not the item cut to the list's schema) and `request` the call's; `record` is not bound. It applies before paging, scope, filters and order, and with them: a page holds only the records it keeps, and the next page's cursor or count counts only those. A record it fails on (a type mismatch, its step budget) is left out, and the failure is listed once per call under `expr_failed`. A later page served from a cursor the mock minted holds the records kept when the first page was cut: a record changed since so that `where` is now `false` for it can still appear there.
- `sort` reads the parameter `param` (one the operation declares). When the request sends it, `by` is evaluated with `value` the parameter as sent (and `request`), and gives `[{member, desc?}]` (or one such map): the items are ordered by each member in turn, as `order` orders them (numbers by value, RFC 3339 date-times by instant whatever their offsets, text by text; values of different kinds by kind: booleans, numbers, date-times, other text, then arrays and objects; an item not holding the member, or holding `null`, last either way), then by the described `order`, items equal on all of those keeping their order, before the page is cut. When the request does not send it, the list keeps its `order`. A `by` that fails, gives no member, or gives one the items' schema does not declare is listed under `expr_failed`, and the list keeps its `order` too. The parameter counts as described: a list never leaves it to generation as a parameter it does not read. Validation refuses `list` on an operation that is no list, a `param` the operation does not declare, a `where` reading a member the type's records do not have, `record` in either expression, `it` in `by`, and a `member` written as text in `by` that the records do not have; a member `by` computes is checked when it runs.

```yaml
operations:
  listItems:
    list:
      # A default status filter: only the active items, unless a status is asked for.
      where: "has(request.query.status) || it.status == 'active'"
      # sort=-price, sort=price:desc, sort=price and sort=category,-price.
      sort:
        param: sort
        by: >-
          value.split(',').map(p, trim(p).startsWith('-')
            ? {'member': trim(p).substring(1), 'desc': true}
            : {'member': trim(p).split(':')[0], 'desc': trim(p).endsWith(':desc')})
  listRecords:
    list:
      # sort[0][field]=price&sort[0][dir]=desc, a deep object parameter.
      sort:
        param: sort
        by: "[{'member': request.query.sort['0'].field, 'desc': request.query.sort['0'].dir == 'desc'}]"
```

`derived` (`{<member>: {kind: count, of} | {kind: sum, of, member, where?}}`), the key `computed` replaces, still loads and answers as before: each member is read as a `computed` entry, `count` as the number of the record's held children of type `of` and `sum` as the exact decimal sum of their `member` over those holding the `where` values, its children found as before (named by the child type's `parent.member`, or stored under the collection its `parent.path_param` names; not soft-deleted). It is set only on a record created in the session or one with a child held; a sum goes into an `integer` member only when integral, a `number` member as a number and a `string` member at the children's largest scale, and nothing is set when a child's member is not a number or more than 500 children would have to be read. A `computed` entry for the same member wins. Write `computed` in new descriptions.

`changes` (`{<member>: {from_request: <pointer>} | {value}}`), the key `set` replaces, still loads and sets as before: each entry is read as a `set` entry, `from_request` reading the request body at that JSON pointer (an explicit `null` sent is set) and `value` that literal, each set as it is (not converted to the member's type), and a `set` entry for the same member wins. As before, `changes` applies to updates, replaces and actions, not to creates. Write `set` in new descriptions.

## At runtime

A mock answers from its active contract version's effective description: tier 0 overlaid with the newest revision's entries that validate. It never reads a raw revision. Each module reads the description first and keeps its heuristic for every key the description leaves out, key by key: a described `paging` keeps the guessed filters, and a type with a `lifecycle` but no `unique` keeps the guessed uniqueness checks. Tier 0 restates what the heuristics decide, so a mock with no revision answers exactly as one built without a description, but for its lists: a list of a type the description gives an identity is answered from the session's records ([Lists from state](#at-runtime)). A value the runtime makes from the description (a list page, a computed member, a lifecycle state, a minted id, a conflict body) is checked against the declared schema like a stored record; one that does not fit is not served, and the answer falls back to the heuristic path, never to an invalid answer.

What reads or writes stored records works per session, with `consistency: on` (the default), on calls with no scenario and no forced status; lists answered from state also need an HTTP route or a SOAP or gRPC operation (a GraphQL field's selection narrows its answer), and an LLM for a next page by a token. GraphQL, gRPC and SOAP operations take kind, type, target, transitions, `set` and lifecycles by operation id; the header and status keys do not apply to them.

**Loading and hot reload.** The description is compiled into the mock's runtime when it is built, so a request costs only lookups by operation id and type name. Every new revision of the active contract version (`put_world`, `patch_world`, a restore, carry-forward at activation, a config file's `world:`, a snapshot import) rebuilds the mock, as a scenario change does, on every node. A write through the Admin API (or `patch_world` over MCP) returns once this node's running mock serves the new revision, waiting at most 10 seconds; its answer's `runtime` says `mounted` (the next call uses it), `pending` (the rebuild has not landed in time, or the description could not be loaded and a retry is due), `not_served` (the version is not the active one, or the mock is not mounted on this node) or `unknown` (a `control` node, which runs no mocks). Other nodes rebuild on their own, a moment later. A rebuild keeps the session's stored records and forgets what the runtime holds in memory (which collection a record was placed in, path aliases), so the next request follows the new revision. A revision that changes a type's identity member or pattern migrates nothing: records stored before stay under their old key and are still reached by the new member's value and by either form of a patterned id. When the store cannot read the description, the mock keeps serving the description it served before for the same contract version, else tier 0, logs a warning, and retries the build after 1 second, doubling up to 60 seconds, until it loads. A newest revision that is not a world description at all serves tier 0 and logs the refusal.

**Kind, type, target and answer.** `kind`, `type`, `target` and `answer.record` are written into the compiled operations, so every heuristic that reads an operation's kind or identity follows them unchanged: a `kind: delete` on an action route deletes its target, a `kind: list` over what the importer took for a read answers a list. `answer.record` may point deep into the answer (`/data/0/item`), and `""` says the answer itself is the record.

**Identity and record pointers.** A type's `identity.member` keys its stored records; with a `pattern` (`folders/{id}`), a read, update or delete by a bare id (`7`) or a patterned one (`folders/7`) reaches a record stored under either form, and a create's minted id takes the pattern. `identity.alternates` are tried after the members the runtime guesses from the path parameter's name or description, so they add to the guess and cannot remove it. An update or action that changes an alternate member retires the old value: a read by it is not found from state. An issued alternate (one no read of the type declares, only a create's answer) is minted by the create, as its answer declares it, and kept with the record; a read never shows it. A request whose described `target.member` is an issued alternate naming no held record answers the operation's `not_found` (else its 404), whatever `unknown_ids` says: the server never issued that value. An action whose `set` rewrites the identity member moves the record to its new key, keeping its place in its collection and its list order; the old key is then not found. A new key another record already holds is left to that record, and the renamed one stays under its old key. A plain update keeps the identity as stored. `answer.identity_header` names the declared 2xx header a create gives its new id in: the create mints the id, places it in that header and stores its request's record under it; `false` turns off a header the importer guessed wrongly. `request.record` says where a create's or update's request holds the one record (`{data: {...}}`); a body where the pointer does not resolve is taken whole. A create described with `request.records`, `answer.records` or `answer.long_running` stores each record it makes, minting an id for one that has none; a long-running answer whose `done` member is a boolean is answered done, since the mock holds its result already.

**Transitions, `set` and replace.** An update's or action's `transition` and `set` are set on its stored target, and on an answer that is that record, whatever the model answered: a `transition` writes states of the type's lifecycle member, a `set` entry its expression's value ([Expressions](#expressions)). A create's `set` is applied by the executor once the request is merged, and fill never writes a member it sets. A value that does not fit its member's schema is dropped, and nothing is set when the result does not fit the type; each is listed under `expr_failed` (the entry's pointer, or the operation's for the whole set). The record a create's executor builds is checked whole once fill has completed it, so a nested member `set` writes (`user.type`) does not drop the set for the members of `user` fill writes next. An action with a `transition` or `set` on a target the session holds is answered from state with no LLM call: the record as it then stands, in the envelope `answer.record` names (its other required members drawn from the faker where nothing tells them and each is a scalar, a `status` beside the record, as a read from state draws them), or, for an answer of another schema (an acknowledgement), that schema with the record's values for the members it shares and the faker's for the rest. An update described as `kind: replace` stores what its request sends instead of merging it into the stored record.

**Patch documents and instructions.** An update of a held record whose request is a patch document is applied to the stored record with no LLM call: a JSON Patch (RFC 6902, sent as `application/json-patch+json` or an array of `{op, path}` operations), a SCIM PatchOp (RFC 7644: `add`, `replace` and `remove` at an attribute path or with none), or a JSON merge patch (`application/merge-patch+json`, whose `null` removes a member). A plain update merges objects at any depth, so a member it leaves out of a nested object stays; a request member the record does not declare is generated, unless the update's `set` reads it (`request.body.SmsUrl` for the record's `sms_url`), and then only `set` places it. An update with `request.instructions` applies each instruction's `changes` in order. A patch or instruction applies all or nothing: one that does not (a failed `test`, a path the record does not reach, a SCIM path with a filter, an instruction of a kind `kinds` does not give, a braced step nothing fills) is generated, as is one that changes a read-only member or adds a member the record does not declare; the identity stays as stored.

**Lifecycle, soft delete and `not_found`.** A created record whose request does not name the lifecycle member starts in the `initial` state. A delete of a type with a `soft_delete` moves the record to its state instead of removing it: it stays readable in that state, lists leave it out unless the request sends the `listed_with` parameter at its value, and unique sets no longer count it. The `listed_with` parameter is the soft delete's alone: a `filters` entry on it that names no `member` is refused (a list from state ignores one), as it would narrow to a member no record holds. A delete of a record already in its soft-delete state removes it, so its read is then not found (Okta's delete after a deactivate). So does a delete sending the `unless` parameter at its value (`DELETE /products/7?force=true`, where a plain delete moves the product to the trash); another value of it soft-deletes as before. A state is written as its member declares it (`"true"` into a boolean member is `true`); a state the member refuses is not written, and the delete removes the record as before. An operation's `not_found` is the status, with the body it declares, of a read of a record the mock knows is missing (deleted, or never seen under `unknown_ids: 404`).

**Unique sets.** A create or update whose values of a `unique` set's members equal another stored record's of the type, within the same value of its `scope` member, is refused before generating, with the set's `conflict` status when the operation declares a response for it, else the first refusal status the operation declares; a set the operation can refuse with neither is skipped, and the next set is checked. At most 500 records of the type are compared. The refusal's body is the one the model writes for that operation, status and cause, once, as for the other [error bodies Mockingbird answers itself](concepts.md), with its message naming the set's members and the request's values (`alias 'welcome-1' is already in use`) and its path-like members the member (a map of reasons, `{"alias": ["…"]}`, is keyed by the member; an array of error objects naming no field says the message in each item); offline, the faker's body carries the same message.

**Concurrency.** A write to a stored record whose described `header` (else, with no header or none sent, the `member` in its body or query) does not hold the record's current value is refused from the record's first write with the described `conflict`, when the operation declares it, else its `412` or `409`. Its body is the one the model writes for a stale token, once per operation and status, with a message saying the token does not match the record's current version. `If-Match: *` and a write sending no token pass. An update advances the member: a value the model left as it was is minted anew in the same format. A `concurrency` with no `member` leaves the heuristics as they are.

**Idempotency.** An operation with a described `idempotency` header answers a call repeating a key already answered in the session with that first answer (status, content type, body and headers), before anything else of the call is read, generated or stored, also when the repeat has another body. The final answer is kept, a success or a refusal; a server error, a fallback, or a scenario's or forced status's answer is not, so its retry is a new request. A session keeps its 1024 newest keys. Nothing is inferred: a header the description does not name never replays.

**Paging, filters, scope and order.** A list whose operation describes `paging`, `filters` (non-empty), `scope` or `order` keeps each item through them on every path that puts records into a list answer, and each key replaces only its own heuristic. `filters` replace the filters guessed from parameter names; `scope` is checked on top of the route's collection; `order` sorts the kept items (unset, the model's order, then the store's); `paging` replaces the guessed page parameter, size and token place; a token paging (`cursor`, `next_url`, `header_cursor`) that names only one side of its token's place (no `request` parameter, as a `next_url` the client only follows, or no `response` member or header) keeps the side it names and the heuristics find the other, and its page is cut to the size asked when they find it, else served whole. `eq` compares by value (`"3"` is `3`) and an array parameter matches any of its values; `contains` matches an array member holding the value or a string member holding it as a case-sensitive substring; `gte` and `lte` compare numbers, and RFC 3339 date-times when both sides are one; `flag` matches the boolean; `text` is a case-insensitive substring of the item's string members, or of the described member; one naming no member does not narrow the items the model answered, since a query may hold qualifiers no member holds (`language:rust`). A value that is not of the member's declared type (`minAmount=abc`) narrows nothing. An item the model answered (a stored record may be overlaid on it) that does not hold the filtered member, or holds `null` there, says nothing about it and is kept; only a value that disagrees leaves it out. Where the store fills the page from stored records (a list from state, a page's rest), a record that does not hold the member does not match. A page number counts from 1 unless its parameter's `minimum` is 0; a page past the end is an empty page with no next token; a page token the mock never minted falls back to generation.

<a id="message-queues"></a>**Message queues.** An action with a `queue` keeps messages on its held target (a queue record), first in first out, per session, with no LLM call. `effect: append` (a publish) puts the request's record (its body, or at `request.record`) last and answers as a write of the target does (an acknowledgement's booleans `true`, so a publish's `routed` is `true`); name the queue by `target` (`{path_param: name}`, or `{member: routing_key}` when the body names it). `effect: take` (a get) answers the first messages, oldest first, as an array (at `answer.records` when described): as many as the request body member at `count` asks, one when absent. It removes them, unless the body member at `requeue.at` holds `requeue.value` (or one of its values, an array): then they stay at the head, and a later take answers them with the `redelivered` member `true` (`false` for one never taken). A target holds at most 1000 messages and 1 MiB of them, and its messages go when it is deleted. A `queue` sets no members (no `transition` or `changes`) and needs a target. An unheld target, an append past the cap or an answer that does not validate is generated (`exec_fallback` `not_held`, `over_cap`, `render_failed`).

```yaml
operations:
  publishMessage: {kind: action, type: queues, target: {path_param: name}, queue: {effect: append}}
  getMessages:
    kind: action
    type: queues
    target: {path_param: name}
    queue: {effect: take, count: /count, requeue: {at: /ackmode, value: [requeue]}, redelivered: redelivered}
```

<a id="filter-grammars"></a>**Filter grammars.** A filter with a `grammar` parses its parameter's value into one expression: comparisons of dotted member paths (`eq`, `ne`, `gt`, `ge`, `lt`, `le`, `contains`, `startswith`, `endswith`, present, `in`) joined by `and`, `or`, `not` and parentheses, over text, numbers, booleans and `null`. Each comparison tests the member as the filters above do: `eq` by value, the bounds on numbers and RFC 3339 date-times (sides that do not compare say nothing), `contains` on an array's items or as a substring. A stored record that does not hold a compared member does not match it, but for `ne`, which it does (`null ne 31`; and `not (age lt 100)` keeps it); an item the model answered says nothing of one, and is kept; a value not of its member's declared type (`age eq 'abc'`) says nothing. An item is left out only when the expression is false. A value that does not parse, or a grammar parameter given more than once, is never guessed at: the list is generated (`exec_fallback=unparsed_filter`, naming the parameter).

| `grammar` | Reads | Example |
| --- | --- | --- |
| `scim` | RFC 7644 §3.4.2.2: `eq ne co sw ew gt ge lt le pr`, `and`, `or`, `not`, `"quoted"` text; operators, attribute names and text in any case; a schema URN before an attribute dropped. A bracketed value filter (`emails[type eq "work"]`) is generated. | `filter=userName eq "ann" and emails.value co "@example.com"` |
| `odata` | `$filter`: `eq ne gt ge lt le`, `in (...)`, `and`, `or`, `not`, `'quoted'` text, `contains(m,'x')`, `startswith(...)`, `endswith(...)`; `a/b` is the member `a.b`. | `$filter=status eq 'open' and amount ge 10` |
| `field_value` | `field:value` pairs joined by commas, each an equality, all holding. | `filter=status:open,priority:high` |
| `deep_object` | Every query key bracketed below the parameter (`filter[...]`), none of them undescribed: the keys before the last name the member; a last key `_eq`, `_ne`, `_gt`, `_lt`, `_gte`, `_lte`, `_in`, `_contains`, `_starts_with` or `_ends_with` (or the same after `$`, in any case) is the operator, none is `eq`; all hold. `_or`/`_and` keys are generated. | `filter[category][name][_eq]=tools`; `metadata[run]=r1` with `member_prefix: metadata` |

**Lists from state.** A list (`kind: list`) whose type has an identity is answered from the session's records from its first call, with no LLM call. A session that never wrote a record of the type (it holds none, created none and deleted none) is generated, since the type may be data the API holds and the session never saw (a search's offers); one whose records were all deleted lists nothing. The records are those of the type in the route's collection: a route with no path parameter holds every record of the type when it is the type's only such list (the only one of its items' type as the contract's schemas name it, else, where they name none, the only one the description gives the type) (another, `/me/tasks` beside `/tasks`, holds part of them and is generated, unless the request gives the described `scope`); a nested collection (`/parents/p1/children`) holds the records created under it, those whose member named for the path parameter or whose type's `parent.member` holds its value, and those its mark holds; one that holds none of the type's held records is generated, since they may sit under another route (projects created under a team and listed under the organization), unless it was listed, the type's described `parent` names it, a create of the type is on its path, or a held record holds a member named for each of its path parameters. Each list marks its collection listed in the state store with the ids it placed there, so it survives the rebuild every revision makes, and each create into it adds to the mark; records go in the mark's order, then those no create stored (seeded) least recently written first, then the created in the order they were created, so a later write does not move a record. The query then keeps them as described above (the parameters the description does not name filter by name), the page is cut and counted, and a next page by a token is served from the cut. A page number or offset with a next page writes it at the described `response` member or header, else into the answer's one next-page URL member (NetBox's `next`); with no place at all and a `next` member left `null`, which would say no page follows, the list is generated. A list paged by its `Range` header is ordered and cut by it, a `206` with a `Next-Range` while more remain, and a list the query leaves nothing of answers the bodiless `204` its operation declares beside its list. Envelope members beside the list are what their schema fixes, else `null` where the schema allows it, else the faker's. A query parameter the description does not name and no item member is named for is generated (unless its value is blank, its declared `default`, or the first page), as is a list asked by a body (a `POST`, a SOAP or gRPC request) whose body holds a member that asks something (not `null`, blank, empty or its declared `default`) and that no described parameter names or holds (`exec_fallback_on` names it by its pointer, `ActiveList/Sort`; an object member a described pointer reaches into is looked into), as is a page token the mock never minted, a collection of more than 500 records, or an answer that does not validate. A list whose operation describes `paging`, `filters`, `scope` or `order` but that is generated (a type with no identity) is answered from the session's records once the session has listed that collection with a query asking for nothing but a page. A page with no next page (the last one, or a list short enough to need no cut) never carries the contract's example (or a fixed value) at the described paging `response` member: the member is left out, else `null`, else empty text, the first the answer validates with; when the schema admits none of them, the filled value stays and the answer is marked `no_page_end`.

<a id="embedded-records"></a>**Embedded records.** A type whose `parent.embedded` names an array member of its parent's records is not stored on its own. A read, update or delete of one by its own route (`/incidents/{incident_id}/incident_updates/{incident_update_id}`) that the session does not hold as a record finds the held parent whose array holds an item with that id (the parent its `parent.path_param` names, when the path has it, else the one held parent of the type holding it; when several do, the call is generated) and answers from that item with no LLM call: a read renders it, an update changes it as an update of a record does, a delete takes it out of the array (or, with the type's `soft_delete`, moves it to that state in place) and marks it deleted, so a later read of it is the declared `404`; the parent is stored with the item changed in place, in its collection as before. Its webhooks name the child's id. Nothing is answered this way while a write the parent's record does not show yet is remembered.

**Computed members.** A type's `computed` members (and its `derived` ones, read as `computed`) are set when a record of it is answered from state, in a list from state, in a write's answer, or in a generated answer about a held record, and are never stored ([Expressions](#expressions)).

### Gaps in the request log

Each answer of an operation the description leaves open carries its gaps in the request record, under `request.world_gap` (`get_request`, or `list_requests` with `include_bodies`), and an info log line with `world_gap`: `no_identity` on each operation of a type with no identity that its calls can use (the operations the `no_identity` gap names, whose calls also carry `exec_fallback: no_identity`; an operation with no type is not covered, although its calls carry that fallback too), `no_target` and `no_record` as in `gaps`, and `no_record` also on a 2xx create or update that found no record to store, and `no_page_end` on a last page from state whose described paging `response` member admits no value saying there is no next page (left out, `null` or empty text), so it carries the contract's example. Two more kinds mark an update's request rather than the description, and no world entry resolves them: `ambiguous_choice` when the request sent a scalar for a member the stored record holds as an object and no one member of that object admits the scalar (none does, or several do), and `unapplied_choice` when one member admits it but the object it would make cannot be told whole (a required member the old value decided has no value for the new one, or the schema refuses the object). In both cases the scalar was not applied to the stored record, which keeps its previous object. Do not write an entry for them; change the request or the contract's schema (an `enum`, `const` or examples on the member that takes the scalar) instead. A gap is a mark only: it never changes or refuses an answer. Problem bodies the mock writes itself (a chaos `503`, a refused plan) carry none. Read the gaps of the calls that went wrong to find the entries to write next.

The same record says what the executor made of the call: `exec_fallback`, why a call the description classifies went on to generation (`not_held`, `behind`, `record_invalid`, `undescribed_param` and the other reasons the info log gives), with `exec_fallback_on` naming the query parameter or body member that caused it where one did, and `render_error`, why the record did not render, on a read's or a create's `render_failed` and a create's `record_invalid` (the answer's first schema violation, `<pointer> <keyword>` such as `/result/date type`); and `expr_failed`, the [expressions](#expressions) that failed, each `{pointer, error}` with its JSON pointer in the description (`/operations/addMember/set/number`) and the error's line and column. A read, list or create answered from the mock's record pool notes `pool`, how many records it took ([`limits.record_pool_tokens`](configuration.md)). In a session that has not written a pooled type, its lists with no scope share one set of the pool's records: the first one takes them, and later lists show the same set. Declared filters still apply, a param the world does not describe still falls back, and once the session creates or deletes records of the type, its lists show what it wrote. No response header carries them; the MCP tool `call_mock` reads its call's record and returns `expr_failed` beside the answer, so an assistant sees a failed expression while it checks the mock.

## With an assistant

Install the skills (`mockingbird agent install <agent>`, see [Coding agents and MCP](agents.md)). The `mockingbird-world` skill has the assistant read `gaps` after each import, read that part of the contract and its documentation, write entries with `patch_world` as `agent`, fix what comes back under `refused`, and check the result with `call_mock` on a create read back. When a recorded call answered wrongly, the `world_gap` of its request record (`get_request`) names what to describe next. It records only what the contract or documentation states.

The world tools act on the active contract version unless they are given `version`. A later import that was not activated, or an unapproved draft, is a different version, so the assistant must pass `version` (the number the import or `approve_draft` answered) to `get_world` and `patch_world` until that version is activated; otherwise it reviews and changes the older version, and carry-forward, which ran when the new version was created, never brings the fixes over.

## In a config file

A mock declared in `mockingbird.yaml` takes its description from a file named by `mocks.<name>.world` (for example `world: orders.world.yaml` next to `contract: orders.yaml`), relative to the config file: YAML, or JSON for a `.json` file. See [the configuration reference](configuration.md#mocks).

The file is an overlay, the same document `put_world` takes. On every reconcile (startup, and with `--watch` each edit of the config, the contract or the world file) Mockingbird validates it against the active contract version and, when the entries that apply differ from the newest revision's, stores them as a new revision by `user`; an unchanged file adds no revision. Refused entries (for example one naming an operation the contract no longer has) are logged at warn level and the rest applies; until a load whose file has none, `get_world` (`mockingbird world show`) lists them under `refused` and in its gaps, and `mockingbird doctor` warns about the mock, naming their pointers. A file that cannot be read or parsed, or is refused whole (a file over 1 MiB, a description over 256 KiB in its JSON form, nested over 32 deep, not of the format's shape), fails the reconcile and names the file. The 256 KiB limit is on the description, so YAML comments and layout do not count against it.

The file wins: an edit made through the API or MCP is replaced by the file's entries at the next reconcile. Removing the `world:` key leaves the stored revisions as they are.

## In a snapshot

`mockingbird snapshot export --include world` writes the newest overlay of each mock's active contract version to `worlds/<mock>.json`, and `snapshot import` adds it as a revision by `user` to each mock it creates, after uploading its contract. Entries the imported contract refuses are not stored; import logs each at warn level and lists them under `refused_world_entries.<mock>` in its output. A plain export writes no world.
