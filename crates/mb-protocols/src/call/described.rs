//! The effective world description, compiled into the runtime once.
//!
//! What the bundle already models (kind, type, target, the answer's record pointer, a
//! type's identity member) is written into the runtime's bundle by
//! [`mb_mcm::world::apply`]; everything else a module reads from the description
//! (lifecycle, unique members, concurrency, paging, filters, scope, order,
//! transitions, changes, computed and derived members, effects, idempotency, `not_found`) is looked up here,
//! by operation id or type name, before the module's heuristic. A key the description
//! leaves absent falls back to that heuristic, key by key.
//!
//! [`Described`] also holds each operation's static gaps (see [`crate::world_gaps`]):
//! what the description leaves open about it, marked `world_gap` in the request log of
//! every answer of the operation. A gap never changes or refuses an answer.
//!
//! `Described::default()` describes nothing; whether a world is attached at all is
//! [`MockRuntime::world`](crate::MockRuntime::world).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use mb_mcm::world::{IdentityHeader, OpDesc, OpKind, TypeDesc, World};
use mb_mcm::{Bundle, OperationKind};
use serde_json::Value;

use crate::world_gaps::{self, GapCode};

/// The effective world of one runtime, indexed for lookups on the request path.
#[derive(Debug, Default)]
pub(crate) struct Described {
    /// The effective world: tier 0 overlaid with the newest revision's valid entries.
    world: Option<Arc<World>>,
    /// The revision `world` is at; `0` for none.
    revision: i64,
    /// Each operation's static gaps, by operation id, in the order
    /// `no_identity`, `no_target`, `no_record`.
    gaps: HashMap<String, Vec<GapCode>>,
    /// The effective identity member of each type whose member differs from tier 0's.
    changed_members: HashMap<String, String>,
    /// The schema of each type's lifecycle member, where the record schema declares it.
    lifecycle_members: HashMap<String, Value>,
    /// The identity header of each create whose effective `answer.identity_header`
    /// differs from tier 0's.
    changed_headers: HashMap<String, IdentityHeader>,
    /// The `identity.alternates` of each type whose effective list differs from tier
    /// 0's.
    alternates: HashMap<String, Vec<String>>,
    /// The world's expressions (`set`, `changes`, `refuse`, `effects`, `computed`,
    /// `derived`), compiled once.
    expressions: mb_mcm::world::Expressions,
}

impl Described {
    /// The index of `world`, the effective world over `base` (`tier0` of the
    /// contract), for `bundle`, the contract with `world` applied to it.
    pub(crate) fn new(bundle: &Bundle, base: &World, world: &World, revision: i64) -> Self {
        let member = |w: &World, t: &str| {
            w.types
                .get(t)
                .and_then(|d| d.identity.as_ref())
                .and_then(|i| i.member.clone())
        };
        let changed_members = world
            .types
            .keys()
            .filter_map(|t| {
                let m = member(world, t)?;
                (member(base, t).as_ref() != Some(&m)).then(|| (t.clone(), m))
            })
            .collect();
        let lifecycle_members = world
            .types
            .iter()
            .filter_map(|(t, d)| {
                let l = d.lifecycle.as_ref()?;
                let record = record_schema(bundle, world, t)?;
                let schema = mb_engine::consistency::property_schema(bundle, &record, &l.member)?;
                Some((t.clone(), schema))
            })
            .collect();
        let header = |w: &World, op: &str| {
            w.operations
                .get(op)
                .and_then(|d| d.answer.as_ref())
                .and_then(|a| a.identity_header.clone())
        };
        let changed_headers = world
            .operations
            .keys()
            .filter_map(|op| {
                let h = header(world, op)?;
                (header(base, op).as_ref() != Some(&h)).then(|| (op.clone(), h))
            })
            .collect();
        let alternates_of = |w: &World, t: &str| {
            w.types
                .get(t)
                .and_then(|d| d.identity.as_ref())
                .and_then(|i| i.alternates.clone())
        };
        let alternates = world
            .types
            .keys()
            .filter_map(|t| {
                let a = alternates_of(world, t)?;
                (alternates_of(base, t).as_ref() != Some(&a)).then(|| (t.clone(), a))
            })
            .collect();
        super::computed::log_skipped_child_types(world);
        Self {
            world: Some(Arc::new(world.clone())),
            revision,
            gaps: static_gaps(bundle, world),
            changed_members,
            lifecycle_members,
            changed_headers,
            alternates,
            expressions: mb_mcm::world::Expressions::compile(world),
        }
    }

