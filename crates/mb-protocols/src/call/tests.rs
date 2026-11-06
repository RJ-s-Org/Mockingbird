use super::*;
use futures::StreamExt;
use mb_engine::StreamItem;
use mb_engine::{Engine, EngineConfig, ScenarioSpec};
use mb_import::compile;
use mb_mcm::{Bundle, Identity, Protocol};
use mb_state::{MemoryStore, StateStore};
use serde_json::json;
use std::sync::Arc;

#[test]
fn an_either_or_phrase_offers_the_words_nearest_its_or() {
    let runs = |v: &[&str]| v.iter().map(|w| w.to_string()).collect::<Vec<_>>();
    assert_eq!(
        either_or(&["templateIdOrAlias"]),
        vec![[runs(&["templateid", "id"]), runs(&["alias"])]]
    );
    assert_eq!(
        either_or(&["The project's id or the slug"]),
        vec![[runs(&["projectid", "id"]), runs(&["slug"])]]
    );
    assert_eq!(
        either_or(&["id or order number"]),
        vec![[runs(&["id"]), runs(&["ordernumber", "order"])]]
    );
    // A negation, a mention, a name with no `or`: none.
    assert!(either_or(&["Not an id or a slug"]).is_empty());
    assert!(either_or(&["The order's id", " not its order number"]).is_empty());
    assert!(either_or(&["The user's id", " A username is not accepted"]).is_empty());
    assert!(either_or(&["widgetId"]).is_empty());
    assert!(either_or(&["or"]).is_empty());
}

#[test]
fn a_create_s_scope_is_noted_and_forgotten() {
    let collections = Collections::default();
    let at = SessionKey {
        workspace: "default".into(),
        mock: "scopes".into(),
        session: "s".into(),
    };
    let r = StoredResource {
        resource_type: "invoices".into(),
        id: "1".into(),
        data: json!({"id": "1"}),
    };
    let mut query = Map::new();
    query.insert("organization_id".into(), json!(" 42 "));
    query.insert("tag".into(), json!(["a", "b"]));
    query.insert("deep".into(), json!({"x": 1}));
    // Placed nowhere known: no scope.
    collections.note_scope(&at, &r, &query);
    assert_eq!(
        collections.carried(&at, &r, "organization_id"),
        ScopeOf::Unknown
    );
    collections.place_top_level(&at, &r);
    collections.note_scope(&at, &r, &query);
    assert_eq!(
        collections.carried(&at, &r, "organization_id"),
        ScopeOf::Carried(vec!["42".to_string()])
    );
    assert_eq!(
        collections.carried(&at, &r, "tag"),
        ScopeOf::Carried(vec!["a".to_string(), "b".to_string()])
    );
    assert_eq!(collections.carried(&at, &r, "deep"), ScopeOf::Unknown);
    // Changed by a write this runtime did not record: noted, no longer known.
    let changed = StoredResource {
        data: json!({"id": "1", "x": 2}),
        ..r.clone()
    };
    assert_eq!(
        collections.carried(&at, &changed, "organization_id"),
        ScopeOf::Noted
    );
    // This runtime's own write keeps it, as of the record now.
    collections.refresh_scope(&at, &changed);
    assert_eq!(
        collections.carried(&at, &changed, "organization_id"),
        ScopeOf::Carried(vec!["42".to_string()])
    );
    // So does a write placing it in a collection.
    let collection = Collection {
        template: "/orgs/{org}/invoices".into(),
        path: "/orgs/o/invoices".into(),
    };
    collections.record(&at, &r, collection.clone(), false);
    assert_eq!(
        collections.carried(&at, &r, "organization_id"),
        ScopeOf::Carried(vec!["42".to_string()])
    );
    // A list showing it changed behind the runtime's back leaves it noted only.
    let moved = StoredResource {
        data: json!({"id": "1", "x": 3}),
        ..r.clone()
    };
    collections.remember(&at, &moved, collection);
    assert_eq!(
        collections.carried(&at, &moved, "organization_id"),
        ScopeOf::Noted
    );
    // A delete ends it.
    collections.forget_id(&at, "invoices", "1");
    assert_eq!(
        collections.carried(&at, &r, "organization_id"),
        ScopeOf::Unknown
    );
}

#[test]
fn a_full_collections_record_drops_only_its_oldest() {
    let collections = Collections::default();
    let at = |n: usize| SessionKey {
        workspace: "default".into(),
        mock: "full".into(),
        session: format!("s{n}"),
    };
    let r = |n: usize| StoredResource {
        resource_type: "t".into(),
        id: n.to_string(),
        data: json!({"id": n}),
    };
    for n in 0..=MAX_COLLECTIONS {
        collections.place_top_level(&at(n), &r(n));
    }
    assert_eq!(collections.placement(&at(0), &r(0)), None);
    let last = MAX_COLLECTIONS;
    assert_eq!(collections.placement(&at(last), &r(last)), Some(None));
    assert_eq!(
        collections.placement(&at(last - 1), &r(last - 1)),
        Some(None)
    );
}

#[test]
fn siblings_are_one_level_under_the_parent() {
    let read = |t: &str| WrittenList {
        resource_type: t.to_string(),
        items: json!({"type": "object"}),
    };
    let lists: HashMap<&str, WrittenList> = HashMap::from([
        ("/lists/{list_id}/members", read("members")),
        ("/lists/{list_id}/links/members", read("members")),
        ("/lists/{list_id}/archived/members", read("members")),
        ("/lists/{list_id}/owners", read("owners")),
        ("/lists/{list_id}/members/{member_id}/notes", read("notes")),
        ("/lists", read("lists")),
        ("/teams/{team_id}/members", read("members")),
    ]);
    let of = |path: &str| -> Vec<(String, bool)> {
        siblings_of(path, &lists)
            .into_iter()
            .map(|s| (s.path, s.by_route))
            .collect()
    };
    // Its route names the members: it writes that list. Two segments down, or
    // another segment, is another list.
    assert_eq!(
        of("/lists/{id}/links/members"),
        vec![("/lists/{list_id}/members".to_string(), true)]
    );
    // On the parent itself: the lists one segment under it, by member only.
    assert_eq!(
        of("/lists/{list_id}"),
        vec![
            ("/lists/{list_id}/members".to_string(), false),
            ("/lists/{list_id}/owners".to_string(), false),
        ]
    );
    // A child's own route: its parent is the member, whose list is one level down.
    assert_eq!(
        of("/lists/{list_id}/members/{member_id}"),
        vec![(
            "/lists/{list_id}/members/{member_id}/notes".to_string(),
            false
        )]
    );
    // No parameter, no parent.
    assert!(of("/lists").is_empty());
}

