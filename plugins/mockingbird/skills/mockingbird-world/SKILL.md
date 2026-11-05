---
name: mockingbird-world
description: Review and write a Mockingbird mock's world description - how its contract behaves as a world (record identity, where requests and answers hold the record, parents, lifecycles and soft deletes, unique members, concurrency tokens, list scope, filters, order and paging). Use after every contract import, or when a mock loses records, cannot read back what it created, or pages, filters or deletes differently from the API's documentation.
---

# Describe a mock's world

Every contract version has a **world description**: a small structured statement of how the API behaves as a world. Mockingbird's heuristics write one automatically (author `heuristic`). You improve it from the contract and its documentation, and Mockingbird validates every entry against the contract before it applies. A description is optional: a mock works without your entries, but a missing identity or record pointer cascades into lost records later.

The format is the JSON Schema `get_world_schema` answers (`mockingbird world schema` on the CLI). Every key is optional; an absent key falls back to the heuristics.

## The loop

1. **Read the gaps first.** After `import_contract` (or `approve_draft`), call `get_world {mock, version}`: pass `version`, the version number the import or approval answered, unless that version is now the active one. Without it every world tool acts on the active version, and a later import that was not activated, or an unapproved draft, is not that version: you would review the old version's gaps and write onto it. The answer holds `world` (the effective description), `authors` (who set each entry; unlisted entries are `heuristic`), `revision` (`0` when only the heuristics wrote it), `refused` and `gaps`. Work through `gaps`:
   - `no_identity`: a type operations write to has no `identity`.
   - `no_target`: a read, update, replace, delete or action does not say which path parameter or body member names its record.
   - `no_record`: a create or update answer in which the record could not be found; say where with `answer.record` (or `answer.records`).
   - `skipped_route`: an HTTP route that is not served. It is reported, not fixable in the description; mention it to the user.
   - `refused`: an entry of the newest revision that did not validate (often after a contract change).
2. **Write what the contract or its docs state.** For each gap, read that part of the contract and its documentation, then write the entries with `patch_world {mock, version, patch, author: "agent"}` (the same `version` as in step 1; a JSON merge patch onto the current overlay; `null` removes a key, arrays are replaced whole). `put_world {mock, world, author: "agent"}` replaces the whole overlay instead. Record only what the contract or the documentation says. Do not guess a lifecycle, a uniqueness rule or a paging style from a field name; leave the key out and the heuristics keep deciding.
3. **Read `refused` in the answer and fix those entries.** A write that is accepted answers `{revision, applied, refused}`: the entries in `applied` are stored as the new revision, and each entry in `refused` is left out, with its JSON pointer and a code:
   - `unknown_key`: the format has no such key there (a typo such as `path_parm`). Check `get_world_schema`.
   - `unresolved`: it names an operation, type, member, parameter, header, status or schema location the contract does not have. Use the contract's exact names and operation ids.
   - `not_applicable`: it does not fit the operation's protocol, e.g. a header or a status on a GraphQL or gRPC operation.
   - `invalid`: the entry is malformed (a pattern without exactly one `{id}`, a state not among the states, a `from_request` pointer that does not parse, or a pointer that does not reach into the schema).

   Some mistakes refuse the whole write instead, and the server stores nothing: the tool call fails with a problem answer. A document over 256 KiB answers 413 (`too_large`; describe structure, never data). A document nested deeper than 32, of the wrong shape (a string where an object belongs, an unknown `kind`), or with a `request` or `answer` pointer that does not parse answers 422 (`invalid`), and its `path` names the first entry at fault. Fix that entry and send the write again.
4. **Check it on the mock.** `call_mock` a create, then read the record back by the id the answer gave (and list it). If the read is a 404 or the list misses it, the identity, target or record pointers are still wrong: look at `get_world` again.
5. **Never put values from traffic or credentials in a description.** It holds member names, parameter names, pointers, states and statuses from the contract. No ids, tokens, keys, customer data or recorded bodies.

Each write adds a numbered revision attached to the contract version; it applies at once and never recompiles the contract. `list_world_revisions {mock}` lists them, `get_world_revision {mock, revision}` reads one with its document, `diff_world {mock, from, to}` shows which pointers changed, and `restore_world_revision {mock, revision}` adds a new revision holding an older one's document. All of them take `version` (default: the active contract version); pass it whenever you work on a version that is not active.

