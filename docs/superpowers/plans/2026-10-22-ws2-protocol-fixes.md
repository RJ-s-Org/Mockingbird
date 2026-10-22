# WS2 Protocol Fixes Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** GraphQL aliases of one field with different arguments get their own generated values, and chaos errors on an operation that declares only a `default` error response answer in that shape instead of problem+json.

**Architecture:** `graphql/select.rs`'s `Selection` is keyed by **response key** (alias or name) and remembers each child's field name; `narrow` emits one property per response key, with aliases of identical `(name, args)` deduplicated to one property and copied back afterwards. `resolve.rs` reads nested values by response key; a body generated in full (creates/updates, no schema) is re-keyed from field names to response keys first. In `chaos.rs`, `error_status` falls back to a schema'd `default` output (status 503), and the REST handler answers it faker-filled with the message-like field set, reusing the validation-error filler.

**Tech Stack:** Rust workspace (toolchain from `rust-toolchain.toml`), `async-graphql` 7.2 dynamic schema (`SelectionField::{name, alias, arguments, selection_set}`), axum, `serde_json`, `mb_llm::ScriptedProvider` for tests. No new dependencies.

**Spec:** `docs/superpowers/specs/2026-10-22-improvements-backlog-design.md`, section **WS2: Protocol fixes** (2.1, 2.2, Tests).

## Global Constraints