#[test]
fn request_kind_follows_protocol() {
    assert_eq!(request_kind(Protocol::Http), RequestKind::Http);
    assert_eq!(request_kind(Protocol::Graphql), RequestKind::Graphql);
    assert_eq!(request_kind(Protocol::Grpc), RequestKind::Grpc);
    assert_eq!(request_kind(Protocol::Soap), RequestKind::Soap);
}

#[test]
fn credential_like_headers_are_sensitive_in_any_case() {
    for name in [
        "api_key",
        "apikey",
        "apiKey",
        "x-apikey",
        "x-api-key",
        "X-API-KEY",
        "x-api_key",
        "authorization",
        "Authorization",
        "proxy-authorization",
        "cookie",
        "Cookie",
        "x-cookie-jar",
        "x-auth-token",
        "my-secret",
        "x-password",
    ] {
        assert!(is_sensitive(name), "{name}");
    }
    for name in ["x-trace", "x-mockingbird-session", "accept", "user-agent"] {
        assert!(!is_sensitive(name), "{name}");
    }
}

#[test]
fn an_effect_type_matches_the_one_shown_type_of_its_stem() {
    let shown: Vec<String> = ["playlists", "message_reactions", "tasks", "Issues"]
        .into_iter()
        .map(String::from)
        .collect();
    for (written, t) in [
        ("playlist", "playlists"),
        ("Playlists", "playlists"),
        ("messageReaction", "message_reactions"),
        ("message-reaction", "message_reactions"),
        ("task", "tasks"),
        ("issue", "Issues"),
    ] {
        assert_eq!(stem_type(&shown, written, &[]), Some(t), "{written}");
    }
    assert_eq!(stem_type(&shown, "order", &[]), None);
    // Two shown types of one stem: no guess.
    let both: Vec<String> = vec!["entries".into(), "entry".into()];
    assert_eq!(stem_type(&both, "Entry", &[]), None);
}

#[test]
fn a_create_s_effects_copy_its_request_onto_no_sibling() {
    let effect = |t: &str, id: &str, set: Value| ResourceEffect {
        resource_type: t.into(),
        id: id.into(),
        set: set.as_object().cloned().unwrap_or_default(),
    };
    let sets = |v: &[ResourceEffect]| {
        v.iter()
            .map(|e| format!("{}:{}", e.id, Value::Object(e.set.clone())))
            .collect::<Vec<_>>()
    };
    let split = |body: Value, effects: Vec<ResourceEffect>| {
        let (kept, dropped) = sibling_effects("payments", Some("new"), Some(&body), effects);
        (sets(&kept), sets(&dropped))
    };
    // The amount alone, though `1` is the sibling's id.
    assert_eq!(
        split(
            json!({"amount": 1, "description": "Order X-2"}),
            vec![effect("payments", "1", json!({"amount": 1}))],
        ),
        (vec![], vec!["1:{\"amount\":1}".to_string()])
    );
    // An amount the request does not give, and a default it does not copy, apply.
    assert_eq!(
        split(
            json!({"amount": 9.99, "is_default": true}),
            vec![effect(
                "payments",
                "1",
                json!({"amount": 9.990, "is_default": false, "replies": 1})
            )],
        ),
        (
            vec!["1:{\"is_default\":false,\"replies\":1}".to_string()],
            vec!["1:{\"amount\":9.99}".to_string()]
        )
    );
    // A nested object holding a copied value goes whole, its other members too.
    assert_eq!(
        split(
            json!({"fields": {"parent": {"id": "tr_1"}, "note": "copied"}}),
            vec![effect(
                "payments",
                "tr_1",
                json!({"fields": {"parent": {"id": "tr_1"}, "note": "copied"}, "replies": 2})
            )],
        ),
        (
            vec!["tr_1:{\"replies\":2}".to_string()],
            vec![
                "tr_1:{\"fields\":{\"note\":\"copied\",\"parent\":{\"id\":\"tr_1\"}}}".to_string()
            ]
        )
    );
    // So does one holding a nested value of the request beside values of its own.
    assert_eq!(
        split(
            json!({"fields": {"parent": {"id": "tr_1"}, "note": "copied"}}),
            vec![effect(
                "payments",
                "tr_1",
                json!({"fields": {"parent": {"id": "tr_1"}, "note": "its own"}})
            )],
        )
        .0,
        Vec::<String>::new()
    );
    // The sibling's own id is copied as much as any value: a reply's `in_reply_to`
    // on its parent, a `parent_id` making a record its own parent, a `quantity` that
    // happens to equal an id. What the request does not give still applies.
    assert_eq!(
        split(
            json!({"amount": 3, "in_reply_to": 1_234_567}),
            vec![effect(
                "payments",
                "1234567",
                json!({"in_reply_to": 1_234_567, "replies": 1})
            )],
        ),
        (
            vec!["1234567:{\"replies\":1}".to_string()],
            vec!["1234567:{\"in_reply_to\":1234567}".to_string()]
        )
    );
    assert_eq!(
        split(
            json!({"parent_id": "tr_1"}),
            vec![effect("payments", "tr_1", json!({"parent_id": "tr_1"}))],
        ),
        (vec![], vec!["tr_1:{\"parent_id\":\"tr_1\"}".to_string()])
    );
    assert_eq!(
        split(
            json!({"amount": 2, "quantity": 1}),
            vec![effect("payments", "1", json!({"quantity": 1}))],
        ),
        (vec![], vec!["1:{\"quantity\":1}".to_string()])
    );
    // The created record and other types are left as they are.
    assert_eq!(
        split(
            json!({"amount": 1}),
            vec![
                effect("payments", "new", json!({"amount": 1})),
                effect("customers", "1", json!({"amount": 1})),
            ],
        ),
        (
            vec![
                "new:{\"amount\":1}".to_string(),
                "1:{\"amount\":1}".to_string()
            ],
            vec![]
        )
    );
}