When someone else may write at the same time, pass `expect_revision` (the `revision` you read). A newer revision answers `412 revision_conflict`: call `get_world` again, redo the patch on what is there now, and write again.

A new contract version starts from the previous version's newest overlay. Entries that still validate carry forward with their authors; the rest appear under `refused` (code `unresolved`) and in `gaps`, so review the description after every import, not only the first.

## Vocabulary

Types (`types.<resource type>`):
- `identity: {member, pattern?}`: the member holding the record's id; `pattern` when the id is embedded in a name (`folders/{id}`).
- `parent: {type, member?, path_param?, pattern?}`: the record's parent and where it is named.
- `lifecycle: {member, states?, initial?, soft_delete?: {state, listed_with?: {param, value}}}`: the status member, and the state a delete only moves the record to, with the list parameter that still shows such records.
- `unique: [{members, scope?, conflict?}]`: member sets the docs say are unique, within a member's scope, with the declared conflict status.
- `concurrency: {member?, header?, conflict?}`: the body member and/or header holding a version or ETag, and the declared conflict status.
- `derived: {<member>: {kind: count, of} | {kind: sum, of, member, where?}}`: a member the docs define as a count of a child list or a sum over child records.

Operations (`operations.<operation id>`):
- `kind`: `create`, `read`, `list`, `update`, `replace`, `delete` or `action`; `type`: the resource type.
- `target: {path_param | member}`: what names the record the operation acts on.
- `request` and `answer`: `{record}` or `{records}` as JSON pointers into the body (nested envelopes, one-item arrays, bulk arrays); `answer.long_running: {done, result}` for long-running operations.
- `transition: {<member>: <value>}`: members an action sets (the type needs a `lifecycle`).
- `changes: {<member>: {from_request: <pointer>} | {value}}`: members of the target an action changes.
- Lists: `scope: {param, member}`, `filters: [{param, member?, kind}]` (`eq`, `contains`, `gte`, `lte`, `flag`, `text`), `order: {member, descending?}`, `paging: {style, request?, response?, response_header?, size?}` (`cursor`, `page`, `offset`, `next_url`, `header_cursor`).
- `idempotency: {header}`: the header that makes a replay answer as the first call did; `not_found`: the declared status of a missing record.

GraphQL, gRPC, SOAP and AsyncAPI operations use the same keys, keyed by operation id; HTTP-only keys (headers, statuses) are refused there as `not_applicable`.

## Worked example

A folder API whose documentation says: folders are named `folders/{id}`; a delete only moves a folder to `DELETE_REQUESTED`, and `showDeleted=true` still lists it; display names are unique under a parent (409); writes take the `etag` in `If-Match` (412); `MoveFolder` (`POST /v3/{name}:move`) sets `parent` from `destinationParent`; creates are long-running. And a Xero-style `PUT /Contacts` that wraps records as `{Contacts: [...]}`:

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
  PutContacts:             # Xero: PUT /Contacts with {Contacts: [...]}
    kind: create
    type: contact
    request: { records: /Contacts }
    answer: { records: /Contacts }
```

Written one gap at a time, the first fix is a call such as:

```json
patch_world {"mock": "folders", "author": "agent", "patch": {
  "types": {"folder": {"identity": {"member": "name", "pattern": "folders/{id}"}}},
  "operations": {"MoveFolder": {"target": {"path_param": "name"}}}
}}
```

Then check `refused` in its answer, `call_mock` a `CreateFolder` and read the folder back.

## Without MCP

The same operations are CLI subcommands: `mockingbird world show <mock> [--version N] [--json]` (gaps first), `mockingbird world schema`, and `mockingbird world put|patch|revisions|revision|restore|diff`. The Admin API path is `/api/v1/workspaces/{ws}/mocks/{mock}/contracts/{version}/world` (`version` a number or `active`). A mock declared in `mockingbird.yaml` can take `mocks.<name>.world: <file>` (an overlay, YAML or JSON, relative to the config); the file is stored as a `user` revision on every reconcile where it changed and replaces API edits, so for those mocks write your entries into that file instead of calling `put_world` or `patch_world`.
