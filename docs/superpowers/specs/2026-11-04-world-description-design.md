# World descriptions: design

Status: draft for review; open questions answered 2026-11-04. Date: 2026-11-04. Evidence: the blind benchmark rounds in `docs/benchmarks/blind*/`, the eighth and ninth set diagnoses, and the cross-suite diagnosis of blind sets 1–7.

## Problem

After nine blind sets, each new set opens with a first run of 30–55%. Each round's fixes then add a few points to older sets. The failures that remain are mostly not engine bugs. They come from API styles the importer's heuristics do not model:
- a record held as the one item of an array inside an envelope (Xero);
- AIP resource names such as `folders/123`, long-running operations and `:move` methods (Cloud Resource Manager);
- a `{success, result}` envelope (Cloudflare);
- Relay connections (Linear) and RPC-style routes (Dropbox).

One early break, usually a record with no identity, cascades. Xero passed 13/168 and Resource Manager 22/168, because nothing was ever stored. Writing heuristics for one style at a time will not keep up with the long tail of real APIs.

The compiled contract (`mb_mcm::Operation`) stores only `kind`, `resource_type` and `identity`. Envelope depth is at most one member (`RecordAt`).

`mb-protocols/src/call/*` and `BundleIndex` guess the following fresh on every request, and nothing exposes what they guessed:
- paging;
- concurrency tokens;
- filters and scope;
- unique members;
- delete scope.

There is no model at all of:
- lifecycle or status members;
- soft deletes;
- header cursors;
- `If-Match`.

The only override is `x-mockingbird-resource`, with three members, on OpenAPI only.

## Goal

The mock reads the API's documentation the way a careful developer would. The result is written down once as a **world description**, a small structured statement of how the contract behaves as a world. The engine runs from it deterministically.

The LLM that is best placed to write it already sits next to the user. Mockingbird is used mostly through coding assistants (Claude Code, Codex, Cursor), which read the contract anyway. So the assistant writes the description through Mockingbird's MCP tools or CLI, and Mockingbird validates it. Reading documentation is what LLMs do well. Holding structure exactly is what the engine does well.

## Constraints

1. **Optional, never required.** A contract alone still works. The heuristics produce a description automatically, so every mock always has one. An assistant or a person only improves it. This keeps constraint 1 of the [simulated world design](2026-10-30-simulated-world-design.md), which rejected a required, reviewed blueprint. A description is not a dataset and holds no records.
2. **Validated, never trusted.** Every entry must resolve against the contract: pointers into schemas, members that exist, parameters that are declared, statuses that are declared. An entry that doesn't resolve is refused with an error naming it, and the rest still applies.
3. **No API names in the engine.** The description is where per-API knowledge goes. The engine reads only the description's vocabulary.
4. **Secrets stay references.** A description holds no values from traffic and no credentials.
5. **Every answer still matches the contract.**

## Who writes it

| Tier | Author | When |
| --- | --- | --- |
| 0 | Heuristics, including the documented-convention detectors below | Always, at compile time. The baseline today's behaviour becomes. |
| 1 | The user's coding assistant, through MCP or CLI | The primary path. The `mockingbird-mock-api` skill tells the assistant to review the description after every import, and a new `mockingbird-world` skill says how. |
| 2 | Mockingbird's configured LLM (`mockingbird world draft`) | A fallback where no assistant is involved. It is off by default, uses the import profile and budget, and goes through the same validator. |
| 3 | A person editing the file | Always possible. |

Each entry records its author (`heuristic`, `agent`, `llm` or `user`). A later tier overrides an earlier one entry by entry. Recompiling a changed contract keeps the entries written by an assistant, LLM or person that still validate. The ones that no longer validate are reported in the diagnostics.

## The description

A YAML or JSON document, versioned with a published JSON Schema (`mockingbird schema world`). An illustrative excerpt:

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
    changes: { parent: { from_request: destinationParent } }
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

Vocabulary:

- **types:**
  - `identity`: member, optional pattern.
  - `parent`.
  - `lifecycle`: status member, states, initial state, soft delete and how it is listed.
  - `unique`: member sets, scope, declared conflict status.
  - `concurrency`: body member and/or header, conflict status, how the token advances.
  - `derived`: members the engine computes exactly from stored records, so a model never does the arithmetic. Two forms are supported:
    - the count of a child list;
    - the sum of a member over child records, optionally filtered by state (e.g. a balance as the sum of posted entry amounts).

    Nothing else is derived. Members not described stay with the model, as today.