#[test]
fn the_id_only_chooses_among_types_of_the_stem() {
    let shown: Vec<String> = ["entries", "entry", "playlists", "tracks"]
        .into_iter()
        .map(String::from)
        .collect();
    // Several of the stem: the one holding the id.
    assert_eq!(stem_type(&shown, "Entry", &["entry"]), Some("entry"));
    assert_eq!(stem_type(&shown, "Entry", &["entries", "entry"]), None);
    assert_eq!(stem_type(&shown, "Entry", &["tracks"]), None);
    // One of the stem, the id held elsewhere: they disagree.
    assert_eq!(stem_type(&shown, "playlist", &["tracks"]), None);
    assert_eq!(
        stem_type(&shown, "playlist", &["playlists"]),
        Some("playlists")
    );
    // Held nowhere: the stem's type, which then holds no such resource.
    assert_eq!(stem_type(&shown, "playlist", &[]), Some("playlists"));
    // No stem match: the id names no type.
    assert_eq!(stem_type(&shown, "song", &["tracks"]), None);
}

#[test]
fn a_stub_names_its_record_and_says_nothing_else() {
    let stored = json!({"id": "t1", "uri": "u:t1", "name": "Song", "plays": 3});
    let stub = |v: Value| v.as_object().unwrap().clone();
    let id = ["id", "number"];
    assert!(is_stub(&stub(json!({"id": "t1"})), &stored, &id));
    assert!(is_stub(
        &stub(json!({"id": "t1", "uri": "u:t1"})),
        &stored,
        &id
    ));
    // A link that is not the stored one, a member that is not a link, no identity.
    assert!(!is_stub(
        &stub(json!({"id": "t1", "uri": "u:x"})),
        &stored,
        &id
    ));
    assert!(!is_stub(
        &stub(json!({"id": "t1", "plays": 3})),
        &stored,
        &id
    ));
    assert!(!is_stub(&stub(json!({"uri": "u:t1"})), &stored, &id));
    // As much as the record: nothing to fill.
    assert!(!is_stub(&stub(stored.clone()), &stored, &id));
}

#[test]
fn credential_names_follow_data_plane_auth_changes() {
    use crate::{DataPlaneAuth, DataPlaneAuthMode};
    let bundle = Arc::new(compile(STREAM_CONTRACT.as_bytes()).unwrap());
    let mut rt = MockRuntime::new(
        "t",
        bundle,
        Arc::new(Engine::new(None, EngineConfig::default())),
        Arc::new(MemoryStore::new(Duration::from_secs(60))),
    );
    // Reading the names before auth is configured must not pin them.
    assert!(rt.credential_names().headers.is_empty());
    rt.set_data_plane_auth(DataPlaneAuth {
        mode: DataPlaneAuthMode::ApiKey,
        header: "x-tenant-pass".into(),
    });
    let mut headers = HeaderMap::new();
    headers.insert("x-tenant-pass", "k".parse().unwrap());
    headers.insert("x-trace", "t".parse().unwrap());
    let forwarded = forwarded_headers(&rt, &headers);
    assert!(!forwarded.contains_key("x-tenant-pass"), "{forwarded:?}");
    assert!(forwarded.contains_key("x-trace"), "{forwarded:?}");
    assert!(public_query(&rt, &[("api_key".into(), "k".into())]).is_empty());

    rt.set_data_plane_auth(DataPlaneAuth::default());
    assert!(rt.credential_names().headers.is_empty());
    assert!(forwarded_headers(&rt, &headers).contains_key("x-tenant-pass"));
}

#[test]
fn reference_fields() {
    assert_eq!(reference_prefix("customerId"), Some("customer"));
    assert_eq!(reference_prefix("customer_id"), Some("customer"));
    assert_eq!(reference_prefix("Id"), None);
    assert_eq!(reference_prefix("id"), None);
    assert_eq!(last_path_param("/a/{x}/b/{y}"), Some("y".into()));
    assert_eq!(last_path_param("/a"), None);
    assert_eq!(last_path_param("/a/{x}/b"), Some("x".into()));
    assert_eq!(last_path_param("/m/{Sid}.json"), Some("Sid".into()));
    assert_eq!(last_path_param("/c({id})"), Some("id".into()));
    assert_eq!(last_path_param("/c('{id}')/x"), Some("id".into()));
    assert_eq!(last_path_param("/carts/key={key}"), Some("key".into()));
    assert_eq!(last_path_param("/lines(doc={d},seq={s})"), None);
    assert_eq!(last_path_param("/lines({d},{s})"), None);
    assert_eq!(last_path_param("/c({id})/NS.f(asOf={date})"), None);
}

const STREAM_CONTRACT: &str = r#"
openapi: 3.1.0
info: {title: Ticks, version: "1"}
paths:
  /ticks:
    get:
      operationId: ticks
      responses:
        "200":
          description: OK
          content:
            application/json:
              schema:
                type: object
                required: [id, "n"]
                properties: {id: {type: string}, "n": {type: integer}}
"#;

/// A `StreamServer` operation over `ticks` whose item is
/// `{id: string, n: integer}`.
fn stream_runtime() -> Arc<MockRuntime> {
    let mut bundle: Bundle = compile(STREAM_CONTRACT.as_bytes()).unwrap();
    bundle.operations[0].kind = OperationKind::StreamServer;
    bundle.operations[0].resource_type = Some("ticks".into());
    let state: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    Arc::new(MockRuntime::new(
        "ticks",
        Arc::new(bundle),
        Arc::new(Engine::new(None, EngineConfig::default())),
        state,
    ))
}

fn session(name: &str) -> SessionKey {
    SessionKey {
        workspace: "default".into(),
        mock: "ticks".into(),
        session: name.into(),
    }
}

#[test]
fn cache_key_ignores_query_order_and_tracks_the_selection() {
    let rt = stream_runtime();
    let op = &rt.bundle.operations[0];
    let call = |q: Value| OperationCall {
        query: q.as_object().unwrap().clone(),
        ..OperationCall::default()
    };
    let a = call(json!({"a": 1, "b": 2}));
    let b = call(json!({"b": 2, "a": 1}));
    assert_eq!(
        cache_key(&rt.bundle, op, &a, None),
        cache_key(&rt.bundle, op, &b, None)
    );
    assert_ne!(
        cache_key(&rt.bundle, op, &a, None),
        cache_key(&rt.bundle, op, &call(json!({"a": 2, "b": 2})), None)
    );
    let narrow = json!({"type": "object", "properties": {"id": {"type": "string"}}});
    assert_ne!(
        cache_key(&rt.bundle, op, &a, None),
        cache_key(&rt.bundle, op, &a, Some(&narrow))
    );
}

