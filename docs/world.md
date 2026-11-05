# World descriptions

A **world description** says how a contract behaves as a world: which member holds a record's id, where a request or an answer holds the record, what a delete does, which members must be unique, how a list is scoped, filtered, ordered and paged. Every contract version has one. Mockingbird's import heuristics write it automatically, and your coding assistant (or you) can improve it from the API's documentation. Mockingbird validates every entry against the contract before it applies.

A description is optional: a contract alone still works. It is not a dataset and holds no records, no values from traffic and no credentials.

> Today the description is read, validated, stored and reported. The runtime does not yet answer from it: it keeps the heuristics, so no mock answers differently because of a description. Later releases read it one module at a time (identity and record pointers first) and keep the heuristics as the fallback wherever it is silent.

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
mockingbird world schema                   # the format, as JSON Schema
```

The MCP tool is `get_world {mock, version?}`, and `get_world_schema` answers the format's JSON Schema as `mockingbird world schema` prints it; the Admin API is `GET /api/v1/workspaces/{ws}/mocks/{mock}/contracts/{version}/world`, with `version` a number or `active`. The answer:

| Field | What it holds |
| --- | --- |
| `version` | The contract version it describes. |
| `world` | The effective description: tier 0 overlaid with the newest revision's entries that validate. |
| `authors` | The author of each entry the newest revision sets, keyed by JSON pointer (`/types/folder/identity`). |
| `revision` | The newest revision's number; `0` when only the heuristics wrote it (every version stored before world descriptions starts there). |
| `refused` | Entries of the newest revision that did not validate; each falls back to tier 0. |
| `gaps` | What the description leaves open. |

Gaps:

| Code | Meaning |
| --- | --- |
| `no_identity` | A type that operations write to has no `identity`. |
| `no_target` | A read, update, replace, delete or action does not say what names its record. |
| `no_record` | A create or update answer in which no member fits the type, and no `answer.record` says where it is. |
| `skipped_route` | An HTTP route of the contract that is not served (it matches the same requests as an earlier one, or is not a valid route pattern). |
| `refused` | An entry refused at validation. |

## Writing it

A write sends an **overlay**: only the entries you want to set. The effective description is computed on every read as tier 0 overlaid with the newest revision.

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

Each write adds a numbered revision (from 1) attached to its contract version, with its author. The newest revision applies at once. A change never recompiles the contract or adds a contract version.

Two writers can meet. Send the revision you read as `expect_revision` (the `If-Match` header in the Admin API). If someone wrote since, the write is refused with `412 revision_conflict`: read again and redo the change. Without it, the write adds a revision on top of the newest.

### A new contract version

A new contract version (an upload, a config-file change, an approved import draft) starts from the previous version's newest overlay. Entries that still validate carry forward with the author that set them. Entries that no longer resolve (a member, operation or status the new contract dropped) are listed under `refused` with code `unresolved` and as gaps; everything else applies, and reads never fail.

## The format

YAML or JSON, `version: 1`. Every key is optional; an absent key falls back to the heuristics. `get_world_schema` (MCP) and `mockingbird world schema` give the full JSON Schema.

**Types** (`types.<resource type>`):

| Key | Shape | Meaning |
| --- | --- | --- |
| `identity` | `{member, pattern?}` | The member holding the id; `pattern` (one `{id}`) when the id is embedded in a name. |
| `parent` | `{type, member?, path_param?, pattern?}` | The parent record and where it is named. |
| `lifecycle` | `{member, states?, initial?, soft_delete?}` | The status member and its states; `soft_delete: {state, listed_with?: {param, value}}` for a delete that only moves the record to a state, and the list parameter that still shows it. |
| `unique` | `[{members, scope?, conflict?}]` | Member sets that must be unique, optionally within a member's scope, with the declared conflict status. |
| `concurrency` | `{member?, header?, conflict?}` | The body member and/or header holding the version or ETag, and the declared conflict status. |
| `derived` | `{<member>: {kind: count, of} \| {kind: sum, of, member, where?}}` | Members computed from stored records: the count of a child list, or the sum of a member over child records. |

**Operations** (`operations.<operation id>`):

| Key | Shape | Meaning |
| --- | --- | --- |
| `kind` | `create`, `read`, `list`, `update`, `replace`, `delete`, `action` | What the operation does. |
| `type` | string | Its resource type. |
| `target` | `{path_param}` or `{member}` | What names the record it acts on. |
| `request`, `answer` | `{record}` or `{records}` (JSON pointers) | Where the body holds the record or records: nested envelopes, one-item arrays, bulk arrays. `answer.long_running: {done, result}` for long-running operations. |
| `transition` | `{<member>: <value>}` | Members an action sets; the type needs a `lifecycle`. |
| `changes` | `{<member>: {from_request: <pointer>} \| {value}}` | Members of the target the operation changes. |
| `scope` | `{param, member}` | The parameter that limits a list to one scope. |
| `filters` | `[{param, member?, kind}]` | `kind`: `eq`, `contains`, `gte`, `lte`, `flag`, `text`. |
| `order` | `{member, descending?}` | The order of a list. |
| `paging` | `{style, request?, response?, response_header?, size?}` | `style`: `cursor`, `page`, `offset`, `next_url`, `header_cursor`. |
| `idempotency` | `{header}` | The header that makes a replay answer as the first call did. |
| `not_found` | status | The declared status of a missing record. |

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
    changes: { parent: { from_request: /destinationParent } }
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
    filters: [{ param: showDeleted, kind: flag }]
  PutContacts:             # PUT /Contacts with {Contacts: [...]}
    kind: create
    type: contact
    request: { records: /Contacts }
    answer: { records: /Contacts }
```