    /// The compiled expressions of operation `id` (its `set`, with its `changes`
    /// rewritten as `set`, and its `refuse`), when it has any.
    pub(crate) fn expressions(&self, id: &str) -> Option<&mb_mcm::world::OpExpressions> {
        self.expressions.op(id)
    }

    /// The compiled computed members of type `t` (its `computed`, with its `derived`
    /// rewritten as `computed`), when it has any.
    pub(crate) fn computed(&self, t: &str) -> Option<&mb_mcm::world::TypeExpressions> {
        self.expressions.of_type(t)
    }

    /// The effective world, when one is attached.
    pub(crate) fn world(&self) -> Option<&Arc<World>> {
        self.world.as_ref()
    }

    /// The revision of the attached world; `0` with none.
    pub(crate) fn revision(&self) -> i64 {
        self.revision
    }

    /// The identity header the description gives create `op_id` when it differs from
    /// tier 0's (the detector's): a header name, or [`IdentityHeader::Off`].
    pub(crate) fn changed_identity_header(&self, op_id: &str) -> Option<&IdentityHeader> {
        self.changed_headers.get(op_id)
    }

    /// The alternate members the description gives type `t` when they differ from tier
    /// 0's (which emits none): `[]` included.
    pub(crate) fn described_alternates(&self, t: &str) -> Option<&[String]> {
        self.alternates.get(t).map(Vec::as_slice)
    }

    /// The effective identity pattern of type `t` (`folders/{id}`), when it has one.
    pub(crate) fn identity_pattern(&self, t: &str) -> Option<&str> {
        self.type_desc(t)?.identity.as_ref()?.pattern.as_deref()
    }

    /// The effective identity member of type `t` when it differs from tier 0's (a
    /// type tier 0 gives no member counts as different).
    pub(crate) fn changed_identity_member(&self, t: &str) -> Option<&str> {
        self.changed_members.get(t).map(String::as_str)
    }

    /// How the description says operation `op_id` acts.
    pub(crate) fn op(&self, op_id: &str) -> Option<&OpDesc> {
        self.world.as_ref()?.operations.get(op_id)
    }

    /// How the description says records of type `t` behave.
    pub(crate) fn type_desc(&self, t: &str) -> Option<&TypeDesc> {
        self.world.as_ref()?.types.get(t)
    }

    /// What the description leaves open about operation `op_id`.
    pub(crate) fn gaps(&self, op_id: &str) -> &[GapCode] {
        self.gaps.get(op_id).map_or(&[], Vec::as_slice)
    }

    /// Replaces the static gaps of operation `op_id`.
    pub(crate) fn set_gaps(&mut self, op_id: String, gaps: Vec<GapCode>) {
        self.gaps.insert(op_id, gaps);
    }

    /// Lifecycle state `state` of type `t` as its status member declares it: `true` or
    /// `false` for a boolean member, a number for a numeric one, the text otherwise.
    /// `None` when `t` has no lifecycle, or the state is not a value of the member's
    /// type or of its `enum`.
    pub(crate) fn state_value(&self, t: &str, state: &str) -> Option<Value> {
        self.type_desc(t)?.lifecycle.as_ref()?;
        match self.lifecycle_members.get(t) {
            Some(schema) => typed_state(schema, state),
            None => Some(Value::String(state.to_owned())),
        }
    }
}