#[test]
fn target_id_from_input_field() {
    let rt = stream_runtime();
    let mut op = rt.bundle.operations[0].clone();
    op.protocol = Protocol::Grpc;
    op.kind = OperationKind::Command(Effect::Update);
    op.identity = Some(Identity {
        path_param: None,
        field: "id".into(),
        input_field: Some("tickId".into()),
        path_field: None,
        record: None,
    });
    let call = OperationCall {
        operation_id: op.id.clone(),
        body: Some(json!({"tickId": 7, "n": 1})),
        ..OperationCall::default()
    };
    assert_eq!(target_id(&op, &call), Some("7".into()));
    let missing = OperationCall {
        body: Some(json!({"n": 1})),
        ..call
    };
    assert_eq!(target_id(&op, &missing), None);
}

/// A read a world description gave an identity by its kind alone (no path
/// parameter, no request member) names no target id, whatever the request
/// carries: it is generated as a read of an unheld id is.
#[test]
fn a_read_with_no_target_names_no_id() {
    let rt = stream_runtime();
    let mut op = rt.bundle.operations[0].clone();
    op.kind = OperationKind::Query;
    op.identity = Some(Identity {
        path_param: None,
        field: "id".into(),
        input_field: None,
        path_field: None,
        record: None,
    });
    let call = OperationCall {
        operation_id: op.id.clone(),
        path_params: [("id".to_owned(), json!("7"))].into_iter().collect(),
        body: Some(json!({"id": 7})),
        ..OperationCall::default()
    };
    assert_eq!(target_id(&op, &call), None);
}

#[tokio::test]
async fn change_feed_emits_upserts_for_resource_type() {
    let rt = stream_runtime();
    let op = rt.bundle.operations[0].clone();
    let mut feed = change_feed(&rt, session("a"), &op, None);
    let tick = |id: &str| StoredResource {
        resource_type: "ticks".into(),
        id: id.into(),
        data: json!({"id": id, "n": 1}),
    };
    // Another session, another type, and an invalid item: all ignored.
    rt.state.upsert(&session("b"), tick("t0")).await.unwrap();
    let other = StoredResource {
        resource_type: "tocks".into(),
        ..tick("t1")
    };
    rt.state.upsert(&session("a"), other).await.unwrap();
    let invalid = StoredResource {
        data: json!({"id": "t2"}),
        ..tick("t2")
    };
    rt.state.upsert(&session("a"), invalid).await.unwrap();
    rt.state.upsert(&session("a"), tick("t3")).await.unwrap();

    let got = tokio::time::timeout(Duration::from_secs(1), feed.next())
        .await
        .unwrap();
    assert_eq!(got, Some(json!({"id": "t3", "n": 1})));
    let none = tokio::time::timeout(Duration::from_millis(100), feed.next()).await;
    assert!(none.is_err(), "nothing else is emitted");
}

#[tokio::test]
async fn change_feed_is_empty_without_resource_type() {
    let rt = stream_runtime();
    let mut op = rt.bundle.operations[0].clone();
    op.resource_type = None;
    let mut feed = change_feed(&rt, session("a"), &op, None);
    assert_eq!(feed.next().await, None);
}

#[tokio::test]
async fn open_stream_applies_scenario_stream_settings() {
    let mut rt = stream_runtime();
    let scenario = ScenarioRuntime {
        spec: ScenarioSpec {
            name: "two".into(),
            instructions: None,
            set: Some(json!({"n": 42})),
            status: None,
        },
        stream_items: Some(2),
        stream_item_interval: Some(Duration::from_millis(1)),
        ..ScenarioRuntime::default()
    };
    Arc::get_mut(&mut rt)
        .unwrap()
        .scenarios
        .insert("two".into(), scenario);
    let op = &rt.bundle.operations[0];
    let mut headers = HeaderMap::new();
    headers.insert(SCENARIO_HEADER, "two".parse().unwrap());
    let plan = plan(&rt, op, &headers, Presented::default()).await.unwrap();
    assert_eq!(plan.stream_items, Some(2));
    let call = OperationCall {
        operation_id: op.id.clone(),
        ..OperationCall::default()
    };
    let items: Vec<StreamItem> = open_stream(&rt, &plan, call, Vec::new())
        .await
        .unwrap()
        .collect()
        .await;
    assert_eq!(items.len(), 2);
    assert!(items.iter().all(|i| i.value["n"] == 42));

    // The header beats the scenario.
    headers.insert(STREAM_ITEMS_HEADER, "3".parse().unwrap());
    let plan = super::plan(&rt, op, &headers, Presented::default())
        .await
        .unwrap();
    assert_eq!(plan.stream_items, Some(3));
}

/// An example contract, compiled.
fn example(path: &str) -> Bundle {
    let path = format!("{}/../../examples/apis/{path}", env!("CARGO_MANIFEST_DIR"));
    compile(&std::fs::read(path).unwrap()).unwrap()
}

fn runtime_of(bundle: Bundle) -> MockRuntime {
    let state: Arc<dyn StateStore> = Arc::new(MemoryStore::new(Duration::from_secs(3600)));
    MockRuntime::new(
        "m",
        Arc::new(bundle),
        Arc::new(Engine::new(None, EngineConfig::default())),
        state,
    )
}

/// What `next_numbers` used to compute per create.
fn scanned_path_fields<'a>(bundle: &'a Bundle, resource_type: &str) -> Vec<&'a str> {
    let mut fields: Vec<&str> = bundle
        .operations
        .iter()
        .filter(|o| o.resource_type.as_deref() == Some(resource_type))
        .filter_map(|o| o.identity.as_ref()?.path_field.as_deref())
        .collect();
    fields.sort_unstable();
    fields.dedup();
    fields
}

/// What `store_wrapped` used to compute per create.
fn scanned_listed(bundle: &Bundle) -> Vec<String> {
    bundle
        .operations
        .iter()
        .filter(|o| matches!(o.kind, OperationKind::Query))
        .filter_map(|o| o.success_output()?.schema.as_ref())
        .flat_map(|schema| list_members(bundle, schema))
        .collect()
}