- **operations:**
  - `kind`: create, read, list, update, replace, delete, action.
  - `type` and `target`: which path parameter or body member names the record.
  - `request` and `answer`: JSON pointers to the record or records. This covers nested envelopes, one-item arrays, bulk arrays and long-running operations.
  - `transition`: a state set or moved by an action.
  - `changes`: request members applied to the target.
  - `scope`, `filters`, `order`, `paging`: style, the request and response members or headers, page size.
  - `idempotency`: the header that makes a replay answer as the first call did.
  - `not_found`: the declared status to use.
- Every key is optional. An absent key falls back to today's inference.

## Flow

1. **Import or compile** runs the heuristics as today, then turns their results into a tier-0 description. The description is stored on the contract version next to the bundle (`ContractVersion`, the same home as `draft_meta`), not in session state.
2. **Diagnostics:** `get_world`, available as an MCP tool, `mockingbird world show` and `GET /mocks/{m}/contracts/{v}/world`, returns the description with each entry's author. It also lists the gaps:
   - types with no identity;
   - operations with no target;
   - answers whose record could not be found;
   - skipped routes;
   - entries refused at validation.

   Today none of this is exposed.
3. **The assistant** reads the contract and the diagnostics, then writes entries with `put_world` (whole document) or `patch_world` (JSON merge patch). Validation answers with what was applied and what was refused, and why.
4. **Revisions:**
   - Each change adds a numbered revision of the description, attached to its contract version. The newest revision applies at once, as `put_scenario` does today.
   - Revisions can be listed, diffed and rolled back. Changing one never recompiles the contract or adds a contract version.
   - A new contract version starts from the previous description. Entries that still validate carry forward, and the rest appear as gaps.
   - Config-file mocks take `mocks.<name>.world: <path>`.
5. **Runtime:** the modules that guess today (identity, envelopes, `pages`, `runs`, `lists`, `tokens`, `guards`, `deleted`) read the description first and guess only where it is silent.
6. **Snapshots** carry the description as a new `SnapshotPart`.

## Documented conventions (tier 0)

Conventions many APIs follow get structural detectors that propose tier-0 entries. Each one is tested on synthetic contracts and never keyed to an API name:
- AIP resource names, custom methods (`{name}:verb`) and long-running operations;
- Relay connections;
- OData keys, `$top`/`$skip`/`@odata.nextLink`, ETags;
- HAL and JSON:API envelopes and links;
- `Link` header and cursor-header paging;
- RPC-style POST routes.

An assistant or a person can override any detection.

## Degrading gracefully

When identity or a record pointer is unknown, the operation is still answered by the LLM. It is marked `world_gap` in the request log and listed in the diagnostics, so the assistant sees what to fix instead of facing silent cascades. A gap never makes the mock refuse a request.

## Other protocols

The description is keyed by operation id, so GraphQL, gRPC, SOAP and AsyncAPI share the format. OpenAPI and Swagger come first. `x-mockingbird-resource` becomes one input to tier 3 and keeps working.

## Measuring it

The benchmark gains a `--world` mode with three tiers, reported separately for each model:
- `heuristic`: today's behaviour.
- `llm`: tier 2, with the default model.
- `agent`: a capable model writes the description from the contract text and the `mockingbird-world` skill alone, once per contract. It never sees checks, journeys or engine code. The bench records its cost and runs the journeys on the result.

Blindness rules stay as they are. The headline number is the `agent` tier on the default runtime model, on a blind set that was never diagnosed. A per-model table shows each runtime model. Providers stay pinned.

## Delivery order

Each step is its own PR with independent review, and each is measured on blind sets.

1. **Foundation:**
   - MCM `World` types, the JSON Schema, and a validator with path-precise errors.
   - The tier-0 description built from today's heuristics, with no behaviour change, and a test that proves no change.
   - Storage on the contract version.
   - `get_world` and diagnostics in MCP, CLI and Admin API, including skipped routes and gaps.
2. **Authoring:** `put_world`/`patch_world`, versioning, config-file and snapshot support, and the `mockingbird-world` skill.
3. **Runtime reads the description**, in this order:
   - identity and record pointers, which are behind the cascades;
   - operation kind, target and transition;
   - lifecycle and soft delete;
   - unique members and concurrency;
   - paging, filters and scope;
   - derived counts and sums.

   Each module keeps its heuristic as the fallback.
4. **Benchmark `--world` tiers**, and a blind tenth set measured on all three.
5. **Convention detectors** (tier 0), one PR per convention family.
6. **Tier-2 `mockingbird world draft`.** It runs only when asked, never automatically at import.

## Decisions

1. A description change adds a revision attached to its contract version. It doesn't add a contract version (see Flow, step 4).
2. Tier 2 runs only on request.
3. Derived counts and sums are in scope, as their own runtime step. The benchmark checks stay as they are: they already test what the contracts document.