- Branch `feat/ws2-protocol-fixes`, created from `main` in its own worktree; one PR; do **not** merge (the user approves each merge).
- Product code changes stay in WS2's files: `crates/mb-protocols/src/graphql/{select,resolve,batch}.rs`, `crates/mb-protocols/src/chaos.rs`, `crates/mb-protocols/src/http_rest/handler.rs`. Do not touch `crates/mb-engine/*` or `crates/mb-protocols/src/call.rs` (WS1). `batch.rs` needs no change: parts are already keyed by the root field's response key (`resolve.rs:227`), so root aliases with different arguments already get separate parts.
- Tests go in existing files: `crates/mb-protocols/src/graphql/select.rs` and `chaos.rs` `#[cfg(test)]` modules, `crates/mb-protocols/tests/graphql.rs`, `crates/mockingbird/tests/chaos.rs`.
- No `unwrap()`/`expect()` outside tests. No new config keys, no Admin API change (so no `npm run gen:api`), no console change.
- Chaos message copy, exactly: `Service Unavailable (injected by chaos)` (i.e. `format!("{reason} ({})", crate::chaos::CHAOS_ERROR_DETAIL)` with the status's canonical reason).
- The status of a chaos error answered with the `default` shape is **503**.
- Commits end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`; the PR body ends with `🤖 Generated with [Claude Code](https://claude.com/claude-code)`.
- Known limitation, accepted: when fragments on different types give one response key different field names (`... on A { t: name } ... on B { t: title }`), the first field name wins (as arguments do today).

## Review Focus

1. Aliases that swap field names in a narrowed read (`issue { title: body body: title }`): each response key must carry its own field's value, not the same-named field's.
2. The same swap in a mutation, whose answer is generated in full and keyed by field names: must still be served correctly (it is today; re-keying must keep it).
3. Aliased fields on a read with a stored target (`issue(id:) { ref: id heading: title }` after `createIssue`): must still reflect the stored resource and the forced identity, which the engine overlays by field name.
4. Identical-argument aliases with different sub-selections (`a: repositories(first: 2) { name } b: repositories(first: 2) { stargazerCount }`): one property, whose item schema covers both sub-selections, and `b` must not be required of the model.
5. A `default` response without a schema: chaos keeps answering problem+json 503 (there is no shape to fill).

Each line has its test in Task 2 (1–4) or Task 3 (5).

---

### Task 1: Selection keyed by response key, narrowed per response key

**Files:**
- Modify: `crates/mb-protocols/src/graphql/select.rs:15-49` (`Selection`, `Selection::of`, `merge`), `:55-131` (`narrow`, `kept`, `properties`)
- Test: `crates/mb-protocols/src/graphql/select.rs` (`mod tests`)

**Interfaces:**
- Consumes: `async_graphql::SelectionField::{name, alias, arguments, selection_set}` (fragments already flattened by `selection_set`).
- Produces:
  ```rust
  #[derive(Debug, Default, PartialEq)]
  pub(crate) struct Selection {
      /// The field's name (empty for a hand-built root).
      pub name: String,
      /// Literal argument values (variables resolved).
      pub args: Map<String, Value>,
      /// Children by response key (alias or name), merged across fragments.
      pub fields: BTreeMap<String, Selection>,
      /// The response key of the first child, in document order, with the
      /// same name and arguments: this child is not generated, it copies
      /// that one's value.
      pub shares: Option<String>,
  }
  impl Selection { pub(crate) fn of(field: &SelectionField<'_>) -> Self }
  pub(crate) fn narrow(schema: &Value, defs: &Map<String, Value>, sel: &Selection) -> Value // signature unchanged
  ```

- [ ] **Step 1: Write the failing tests**

Update the test helpers to the new struct (`sel` builds children whose `name` equals their key) and add an aliasing helper:

```rust
fn sel<const N: usize>(fields: [(&str, Selection); N]) -> Selection {
    Selection {
        fields: fields.into_iter().map(|(k, mut s)| {
            if s.name.is_empty() { s.name = k.to_string(); }
            (k.to_string(), s)
        }).collect(),
        ..Selection::default()
    }
}

/// A child named `name` with `args`, below `inner`.
fn aliased(name: &str, args: Value, inner: Selection) -> Selection {
    Selection { name: name.into(), args: args.as_object().cloned().unwrap_or_default(), ..inner }
}

#[test]
fn aliases_with_different_arguments_get_their_own_properties() {
    let nodes = || sel([("nodes", sel([("name", leaf())]))]);
    let query = sel([
        ("small", aliased("repositories", json!({"first": 1}), nodes())),
        ("large", aliased("repositories", json!({"first": 5}), nodes())),
    ]);
    let out = narrow(&json!({"$ref": "#/$defs/User"}), &defs(), &query);
    let props: BTreeSet<&str> = out["properties"].as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(props, BTreeSet::from(["__typename", "small", "large"]));
    let required: BTreeSet<&str> = out["required"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
    assert_eq!(required, BTreeSet::from(["__typename", "small", "large"]));
    assert_eq!(out["properties"]["small"]["description"], "Queried with arguments {\"first\":1}");
    assert_eq!(out["properties"]["large"]["description"], "Queried with arguments {\"first\":5}");
    assert_eq!(out["properties"]["small"]["properties"]["nodes"]["items"]["required"], json!(["__typename", "name"]));
}

#[test]
fn renamed_field_takes_its_field_schema() {
    let query = sel([("heading", aliased("login", json!({}), leaf()))]);
    let out = narrow(&json!({"$ref": "#/$defs/User"}), &defs(), &query);
    assert_eq!(out["properties"]["heading"], json!({"type": "string"}));
    assert!(out["properties"].get("login").is_none(), "{out}");
    assert_eq!(out["required"], json!(["__typename", "heading"]));
}

#[test]
fn shared_aliases_emit_one_property() {
    let mut b = aliased("login", json!({}), leaf());
    b.shares = Some("a".into());
    let query = sel([("a", aliased("login", json!({}), leaf())), ("b", b)]);
    let out = narrow(&json!({"$ref": "#/$defs/User"}), &defs(), &query);
    assert!(out["properties"].get("b").is_none(), "{out}");
    assert_eq!(out["required"], json!(["__typename", "a"]));
}
```

(Add `use std::collections::BTreeSet;` to the test module.) The three existing tests must keep passing unchanged apart from the helper.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p mb-protocols --lib graphql::select`
Expected: FAIL to compile with `struct `Selection` has no field named `name`` (and `shares`).

- [ ] **Step 3: Implement the new `Selection` and `narrow` in `crates/mb-protocols/src/graphql/select.rs`**

- `Selection::of`: key each child by `child.alias().unwrap_or(child.name())`; set `name` from `child.name()`; arguments from the first occurrence of that response key (as today); merge sub-selections of repeated response keys (fragments). Then, in document order of first appearance, give every child whose `(name, args)` equals an earlier child's `shares = Some(<earlier key>)` and merge a copy of its sub-selection into the earlier child (so the generated value covers both). Keep the order in a local `Vec<String>`; `fields` stays a `BTreeMap`. `merge` also merges `shares`-free children by response key.
- `narrow`/`properties`: iterate `sel.fields` (skipping `shares.is_some()`), look the schema up by `child.name` in the object's `properties`, emit it under the response key (narrowed by the child, with the existing "Queried with arguments …" note); always keep `__typename` when the schema has it. A selected name missing from `properties` (another member of a union) is skipped.
- `required`: `__typename` if listed, then every non-shared response key whose field name is listed.
- Update the module and struct doc comments (no more "aliases share the first one's arguments").

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p mb-protocols --lib graphql::select`
Expected: PASS (6 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/mb-protocols/src/graphql/select.rs
git commit -m "feat(graphql): narrow the selection per response key

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 2: Project generated values by response key

**Files:**
- Modify: `crates/mb-protocols/src/graphql/select.rs` (add `rekey`, `settle`)
- Modify: `crates/mb-protocols/src/graphql/resolve.rs:353-416` (post-process the root body), `:462-474` (`nested`)
- Test: `crates/mb-protocols/src/graphql/select.rs` (`mod tests`), `crates/mb-protocols/tests/graphql.rs`

**Interfaces:**
- Consumes: `Selection` (Task 1).
- Produces:
  ```rust
  /// A body keyed by field names (generated in full) re-keyed by response
  /// key: each selected key takes its field name's member, recursively
  /// through objects and lists; `__typename` is kept. A value whose
  /// selection has no fields is returned unchanged.
  pub(crate) fn rekey(value: Value, sel: &Selection) -> Value;
  /// A narrowed body made whole: every child with `shares` gets a copy of
  /// the shared key's value (recursively, through lists), and at the top
  /// level a renamed key takes its field name's member when the engine
  /// put one there (stored target overlay, forced identity), provided that
  /// name is not itself a response key and only one non-shared child uses it.
  pub(crate) fn settle(value: &mut Value, sel: &Selection);
  // resolve.rs, signature unchanged:
  pub(crate) fn nested(name: &str, ty: &Type, shapes: &Shapes, ctx: &ResolverContext<'_>)
      -> async_graphql::Result<Option<FieldValue<'static>>>;
  ```

- [ ] **Step 1: Write the failing unit tests (select.rs)**

```rust
#[test]
fn rekey_maps_field_names_to_response_keys() {
    let query = sel([("title", aliased("body", json!({}), leaf())), ("body", aliased("title", json!({}), leaf()))]);
    let full = json!({"__typename": "Issue", "title": "T", "body": null, "state": "OPEN"});
    assert_eq!(rekey(full, &query), json!({"__typename": "Issue", "title": null, "body": "T"}));
    let names = sel([("names", aliased("nodes", json!({}), sel([("n", aliased("name", json!({}), leaf()))])))]);
    assert_eq!(
        rekey(json!({"nodes": [{"name": "a"}, {"name": "b"}]}), &names),
        json!({"names": [{"n": "a"}, {"n": "b"}]})
    );
}

#[test]
fn settle_copies_shared_aliases_and_reflects_stored_names() {
    let mut b = aliased("login", json!({}), leaf());
    b.shares = Some("a".into());
    let query = sel([("a", aliased("login", json!({}), leaf())), ("b", b), ("ref", aliased("id", json!({}), leaf()))]);
    let mut body = json!({"a": "octocat", "ref": "generated", "id": "stored"});
    settle(&mut body, &query);
    assert_eq!(body["b"], "octocat");
    assert_eq!(body["ref"], "stored");
}
```

- [ ] **Step 2: Write the failing integration tests (tests/graphql.rs)**

```rust
/// Two aliases of one field with different arguments are separate
/// properties of the prompt and get separate values.
#[tokio::test]
async fn aliases_with_different_arguments_get_different_values() {
    let repo = |n: &str| json!({"__typename": "Repository", "name": n});
    let provider = scripted(vec![json!({
        "__typename": "User",
        "login": "octocat",
        "small": [repo("one")],
        "large": [repo("one"), repo("two")],
    })]);
    let base = serve(runtime(Some(provider.clone()))).await;
    let r = query(&base, "{ viewer { login small: repositories(first: 1) { name } large: repositories(first: 5) { name } } }").await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.headers["x-mockingbird-source"], "llm", "{:?}", r.headers.get("x-mockingbird-fallback-reason"));
    assert_eq!(r.body["data"]["viewer"]["small"], json!([{"name": "one"}]));
    assert_eq!(r.body["data"]["viewer"]["large"], json!([{"name": "one"}, {"name": "two"}]));
    let system = &provider.requests()[0].messages[0].content;
    assert!(system.contains(r#"{\"first\":1}"#) && system.contains(r#"{\"first\":5}"#), "{system}");
}

/// Aliases with identical arguments share one generated value; the model
/// is not asked for the second one.
#[tokio::test]
async fn aliases_with_identical_arguments_share_one_value() {
    let provider = scripted(vec![json!({
        "__typename": "User",
        "a": [{"__typename": "Repository", "name": "x", "stargazerCount": 7}],
    })]);
    let base = serve(runtime(Some(provider))).await;
    let r = query(&base, "{ viewer { a: repositories(first: 2) { name } b: repositories(first: 2) { stargazerCount } } }").await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.headers["x-mockingbird-source"], "llm");
    assert_eq!(r.body["data"]["viewer"]["a"], json!([{"name": "x"}]));
    assert_eq!(r.body["data"]["viewer"]["b"], json!([{"stargazerCount": 7}]));
}

/// Review focus 1: swapped names in a narrowed read.
#[tokio::test]
async fn swapped_aliases_in_a_read_keep_their_fields() {
    let provider = scripted(vec![json!({"__typename": "Issue", "title": null, "body": "T"})]);
    let base = serve(runtime(Some(provider))).await;
    let r = query(&base, r#"{ issue(id: "I_1") { title: body body: title } }"#).await;
    assert_eq!(r.headers["x-mockingbird-source"], "llm", "{}", r.body);
    assert_eq!(r.body["data"]["issue"], json!({"title": null, "body": "T"}));
}

/// Review focus 2 and 3: a mutation answered in full, then an aliased read
/// of the stored issue.
#[tokio::test]
async fn aliases_after_a_mutation_reflect_state() {
    let provider = scripted(vec![
        issue("i-1", "Crash on start"),
        json!({"__typename": "Issue", "ref": "other", "heading": "Something else"}),
    ]);
    let base = serve(runtime(Some(provider))).await;
    let session = [("x-mockingbird-session", "s1")];
    let created = post(&base, json!({
        "query": "mutation($in: CreateIssueInput!) { createIssue(input: $in) { id title: body body: title } }",
        "variables": {"in": {"repositoryId": "r-1", "title": "Crash on start"}},
    }), &session).await;
    assert!(created.body.get("errors").is_none(), "{}", created.body);
    let made = &created.body["data"]["createIssue"];
    assert_eq!(made["title"], Value::Null);
    assert_eq!(made["body"], "Crash on start");
    let id = made["id"].as_str().unwrap();
    let read = post(&base, json!({
        "query": "query($id: ID!) { issue(id: $id) { ref: id heading: title } }",
        "variables": {"id": id},
    }), &session).await;
    assert!(read.body.get("errors").is_none(), "{}", read.body);
    assert_eq!(read.body["data"]["issue"], json!({"ref": id, "heading": "Crash on start"}));
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p mb-protocols --lib graphql::select` → FAIL to compile (`rekey`, `settle` not found).
Run: `cargo test -p mb-protocols --test graphql aliases_ swapped_aliases`
Expected: `aliases_with_different_arguments_get_different_values`, `aliases_with_identical_arguments_share_one_value` and `swapped_aliases_in_a_read_keep_their_fields` FAIL (source `fallback`, or both aliases equal); `aliases_after_a_mutation_reflect_state` may already pass on the name-keyed code and must keep passing.

- [ ] **Step 4: Implement `rekey`/`settle` in `select.rs`, then wire them in `resolve.rs`**

- `resolve::generate_in`: remember whether `narrowed` was `Some`. On success, the body handed back (and recorded in `report.response`) is `settle`d when it was narrowed, else `rekey`ed with `selection`. Both paths (joined batch and `call::generate_narrowed`) get it.
- `resolve::nested`: read the parent's member under this field's response key (`ctx.ctx.item.node.response_key().node`), falling back to `name` when the parent has no such member. Update its doc comment: the fallback serves subscription payloads, which `subscription.rs` does not re-key.
- `batch.rs`: no change (see Global Constraints).

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p mb-protocols --lib graphql::select && cargo test -p mb-protocols --test graphql && cargo test -p mb-protocols --test graphql_ws`
Expected: PASS (all GraphQL tests, including `query_with_variables_fragments_aliases`, `generation_follows_the_selection_set`, `mutation_then_query_reflects_state`; `rejections_are_recorded` is occasionally flaky, re-run once if it fails).

- [ ] **Step 6: Commit**

```bash
git add crates/mb-protocols/src/graphql/select.rs crates/mb-protocols/src/graphql/resolve.rs crates/mb-protocols/tests/graphql.rs
git commit -m "fix(graphql): aliases with different arguments get their own values

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 3: Chaos errors in the contract's `default` shape

**Files:**
- Modify: `crates/mb-protocols/src/chaos.rs:1-11` (module doc), `:127-151` (`error_status`)
- Modify: `crates/mb-protocols/src/http_rest/handler.rs:433-441` (after the native chaos branch), `:780-830` (extract the filler from `invalid_input`)
- Modify: `docs/configuration.md:231` (the `mocks.<name>.chaos` paragraph)
- Test: `crates/mb-protocols/src/chaos.rs` (`mod tests`), `crates/mockingbird/tests/chaos.rs`

**Interfaces:**
- Consumes: `mb_mcm::{Operation, OutputVariant, StatusSelector}` (`Operation::default_output` is private to `mb-mcm`: find the `default` variant with `op.outputs.iter().find(|o| o.status == StatusSelector::Default)`), `Engine::fallback(bundle, op, variant, status, &OperationCall, &GenerationContext) -> GeneratedResponse`, `handler.rs` `set_message`.
- Produces:
  ```rust
  // chaos.rs: doc gains "…, or 429; else 503 when it declares a `default`
  // response with a schema". None only when it declares none of them.
  pub(crate) fn error_status(op: &Operation) -> Option<u16>;
  // handler.rs: the contract's error shape `variant`, faker-filled, its
  // message-like field set to `message`. Never calls the LLM.
  fn filled_error(rt: &MockRuntime, op: &Operation, variant: &OutputVariant, status: u16, message: &str) -> GeneratedResponse;
  ```

- [ ] **Step 1: Write the failing unit test (chaos.rs)**

```rust
#[test]
fn error_status_falls_back_to_a_default_shape() {
    let op = |codes: &[u16], default_schema: bool| {
        let default = if default_schema {
            "        default: {description: e, content: {application/json: {schema: {type: object, properties: {message: {type: string}}}}}}\n"
        } else {
            "        default: {description: e}\n"
        };
        let codes: String = codes.iter().map(|c| format!("        \"{c}\": {{description: x}}\n")).collect();
        let bundle = mb_import::compile(format!(
            "openapi: 3.0.3\ninfo: {{title: T, version: '1'}}\npaths:\n  /t:\n    get:\n      operationId: t\n      responses:\n{codes}{default}"
        ).as_bytes()).unwrap();
        bundle.operations[0].clone()
    };
    assert_eq!(error_status(&op(&[200], true)), Some(503));
    assert_eq!(error_status(&op(&[200, 500], true)), Some(500));
    assert_eq!(error_status(&op(&[200, 429], true)), Some(429));
    assert_eq!(error_status(&op(&[200], false)), None);
}
```

- [ ] **Step 2: Write the failing integration tests (crates/mockingbird/tests/chaos.rs)**

Add a Stripe-like contract and two tests next to `error_uses_declared_variant`:

```rust
const CHARGES: &str = r##"openapi: 3.0.3
info: {title: Charges, version: 1.0.0}
paths:
  /v1/charges/{charge}:
    get:
      operationId: GetChargesCharge
      parameters:
        - {name: charge, in: path, required: true, schema: {type: string}}
      responses:
        "200":
          description: OK
          content:
            application/json:
              schema: {type: object, required: [id], properties: {id: {type: string}}}
        default:
          description: Error response.
          content:
            application/json:
              schema: {$ref: "#/components/schemas/error"}
components:
  schemas:
    error:
      type: object
      required: [error]
      properties:
        error: {$ref: "#/components/schemas/api_errors"}
    api_errors:
      type: object
      required: [type]
      properties:
        type: {type: string, enum: [api_error, card_error, invalid_request_error]}
        message: {type: string, maxLength: 40000}
"##;

/// A charges mock with chaos always on, its `default` response `contract`.
async fn charges_app(dir: &Path, contract: &str) -> App {
    std::fs::write(dir.join("charges.yaml"), contract).unwrap();
    app(dir, &format!("mocks:\n  charges:\n    contract: charges.yaml\n{ALWAYS_ERROR}")).await
}

#[tokio::test]
async fn default_only_error_uses_the_default_shape() {
    let dir = tempfile::tempdir().unwrap();
    let data = serve(charges_app(dir.path(), CHARGES).await.data.clone()).await;
    let r = reqwest::get(format!("{data}/charges/v1/charges/ch_1")).await.unwrap();
    assert_eq!(r.status(), 503);
    assert_eq!(r.headers()["x-mockingbird-chaos"], "error");
    assert!(r.headers()["content-type"].to_str().unwrap().starts_with("application/json"));
    let body: Value = r.json().await.unwrap();
    let bundle = mb_import::compile(CHARGES.as_bytes()).unwrap();
    let op = bundle.operation("GetChargesCharge").unwrap();
    // No 503 is declared, so this is the `default` variant.
    let schema = op.output_for_status(503).unwrap().schema.as_ref().unwrap();
    SchemaValidator::new(&bundle.with_reachable_defs(schema)).unwrap().validate(&body).unwrap();
    assert_eq!(body["error"]["message"], "Service Unavailable (injected by chaos)", "{body}");
}

/// Review focus 5: a `default` without a schema has no shape to fill.
#[tokio::test]
async fn default_without_schema_is_protocol_native() {
    let dir = tempfile::tempdir().unwrap();
    let contract = CHARGES.replace(
        "          description: Error response.\n          content:\n            application/json:\n              schema: {$ref: \"#/components/schemas/error\"}\n",
        "          description: Error response.\n",
    );
    assert_ne!(contract, CHARGES);
    let data = serve(charges_app(dir.path(), &contract).await.data.clone()).await;
    let r = reqwest::get(format!("{data}/charges/v1/charges/ch_1")).await.unwrap();
    assert_eq!(r.status(), 503);
    assert_eq!(r.headers()["content-type"], "application/problem+json");
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p mb-protocols --lib chaos::tests::error_status`
Expected: FAIL (`left: None, right: Some(503)`).
Run: `cargo test -p mockingbird --test chaos default_`
Expected: `default_only_error_uses_the_default_shape` FAILS (content type `application/problem+json`); `default_without_schema_is_protocol_native` passes (guard).

- [ ] **Step 4: Implement**

- `chaos::error_status`: after the 503 / 500 / smallest 5xx / 429 chain, return `Some(503)` when `op.outputs` has a `StatusSelector::Default` output with `schema.is_some()`. Update the module doc (lines 5-11) to say so.
- `handler.rs`: extract `filled_error` from `invalid_input` (its `rt.engine.fallback(…)` + `set_message(…)` lines); `invalid_input` calls it with `reasons`.
- `handler.rs`, right after the `plan.native_chaos_error()` branch and before the stream branch: when `plan.chaos.inject_error`, `plan.forced_status` is `Some(status)`, the operation declares no `StatusSelector::Code(status)` output, and the operation's `default` variant has a schema: set `log.delay = plan.delay`, answer `log.generated(filled_error(rt, op, variant, status, &message))` with `message = format!("{reason} ({})", crate::chaos::CHAOS_ERROR_DETAIL)` (`reason` = canonical reason of `status`, `"Error"` if none), and `mark_chaos`. Declared 5xx/429 outputs keep today's generation path.
- `docs/configuration.md:231`: "An injected error uses the operation's declared 503, 500, other 5xx or 429 response; else its `default` response (HTTP 503, faker-filled, its message-like field set to `Service Unavailable (injected by chaos)`); else the protocol's native one (…)".

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p mb-protocols --lib chaos && cargo test -p mockingbird --test chaos && cargo test -p mb-protocols --test http_rest`
Expected: PASS (including `error_uses_declared_variant`, `undeclared_error_is_protocol_native` and the validation-error tests in `http_rest`).

- [ ] **Step 6: Commit**

```bash
git add crates/mb-protocols/src/chaos.rs crates/mb-protocols/src/http_rest/handler.rs crates/mockingbird/tests/chaos.rs docs/configuration.md
git commit -m "fix(chaos): answer injected errors in the contract's default shape

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 4: Gates, changelog, pull request

**Files:**
- Modify: `CHANGELOG.md` (`## [Unreleased]` → `### Fixed`)

- [ ] **Step 1: Add the changelog entries** under `## [Unreleased]` / `### Fixed`:
  - `GraphQL aliases of one field with different arguments (`small: repositories(first: 1)`, `large: repositories(first: 5)`) get separate generated values; aliases with identical arguments share one.`
  - `Chaos errors on an operation that declares only a `default` error response (Stripe) answer HTTP 503 in that shape, with its message-like field set, instead of problem+json.`

- [ ] **Step 2: Run the local gates**

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo check -p mockingbird --no-default-features
```
Expected: all pass. Known pre-existing failures to ignore and mention in the PR: `tls::llm_client_trusts_outbound_ca` on macOS, and the occasionally flaky `graphql::rejections_are_recorded`. No console files changed, so no npm checks.

- [ ] **Step 3: Commit and push**

```bash
git add CHANGELOG.md
git commit -m "docs: changelog for WS2 protocol fixes

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
git push -u origin feat/ws2-protocol-fixes
```

- [ ] **Step 4: Open the PR (do not merge)**

```bash
gh pr create --base main --head feat/ws2-protocol-fixes \
  --title "fix(protocols): GraphQL aliases with different arguments; chaos errors in the default shape" \
  --body "$(cat <<'EOF'
WS2 of docs/superpowers/specs/2026-10-22-improvements-backlog-design.md (2.1, 2.2).

- GraphQL: the narrowed schema and projection are keyed by response key, so aliases with different arguments get their own values; identical (name, args) aliases share one. Full (create/update) answers are re-keyed; aliased reads still reflect stored state.
- Chaos: an operation with only a `default` error response answers 503 in that shape (faker-filled, message set); problem+json only when no error response is declared.

Gates: fmt, clippy, test pass. Pre-existing, unrelated: `tls::llm_client_trusts_outbound_ca` (macOS), flaky `graphql::rejections_are_recorded`.

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```
Expected: PR URL printed. Report it; the user approves the merge.