/// A list under a member naming a type read by path is that type, and one a
/// record holds, of a schema no read serves, is of no type: not the record's,
/// and not one named for its member.
#[test]
fn a_listed_def_takes_its_members_type() {
    let path = format!(
        "{}/../../docs/benchmarks/contracts/travel.openapi.yaml",
        env!("CARGO_MANIFEST_DIR")
    );
    let rt = runtime_of(compile(&std::fs::read(path).unwrap()).unwrap());
    let index = rt.index();
    assert_eq!(index.types_by_def["Offer"], "offers");
    assert_eq!(index.types_by_def["FlightOffer"], "offers");
    assert_eq!(index.types_by_def.get("PassengerRecord"), None);
}

/// GitHub's `repository` is the detail route's type, not a list's.
#[test]
fn entity_types_of_the_github_contract() {
    let rt = runtime_of(example("github-rest/github-rest.openapi.yaml"));
    let index = rt.index();
    assert_eq!(index.types_by_def["repository"], "repos");
    assert_eq!(
        entity_type(index, Some("repository"), "ignored").as_deref(),
        Some("repos")
    );
    assert_eq!(entity_type(index, None, "repo").as_deref(), Some("repos"));
    assert_eq!(entity_type(index, None, "items"), None);
    assert_eq!(entity_type(index, None, "nothing"), None);
    assert_eq!(entity_type(index, None, ""), None);
}