/// The static gaps of every operation of `bundle` under `world`: `no_identity` on each
/// operation that writes a type with no identity (a read or list of it is not marked),
/// then its own `no_target` and `no_record`.
fn static_gaps(bundle: &Bundle, world: &World) -> HashMap<String, Vec<GapCode>> {
    let all = world_gaps::structural_gaps(bundle, world, &[]);
    let unidentified: HashSet<&str> = all
        .iter()
        .filter(|g| g.code == GapCode::NoIdentity)
        .filter_map(|g| g.r#type.as_deref())
        .collect();
    let mut out: HashMap<String, Vec<GapCode>> = HashMap::new();
    let none = OpDesc::default();
    for op in &bundle.operations {
        let d = world.operations.get(&op.id).unwrap_or(&none);
        let t = d.r#type.as_deref().or(op.resource_type.as_deref());
        let kind = d.kind.or_else(|| OpKind::of(op));
        if let (Some(t), Some(k)) = (t, kind)
            && is_write(k)
            && unidentified.contains(t)
        {
            out.entry(op.id.clone())
                .or_default()
                .push(GapCode::NoIdentity);
        }
    }
    for code in [GapCode::NoTarget, GapCode::NoRecord] {
        for g in all.iter().filter(|g| g.code == code) {
            if let Some(op) = &g.operation {
                let list = out.entry(op.clone()).or_default();
                if !list.contains(&code) {
                    list.push(code);
                }
            }
        }
    }
    out
}

fn is_write(k: OpKind) -> bool {
    matches!(
        k,
        OpKind::Create | OpKind::Update | OpKind::Replace | OpKind::Delete | OpKind::Action
    )
}

/// The schema of one record of type `t`: what a read of it answers where its identity
/// says, else the type's own definition.
fn record_schema(bundle: &Bundle, world: &World, t: &str) -> Option<Value> {
    let of_type = |op: &&mb_mcm::Operation| {
        world
            .operations
            .get(&op.id)
            .and_then(|d| d.r#type.as_deref())
            .or(op.resource_type.as_deref())
            == Some(t)
    };
    let read = bundle
        .operations
        .iter()
        .filter(of_type)
        .filter(|op| op.kind == OperationKind::Query)
        .find_map(|op| {
            let identity = op.identity.as_ref()?;
            let schema = op.success_output()?.schema.as_ref()?;
            match identity.record {
                None => Some(schema.clone()),
                Some(_) => super::bundle_index::record_schema_of(bundle, identity, schema),
            }
        });
    read.or_else(|| bundle.defs.get(t).cloned())
}

/// `state` as a value of the member `schema` declares, when it is one.
fn typed_state(schema: &Value, state: &str) -> Option<Value> {
    let declared: Vec<&str> = match schema.get("type") {
        Some(Value::String(t)) => vec![t.as_str()],
        Some(Value::Array(ts)) => ts
            .iter()
            .filter_map(Value::as_str)
            .filter(|t| *t != "null")
            .collect(),
        _ => Vec::new(),
    };
    let value = match declared.first().copied() {
        Some("boolean") => match state {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            _ => return None,
        },
        Some("integer") => Value::from(state.parse::<i64>().ok()?),
        Some("number") => match state.parse::<i64>() {
            Ok(n) => Value::from(n),
            Err(_) => Value::Number(serde_json::Number::from_f64(state.parse::<f64>().ok()?)?),
        },
        Some("string") | None => Value::String(state.to_owned()),
        Some(_) => return None,
    };
    match schema.get("enum").and_then(Value::as_array) {
        Some(allowed) if !allowed.contains(&value) => None,
        _ => Some(value),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use mb_engine::{Engine, EngineConfig, GeneratedResponse, Source};
    use mb_llm::Usage;
    use mb_state::MemoryStore;
    use serde_json::json;

    use super::*;
    use crate::MockRuntime;

    const FOLDERS: &str = r##"
openapi: 3.0.0
info: {title: Folders, version: "1"}
paths:
  /folders:
    post:
      operationId: createFolder
      requestBody: {content: {application/json: {schema: {$ref: "#/components/schemas/Folder"}}}}
      responses: {"201": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Folder"}}}}}
  /folders/{id}:
    parameters: [{name: id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getFolder
      responses: {"200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Folder"}}}}}
components:
  schemas:
    Folder:
      type: object
      properties:
        id: {type: string}
        archived: {type: boolean}
        state: {type: string}
        level: {type: integer}
        phase: {type: string, enum: [OPEN, SHUT]}
"##;

    fn bundle() -> Bundle {
        mb_import::compile(FOLDERS.as_bytes()).unwrap()
    }

    fn described_with(lifecycle_member: &str) -> Described {
        let b = bundle();
        let base = mb_mcm::world::tier0(&b);
        let t = b
            .operation("getFolder")
            .unwrap()
            .resource_type
            .clone()
            .unwrap();
        let doc: World = serde_json::from_value(json!({
            "types": {t: {"lifecycle": {"member": lifecycle_member}}}
        }))
        .unwrap();
        let (world, refused) = mb_mcm::world::effective(&b, &base, &doc);
        assert!(refused.is_empty(), "{refused:?}");
        let applied = mb_mcm::world::apply(&b, &base, &world);
        Described::new(&applied, &base, &world, 1)
    }

    fn folder_type() -> String {
        bundle()
            .operation("getFolder")
            .unwrap()
            .resource_type
            .clone()
            .unwrap()
    }

    #[test]
    fn state_value_follows_the_member_type() {
        let t = folder_type();
        assert_eq!(
            described_with("archived").state_value(&t, "true"),
            Some(json!(true))
        );
        assert_eq!(described_with("archived").state_value(&t, "ARCHIVED"), None);
        assert_eq!(
            described_with("state").state_value(&t, "ARCHIVED"),
            Some(json!("ARCHIVED"))
        );
        assert_eq!(described_with("level").state_value(&t, "3"), Some(json!(3)));
        assert_eq!(described_with("level").state_value(&t, "x"), None);
        assert_eq!(
            described_with("phase").state_value(&t, "OPEN"),
            Some(json!("OPEN"))
        );
        assert_eq!(described_with("phase").state_value(&t, "GONE"), None);
        // A type with no lifecycle has no states.
        assert_eq!(Described::default().state_value(&t, "true"), None);
    }

    fn answer(gaps: &[&str]) -> GeneratedResponse {
        GeneratedResponse {
            status: 200,
            content_type: None,
            body: None,
            source: Source::Llm,
            usage: Usage::default(),
            fallback_reason: None,
            effects: Vec::new(),
            effect_outcomes: Vec::new(),
            dropped_effects: 0,
            new_record: false,
            patched: None,
            headers: Vec::new(),
            world_gaps: gaps.iter().map(|g| (*g).to_owned()).collect(),
            exec_notes: serde_json::Map::new(),
        }
    }

    #[test]
    fn seed_world_gaps_extends_and_deduplicates() {
        let rt = MockRuntime::new(
            "folders",
            Arc::new(bundle()),
            Arc::new(Engine::new(None, EngineConfig::default())),
            Arc::new(MemoryStore::new(Duration::from_secs(60))),
        )
        .with_test_gaps(HashMap::from([(
            "getFolder".to_owned(),
            vec![GapCode::NoIdentity, GapCode::NoRecord],
        )]));
        let op = rt.bundle().operation("getFolder").unwrap().clone();
        let mut generated = answer(&["no_record"]);
        super::super::seed_world_gaps(&rt, &op, &mut generated);
        assert_eq!(generated.world_gaps, ["no_record", "no_identity"]);
        // Seeding twice adds nothing.
        super::super::seed_world_gaps(&rt, &op, &mut generated);
        assert_eq!(generated.world_gaps, ["no_record", "no_identity"]);
    }

    #[test]
    fn record_effects_writes_world_gap_without_effects() {
        let mut request = serde_json::Value::Null;
        super::super::record_effects(&mut request, &answer(&["no_target"]));
        assert_eq!(request, json!({"world_gap": ["no_target"]}));
        // A second answer recorded on the same request extends it, without repeats.
        super::super::record_effects(&mut request, &answer(&["no_target", "no_record"]));
        assert_eq!(request["world_gap"], json!(["no_target", "no_record"]));
        assert!(request.get("effects").is_none());
    }

    #[test]
    fn tier0_gives_no_world_gap_to_a_read_of_an_unidentified_type() {
        let b = mb_import::compile(
            br##"
openapi: 3.0.0
info: {title: Events, version: "1"}
paths:
  /events:
    post:
      operationId: createEvent
      x-mockingbird-resource: event
      requestBody: {content: {application/json: {schema: {type: object, properties: {kind: {type: string}}}}}}
      responses: {"202": {description: queued}}
"##,
        )
        .unwrap();
        let base = mb_mcm::world::tier0(&b);
        let d = Described::new(&b, &base, &base, 0);
        assert_eq!(d.gaps("createEvent"), [GapCode::NoIdentity]);
        assert_eq!(d.revision(), 0);
        assert!(d.world().is_some());
    }
}