## With an assistant

Install the skills (`mockingbird agent install <agent>`, see [Coding agents and MCP](agents.md)). The `mockingbird-world` skill has the assistant read `gaps` after each import, read that part of the contract and its documentation, write entries with `patch_world` as `agent`, fix what comes back under `refused`, and check the result with `call_mock` on a create read back. It records only what the contract or documentation states.

The world tools act on the active contract version unless they are given `version`. A later import that was not activated, or an unapproved draft, is a different version, so the assistant must pass `version` (the number the import or `approve_draft` answered) to `get_world` and `patch_world` until that version is activated; otherwise it reviews and changes the older version, and carry-forward, which ran when the new version was created, never brings the fixes over.

## In a config file

A mock declared in `mockingbird.yaml` takes its description from a file named by `mocks.<name>.world` (for example `world: orders.world.yaml` next to `contract: orders.yaml`), relative to the config file: YAML, or JSON for a `.json` file. See [the configuration reference](configuration.md#mocks).

The file is an overlay, the same document `put_world` takes. On every reconcile (startup, and with `--watch` each edit of the config, the contract or the world file) Mockingbird validates it against the active contract version and, when the entries that apply differ from the newest revision's, stores them as a new revision by `user`; an unchanged file adds no revision. Refused entries (for example one naming an operation the contract no longer has) are logged at warn level and the rest applies. A file that cannot be read or parsed, or is refused whole (a file over 1 MiB, a description over 256 KiB in its JSON form, nested over 32 deep, not of the format's shape), fails the reconcile and names the file. The 256 KiB limit is on the description, so YAML comments and layout do not count against it.

The file wins: an edit made through the API or MCP is replaced by the file's entries at the next reconcile. Removing the `world:` key leaves the stored revisions as they are.

## In a snapshot

`mockingbird snapshot export --include world` writes the newest overlay of each mock's active contract version to `worlds/<mock>.json`, and `snapshot import` adds it as a revision by `user` to each mock it creates, after uploading its contract. Entries the imported contract refuses are not stored; import logs each at warn level and lists them under `refused_world_entries.<mock>` in its output. A plain export writes no world.