/// A `<x>Id` finds a type of the contract whatever case either is named in, and
/// only one of the same stem.
#[test]
fn references_find_types_of_the_same_stem() {
    let contract = r##"
openapi: 3.0.0
info: {title: T, version: "1"}
paths:
  /event-types/{id}:
    parameters: [{name: id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getEventType
      responses: {"200": {description: ok, content: {application/json: {schema: {type: object, properties: {id: {type: string}}}}}}}
  /events/{id}:
    parameters: [{name: id, in: path, required: true, schema: {type: string}}]
    get:
      operationId: getEvent
      responses: {"200": {description: ok, content: {application/json: {schema: {type: object, properties: {id: {type: string}}}}}}}
"##;
    let rt = runtime_of(mb_import::compile(contract.as_bytes()).unwrap());
    let index = Some(rt.index());
    for field in ["eventTypeId", "event_type_id", "EventTypeId"] {
        let mut out = Vec::new();
        references_in(field, &json!("et_1"), 0, index, &mut out);
        let [(types, id)] = out.as_slice() else {
            panic!("{field}: {out:?}");
        };
        assert_eq!(id, "et_1");
        assert!(
            types.iter().any(|t| t == "event-types"),
            "{field}: {types:?}"
        );
        assert!(!types.iter().any(|t| t == "events"), "{field}: {types:?}");
    }
    assert_eq!(
        reference_types(None, "eventType"),
        ["eventTypes", "eventType"]
    );
}

/// What an LLM wrote into a log field is bounded, in characters.
#[test]
fn log_fields_are_capped() {
    assert_eq!(log_capped("ch_1"), "ch_1");
    let exact = "é".repeat(MAX_LOGGED_CHARS);
    assert_eq!(log_capped(&exact), exact);
    let long = log_capped(&"é".repeat(MAX_LOGGED_CHARS * 100));
    assert_eq!(long.chars().count(), MAX_LOGGED_CHARS + 1);
    assert!(long.ends_with('…'));
}

/// The runtime's index answers what scanning the bundle answers.
#[test]
fn bundle_index_matches_a_scan() {
    let mut bundles = vec![
        example("github-rest/github-rest.openapi.yaml"),
        example("slack/slack.openapi.yaml"),
    ];
    // A repeated id resolves to its first operation, as a scan does.
    let mut repeated = bundles[0].clone();
    let mut twin = repeated.operations[1].clone();
    twin.id = repeated.operations[0].id.clone();
    repeated.operations.push(twin);
    bundles.push(repeated);
    for bundle in bundles {
        let rt = runtime_of(bundle);
        let bundle = rt.bundle();
        let mut types = HashSet::new();
        for op in &bundle.operations {
            assert!(std::ptr::eq(
                rt.operation(&op.id).unwrap(),
                bundle.operation(&op.id).unwrap()
            ));
            let members = op
                .success_output()
                .and_then(|o| o.schema.as_ref())
                .map(|schema| list_members(bundle, schema))
                .unwrap_or_default();
            let i = rt.index().operation(&op.id).unwrap();
            if std::ptr::eq(&bundle.operations[i], op) {
                assert_eq!(rt.index().list_members(i), members.as_slice());
            }
            types.extend(op.resource_type.clone());
        }
        assert!(rt.operation("noSuchOperation").is_none());
        for t in types.iter().map(String::as_str).chain(["noSuchType"]) {
            let indexed: Vec<&str> = rt
                .index()
                .path_fields(t)
                .iter()
                .map(String::as_str)
                .collect();
            assert_eq!(indexed, scanned_path_fields(bundle, t), "{t}");
        }
        let scanned: HashSet<String> = scanned_listed(bundle).into_iter().collect();
        assert_eq!(rt.index().listed, scanned);
    }
}

/// The runtime's channel index finds what scanning the channels finds,
/// the first of a repeated id included.
#[test]
fn channel_index_matches_a_scan() {
    let mut bundle = example("kafka-events/commerce-events.asyncapi.yaml");
    assert!(bundle.channels.len() > 1);
    let mut twin = bundle.channels[1].clone();
    twin.id = bundle.channels[0].id.clone();
    bundle.channels.push(twin);
    let rt = runtime_of(bundle);
    let bundle = rt.bundle();
    for channel in &bundle.channels {
        assert!(std::ptr::eq(
            rt.channel(&channel.id).unwrap(),
            bundle.channel(&channel.id).unwrap()
        ));
    }
    assert!(rt.channel("noSuchChannel").is_none());
}

/// Timing harness: `cargo test -p mockingbird-protocols --lib --
/// --ignored --nocapture channel_index_timing`.
#[test]
#[ignore = "timing harness"]
fn channel_index_timing() {
    let events = example("kafka-events/commerce-events.asyncapi.yaml");
    for copies in [10usize, 100] {
        let mut bundle = events.clone();
        bundle.channels = (0..copies)
            .flat_map(|c| {
                events.channels.iter().map(move |ch| {
                    let mut ch = ch.clone();
                    ch.id = format!("{}{c}", ch.id);
                    ch
                })
            })
            .collect();
        let channels = bundle.channels.len();
        let last = bundle.channels[channels - 1].id.clone();
        let rt = runtime_of(bundle);
        let bundle = rt.bundle();
        let n = 20_000u32;
        let started = std::time::Instant::now();
        for _ in 0..n {
            std::hint::black_box(bundle.channel(std::hint::black_box(&last)));
        }
        let scan = started.elapsed() / n;
        let started = std::time::Instant::now();
        for _ in 0..n {
            std::hint::black_box(rt.channel(std::hint::black_box(&last)));
        }
        let indexed = started.elapsed() / n;
        println!("{channels} channels: lookup scan {scan:?}, index {indexed:?}");
    }
}

/// Timing harness: `cargo test -p mockingbird-protocols --lib --
/// --ignored --nocapture index_timing`.
#[test]
#[ignore = "timing harness"]
fn index_timing() {
    let github = example("github-rest/github-rest.openapi.yaml");
    for copies in [10usize, 100] {
        let mut bundle = github.clone();
        bundle.operations = (0..copies)
            .flat_map(|c| {
                github.operations.iter().map(move |op| {
                    let mut op = op.clone();
                    op.id = format!("{}{c}", op.id);
                    op
                })
            })
            .collect();
        let ops = bundle.operations.len();
        let last = bundle.operations[ops - 1].id.clone();
        let issue_type = bundle
            .operations
            .iter()
            .find_map(|o| {
                o.identity
                    .as_ref()?
                    .path_field
                    .as_ref()
                    .and(o.resource_type.clone())
            })
            .unwrap_or_default();
        let rt = runtime_of(bundle);
        let bundle = rt.bundle();
        let n = 2000u32;
        let time = |f: &dyn Fn()| {
            let started = std::time::Instant::now();
            for _ in 0..n {
                f();
            }
            started.elapsed() / n
        };
        let scan = time(&|| {
            std::hint::black_box(bundle.operation(std::hint::black_box(&last)));
        });
        let indexed = time(&|| {
            std::hint::black_box(rt.operation(std::hint::black_box(&last)));
        });
        println!("{ops} operations: lookup scan {scan:?}, index {indexed:?}");
        let scan = time(&|| {
            std::hint::black_box(scanned_path_fields(bundle, &issue_type));
            std::hint::black_box(scanned_listed(bundle));
        });
        let indexed = time(&|| {
            std::hint::black_box(rt.index().path_fields(&issue_type));
            std::hint::black_box(&rt.index().listed);
        });
        println!("{ops} operations: per-create walk {scan:?}, index {indexed:?}");
    }
}

/// Timing harness for what a gRPC call's plan used to copy (its
/// operation and scenario) against finding them again:
/// `cargo test -p mockingbird-protocols --lib -- --ignored --nocapture
/// owned_plan_timing`.
#[test]
#[ignore = "timing harness"]
fn owned_plan_timing() {
    let mut rt = runtime_of(example("github-rest/github-rest.openapi.yaml"));
    let ids: Vec<String> = rt
        .bundle()
        .operations
        .iter()
        .map(|o| o.id.clone())
        .collect();
    for variants in [2usize, 20] {
        let instructions = ids
            .iter()
            .take(variants)
            .map(|id| (id.clone(), "be terse".to_string()))
            .collect();
        let scenario = ScenarioRuntime {
            spec: ScenarioSpec {
                name: "s".into(),
                instructions: Some("x".repeat(500)),
                ..Default::default()
            },
            ..Default::default()
        }
        .with_operation_instructions(instructions);
        rt.scenarios.insert("s".into(), scenario);
        let op_id = &ids[0];
        let n = 2000u32;
        let started = std::time::Instant::now();
        for _ in 0..n {
            let op = rt.operation(op_id).unwrap().clone();
            let s = rt.scenarios["s"].for_operation(op_id).clone();
            std::hint::black_box((op, s));
        }
        let copied = started.elapsed() / n;
        let started = std::time::Instant::now();
        for _ in 0..n {
            let op = rt.operation(op_id).unwrap();
            let s = rt.scenarios.get("s").unwrap().for_operation(op_id);
            std::hint::black_box((op, s));
        }
        let found = started.elapsed() / n;
        println!("{variants} scenario variants: copy {copied:?}, find again {found:?}");
    }
}

/// Timing harness for the copies an offline bidi message (20 earlier
/// bodies and its own) and a consumed event without a recorder (its
/// payload and headers) no longer make: `cargo test -p
/// mockingbird-protocols --lib -- --ignored --nocapture
/// skipped_copies_timing`.
#[test]
#[ignore = "timing harness"]
fn skipped_copies_timing() {
    let body = |fields: usize| -> Value {
        (0..fields)
            .map(|i| (format!("field{i}"), json!({"text": "some words", "n": i})))
            .collect::<Map<String, Value>>()
            .into()
    };
    let n = 1000u32;
    for fields in [20usize, 200] {
        let earlier: Vec<Value> = (0..20).map(|_| body(fields)).collect();
        let current = body(fields);
        let started = std::time::Instant::now();
        for _ in 0..n {
            let copies: Vec<Value> = earlier.to_vec();
            std::hint::black_box((copies, current.clone()));
        }
        let size = current.to_string().len();
        println!(
            "bidi message of {size} bytes: {:?} of copies",
            started.elapsed() / n
        );
    }
    let headers: Vec<(String, String)> = (0..10)
        .map(|i| (format!("x-header-{i}"), "value".repeat(4)))
        .collect();
    for fields in [200usize, 2000] {
        let payload = body(fields);
        let started = std::time::Instant::now();
        for _ in 0..n {
            let map: Map<String, Value> = headers
                .iter()
                .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                .collect();
            std::hint::black_box((map, payload.clone()));
        }
        let size = payload.to_string().len();
        println!(
            "consumed message of {size} bytes: {:?} of copies",
            started.elapsed() / n
        );
    }
}

#[test]
fn canonical_hash_ignores_member_order_and_tells_values_apart() {
    use std::hash::{DefaultHasher, Hasher};
    let hash = |v: &Value| {
        let mut hasher = DefaultHasher::new();
        hash_canonical(v, &mut hasher);
        hasher.finish()
    };
    let mut forward = Map::new();
    forward.insert("a".into(), json!(1));
    forward.insert("b".into(), json!({"y": [1, 2], "x": null}));
    let mut backward = Map::new();
    backward.insert("b".into(), json!({"x": null, "y": [1, 2]}));
    backward.insert("a".into(), json!(1));
    assert_eq!(
        hash(&Value::Object(forward)),
        hash(&Value::Object(backward))
    );
    let distinct = [
        json!({"a": [1, 2]}),
        json!({"a": [2, 1]}),
        json!({"a": "1"}),
        json!({"a": 1}),
        json!({"a": null}),
        json!({}),
        json!([["a", 1]]),
        json!({"a": {"b": 1}}),
        json!({"a": [{"b": 1}]}),
    ];
    let hashes: HashSet<u64> = distinct.iter().map(hash).collect();
    assert_eq!(hashes.len(), distinct.len());
}

/// Counts `get` and `list` calls on a memory store.
struct ListSpy {
    inner: MemoryStore,
    gets: std::sync::atomic::AtomicUsize,
    lists: std::sync::atomic::AtomicUsize,
}

impl ListSpy {
    fn gets(&self) -> usize {
        self.gets.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn lists(&self) -> usize {
        self.lists.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl StateStore for ListSpy {
    async fn get(
        &self,
        s: &SessionKey,
        t: &str,
        id: &str,
    ) -> Result<Option<StoredResource>, StateError> {
        self.gets.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.inner.get(s, t, id).await
    }
    async fn list(
        &self,
        s: &SessionKey,
        t: &str,
        limit: usize,
    ) -> Result<Vec<StoredResource>, StateError> {
        self.lists.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.inner.list(s, t, limit).await
    }
    async fn upsert(&self, s: &SessionKey, r: StoredResource) -> Result<(), StateError> {
        self.inner.upsert(s, r).await
    }
    async fn delete(&self, s: &SessionKey, t: &str, id: &str) -> Result<bool, StateError> {
        self.inner.delete(s, t, id).await
    }
    async fn reset(&self, s: &SessionKey) -> Result<(), StateError> {
        self.inner.reset(s).await
    }
    async fn reset_mock(&self, w: &str, m: &str) -> Result<(), StateError> {
        self.inner.reset_mock(w, m).await
    }
    async fn touch(&self, s: &SessionKey) -> Result<(), StateError> {
        self.inner.touch(s).await
    }
    async fn sessions(&self, w: &str, m: &str) -> Result<Vec<mb_state::SessionInfo>, StateError> {
        self.inner.sessions(w, m).await
    }
    async fn export(&self, s: &SessionKey) -> Result<Vec<StoredResource>, StateError> {
        self.inner.export(s).await
    }
    async fn seed(&self, s: &SessionKey, r: Vec<StoredResource>) -> Result<(), StateError> {
        self.inner.seed(s, r).await
    }
    async fn ping(&self) -> Result<(), StateError> {
        Ok(())
    }
    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<mb_state::StateChanged> {
        self.inner.subscribe()
    }
}

/// GitHub's runtime over a [`ListSpy`], with its read of an issue by
/// number.
fn issue_reads() -> (Arc<ListSpy>, MockRuntime, Operation) {
    let spy = Arc::new(ListSpy {
        inner: MemoryStore::new(Duration::from_secs(3600)),
        gets: Default::default(),
        lists: Default::default(),
    });
    let rt = MockRuntime::new(
        "m",
        Arc::new(example("github-rest/github-rest.openapi.yaml")),
        Arc::new(Engine::new(None, EngineConfig::default())),
        spy.clone(),
    );
    let op = rt
        .bundle()
        .operations
        .iter()
        .find(|o| o.identity.as_ref().is_some_and(|i| i.path_field.is_some()))
        .cloned()
        .unwrap();
    (spy, rt, op)
}

fn issue(op: &Operation, id: &str, number: u64) -> StoredResource {
    StoredResource {
        resource_type: op.resource_type.clone().unwrap(),
        id: id.into(),
        data: json!({"id": id, "number": number, "title": format!("issue {number}")}),
    }
}

/// A read answered from the store never reaches a prompt, so it looks up
/// its target and nothing the target refers to.
/// The search for a parent by name is a generation's: a read the store
/// answers makes no scan, and a generation one, then none.
#[cfg(feature = "http-rest")]
#[tokio::test]
async fn only_a_generation_scans_for_a_parent_by_name() {
    use tower::ServiceExt;
    const CONTRACT: &str = r##"
openapi: 3.0.0
info: {title: Nested, version: "1"}
paths:
  /repos/{repo}:
    get:
      operationId: getRepo
      parameters: [{name: repo, in: path, required: true, schema: {type: string}}]
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Repo"}}}}
  /repos/{repo}/issues/{id}:
    get:
      operationId: getIssue
      parameters:
        - {name: repo, in: path, required: true, schema: {type: string}}
        - {name: id, in: path, required: true, schema: {type: string}}
      responses:
        "200": {description: ok, content: {application/json: {schema: {$ref: "#/components/schemas/Issue"}}}}
components:
  schemas:
    Repo:
      type: object
      required: [id, name]
      properties: {id: {type: string}, name: {type: string}}
    Issue:
      type: object
      required: [id]
      properties: {id: {type: string}}
"##;
    let spy = Arc::new(ListSpy {
        inner: MemoryStore::new(Duration::from_secs(3600)),
        gets: Default::default(),
        lists: Default::default(),
    });
    let rt = MockRuntime::new(
        "m",
        Arc::new(compile(CONTRACT.as_bytes()).unwrap()),
        Arc::new(Engine::new(None, EngineConfig::default())),
        spy.clone(),
    );
    let s = SessionKey {
        workspace: "default".into(),
        mock: "m".into(),
        session: DEFAULT_SESSION.into(),
    };
    for (t, id, data) in [
        ("repos", "7", json!({"id": "7", "name": "widget"})),
        ("issues", "i1", json!({"id": "i1"})),
    ] {
        let stored = StoredResource {
            resource_type: t.into(),
            id: id.into(),
            data,
        };
        spy.inner.upsert(&s, stored).await.unwrap();
    }
    let router = crate::http_rest::router(Arc::new(rt));
    let get = |uri: &'static str| {
        let router = router.clone();
        async move {
            let request = axum::http::Request::get(uri)
                .body(axum::body::Body::empty())
                .unwrap();
            router.oneshot(request).await.unwrap()
        }
    };
    let served = get("/repos/widget/issues/i1").await;
    assert_eq!(served.headers()["x-mockingbird-source"], "state");
    assert_eq!(spy.lists(), 0);
    get("/repos/widget/issues/new1").await;
    assert_eq!(spy.lists(), 1);
    get("/repos/widget/issues/new2").await;
    assert_eq!(spy.lists(), 1);
}

#[cfg(feature = "http-rest")]
#[tokio::test]
async fn a_served_read_makes_one_get() {
    use tower::ServiceExt;
    const CONTRACT: &str = r##"
openapi: 3.0.0
info: {title: Served, version: "1"}
paths:
  /customers/{customer}:
    get:
      operationId: getCustomer
      parameters: [{name: customer, in: path, required: true, schema: {type: string}}]
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema:
                type: object
                required: [id]
                properties: {id: {type: string}, offerId: {type: string}, customer: {type: string}}
"##;
    let spy = Arc::new(ListSpy {
        inner: MemoryStore::new(Duration::from_secs(3600)),
        gets: Default::default(),
        lists: Default::default(),
    });
    let rt = MockRuntime::new(
        "m",
        Arc::new(compile(CONTRACT.as_bytes()).unwrap()),
        Arc::new(Engine::new(None, EngineConfig::default())),
        spy.clone(),
    );
    let s = SessionKey {
        workspace: "default".into(),
        mock: "m".into(),
        session: DEFAULT_SESSION.into(),
    };
    // `customer` is the member the path names, so it holds the path's value.
    let data = json!({"id": "c1", "offerId": "of1", "customer": "c1"});
    let stored = StoredResource {
        resource_type: "customers".into(),
        id: "c1".into(),
        data,
    };
    spy.inner.upsert(&s, stored).await.unwrap();
    let request = axum::http::Request::get("/customers/c1")
        .body(axum::body::Body::empty())
        .unwrap();
    let response = crate::http_rest::router(Arc::new(rt))
        .oneshot(request)
        .await
        .unwrap();
    assert_eq!(response.headers()["x-mockingbird-source"], "state");
    assert_eq!(spy.gets(), 1);
    assert_eq!(spy.lists(), 0);
}

async fn read_by(rt: &MockRuntime, op: &Operation, number: &str) -> Option<String> {
    let mut state = StateAccess::new(rt);
    let (target, _, _) = read_context(
        &mut state,
        &session("s"),
        op,
        Some(number),
        &OperationCall::default(),
    )
    .await;
    target.map(|t| t.id)
}

#[tokio::test]
async fn a_read_by_path_field_finds_this_runtimes_writes_without_a_scan() {
    let (spy, rt, op) = issue_reads();
    let field = op.identity.as_ref().unwrap().path_field.as_deref();
    assert_eq!(field, Some("number"));
    let t = op.resource_type.clone().unwrap();
    let s = session("s");
    let mut state = StateAccess::new(&rt);
    assert!(state.upsert(&s, issue(&op, "I_1", 7)).await);
    assert!(state.insert_seeded(&s, issue(&op, "I_2", 8)).await);
    assert_eq!(read_by(&rt, &op, "7").await.as_deref(), Some("I_1"));
    assert_eq!(read_by(&rt, &op, "8").await.as_deref(), Some("I_2"));
    assert_eq!(spy.lists(), 0);

    // Renumbered behind the runtime's back: the alias no longer holds,
    // so the read scans like before, and remembers what it found.
    rt.state.upsert(&s, issue(&op, "I_1", 9)).await.unwrap();
    assert_eq!(read_by(&rt, &op, "7").await, None);
    assert_eq!(spy.lists(), 1);
    assert_eq!(read_by(&rt, &op, "9").await.as_deref(), Some("I_1"));
    assert_eq!(spy.lists(), 2);
    assert_eq!(read_by(&rt, &op, "9").await.as_deref(), Some("I_1"));
    assert_eq!(spy.lists(), 2);

    // Deleted or reset: the alias names nothing.
    rt.state.delete(&s, &t, "I_1").await.unwrap();
    assert_eq!(read_by(&rt, &op, "9").await, None);
    rt.state.reset(&s).await.unwrap();
    assert_eq!(read_by(&rt, &op, "8").await, None);
    // Another session does not see this one's aliases.
    let mut state = StateAccess::new(&rt);
    assert!(state.upsert(&session("other"), issue(&op, "I_3", 3)).await);
    assert_eq!(read_by(&rt, &op, "3").await, None);
}

#[test]
fn path_aliases_are_bounded() {
    let aliases = PathAliases::default();
    let s = session("s");
    for i in 0..(MAX_PATH_ALIASES + 10) {
        let values = vec![("number", i.to_string())];
        aliases.remember(&s, "issues", &format!("I_{i}"), values);
        assert!(aliases.len() <= MAX_PATH_ALIASES);
    }
    let last = MAX_PATH_ALIASES + 9;
    assert_eq!(
        aliases.id_of(&s, "issues", "number", &last.to_string()),
        Some(format!("I_{last}"))
    );
}

/// Timing harness for a read by path field, scanning (as before) and
/// through [`PathAliases`]: `cargo test -p mockingbird-protocols --lib
/// --release -- --ignored --nocapture path_read_timing`.
#[tokio::test(flavor = "current_thread")]
#[ignore = "timing harness"]
async fn path_read_timing() {
    let (_, rt, op) = issue_reads();
    let s = session("s");
    let t = op.resource_type.clone().unwrap();
    let mut stored = 0;
    for n in [50usize, 500] {
        let mut state = StateAccess::new(&rt);
        for i in stored..n {
            let mut r = issue(&op, &format!("I_{i}"), i as u64);
            r.data["body"] = json!("y".repeat(1500));
            r.data["user"] = json!({"login": "octocat", "id": 1});
            state.upsert(&s, r).await;
        }
        stored = n;
        let iters = 500u32;
        let started = std::time::Instant::now();
        for _ in 0..iters {
            let _ = rt.state.get(&s, &t, "3").await;
            let found = rt
                .state
                .list(&s, &t, MAX_SCAN)
                .await
                .unwrap()
                .into_iter()
                .find(|r| r.data.get("number").and_then(id_string).as_deref() == Some("3"));
            std::hint::black_box(found);
        }
        let scan = started.elapsed() / iters;
        let started = std::time::Instant::now();
        for _ in 0..iters {
            std::hint::black_box(read_by(&rt, &op, "3").await);
        }
        let aliased = started.elapsed() / iters;
        println!("{n} issues stored: read by number, scan {scan:?}, alias {aliased:?}");
    }
}

#[path = "nested_write_tests.rs"]
mod nested_writes;
