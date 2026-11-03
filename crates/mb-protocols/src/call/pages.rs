//! The rest of a list answer cut to its page size, served by a token.
//!
//! [`lists::narrow`] cuts a generated list to the page size a request asks for. A
//! client then asks for the next page by the token the answer carried, which the model
//! made up: the request repeats the first's, or names a token no list knows, and the
//! answer is the first page again (from the cache) or a list made up anew. So when a cut
//! leaves items out, and the contract has a place for a token, the engine mints one.
//! A page the model answered full, at the size asked, with its token absent, `null` or
//! empty, while the session holds records the query asks for that it does not show, is
//! a cut too: those records are its rest ([`lists::stored_rest`]), after any the page
//! size cut. A token the model gave stays, with its count, as it leads to the model's own
//! next page. Such a cut is kept, capped and ended like any other (at most [`MAX_CUTS`]
//! a session). Its records are stored already, so they are not seeded again. A count of
//! the list beside the token that counts fewer than the page and its rest (see
//! [`deleted::with_count_at_least`]) is raised to them, and a boolean beside the token,
//! in the same object, that is `false` is `true` where the answer validates so
//! (`has_more`), as the last page sets it back.
//!
//! The place is one text member of the answer (no `const` or `enum`), at its top level
//! or inside an object member beside the list, whose name is, or ends with the words
//! of, a text query parameter of the route other than the page size: Zoom's
//! `next_page_token` for `next_page_token`, Square's `cursor` for `cursor`, Pipedrive's
//! `additional_data.next_cursor` for `cursor`. A pair is a cursor only when the request
//! does not carry the parameter, or carries it empty (`cursor=`: a page is asked for
//! without one first), the parameter
//! names no member of the items (`status` filters them), and what the model put in the
//! member echoes no value of the request (a search's `query`). Exactly one pair, or none
//! is used; nor is a token written that leaves the answer invalid. With none, the
//! answer is as [`lists::narrow`] left it.
//!
//! A cut is kept once, as one document under the store's reserved type
//! ([`mb_state::INTERNAL_TYPE`]), like the deleted markers: the route, the path's
//! values, the request's other query values, the token's place and parameter, the page
//! size, the page as answered, and the items left out that no delete marker covers (at
//! most [`MAX_REST`]). They are seeded as a list's items are, so a read of one finds it.
//! A session keeps at most [`MAX_CUTS`] cuts: each goes in one of that many places,
//! picked by a counter this process shares across sessions, so nothing is read to count
//! them. A new cut ends whichever of the session's cuts held its place, which is not
//! always its oldest when other sessions mint meanwhile, and two nodes may pick the same
//! place: either only ends a cut early, whose token then goes to the model. The
//! session's TTL ends them all. A token is the cut's nonce, its place and the offset of the next item, in
//! [`TOKEN_LEN`] hex digits: the same token always serves the same page and the same
//! next token, and serving one writes nothing.
//!
//! A request on the same route and path, with the same other query values, carrying a
//! token of a kept cut under its parameter is served without a model call: the next
//! items, as many as the request's page size or the first's, each showing its stored
//! record (see [`lists::overlay_stored`]), then the query's filters
//! ([`lists::narrow`]) and the delete markers ([`deleted::drop_from_list`]) applied
//! again, as the session may have written since; and the next token when more remain.
//! The last page's token is `null`, left out or empty, whichever the answer validates
//! with; with none, the request is the model's. A boolean beside the token, in the same
//! object, that the first page set `true` (`has_more`) is `false` on the last page when
//! the answer validates so, and else as it was. A token with other filter values, of
//! a cut since ended, or of another route, is the model's too. Its query differs from
//! the first page's by the token, so the response cache never answers it with the
//! first page; it is not cached itself, as the token already serves it.
//!
//! An LLM and `consistency: on` only, as [`deleted::applies`] says, and plain calls
//! (no scenario or forced status).

use std::sync::atomic::{AtomicU64, Ordering};

use mb_engine::consistency::{list_items_mut, list_member};
use mb_state::INTERNAL_TYPE;
use sha2::{Digest, Sha256};

use super::*;

/// Cuts kept per session, the newest.
const MAX_CUTS: u64 = 64;
/// Items left out that a cut keeps, at most.
const MAX_REST: usize = 500;
/// Hex digits of a token: the nonce's, the place's and the offset's.
const TOKEN_LEN: usize = 40;
const NONCE_LEN: usize = 28;
const PLACE_LEN: usize = 4;
/// The prefix of a cut's document.
const PREFIX: &str = "#cut/";

/// Cuts minted by this process: which place each takes, and what makes each nonce
/// unlike the last.
static MINTED: AtomicU64 = AtomicU64::new(0);

/// Where an answer of `op` carries its token: the member's path from the top, and the
/// query parameter the token comes back under.
pub(super) struct Slot {
    pub(super) path: Vec<String>,
    param: String,
}

/// Whether member `name` is, or ends with the words of, parameter `param`.
fn names_param(name: &str, param: &str) -> bool {
    let wanted = mb_mcm::joined_name(param);
    let words = mb_mcm::words(name);
    (0..words.len()).any(|from| mb_mcm::joined_name(&words[from..].concat()) == wanted)
}

/// Whether `schema` is text only, with no `const` or `enum`.
fn is_free_text(bundle: &mb_mcm::Bundle, schema: &Value) -> bool {
    let (parts, _) = schema_parts(bundle, Some(schema));
    lists::types_of(&parts).into_iter().eq(["string"])
        && !parts
            .iter()
            .any(|p| p.get("const").is_some() || p.get("enum").is_some())
}

/// The text values `call` holds in its path and query, empty ones aside.
fn request_values(call: &OperationCall) -> Vec<String> {
    let mut out = Vec::new();
    for value in call.path_params.values().chain(call.query.values()) {
        match value {
            Value::Array(values) => out.extend(values.iter().filter_map(id_string)),
            other => out.extend(id_string(other)),
        }
    }
    out.retain(|value| !value.trim().is_empty());
    out
}

/// Whether `call` carries query parameter `name` with a value: `cursor=` carries none.
fn carries(call: &OperationCall, name: &str) -> bool {
    call.query.get(name).is_some_and(|value| match value {
        Value::Null => false,
        Value::String(text) => !text.trim().is_empty(),
        _ => true,
    })
}

/// `body` with each boolean beside `slot`, in the same object, that `first` holds
/// `true` set to `false`: a `has_more` the first page set, on the last.
fn without_more(body: &Value, first: &Value, slot: &[String]) -> Option<Value> {
    let (_, parents) = slot.split_last()?;
    let object_at = |value: &Value| {
        parents
            .iter()
            .try_fold(value.clone(), |at, name| at.get(name).cloned())
    };
    let flags: Vec<String> = object_at(first)?
        .as_object()?
        .iter()
        .filter(|(_, value)| value.as_bool() == Some(true))
        .map(|(name, _)| name.clone())
        .collect();
    if flags.is_empty() {
        return None;
    }
    let mut out = body.clone();
    let mut at = &mut out;
    for name in parents {
        at = at.get_mut(name)?;
    }
    let object = at.as_object_mut()?;
    for flag in flags {
        if object.get(&flag).and_then(Value::as_bool) == Some(true) {
            object.insert(flag, Value::Bool(false));
        }
    }
    Some(out)
}

/// `body` with each boolean beside `slot`, in the same object, that is `false` set to
/// `true`: a `has_more` beside a minted token, as [`without_more`] ends it on the last
/// page. `None` when there is none.
fn with_more(body: &Value, slot: &[String]) -> Option<Value> {
    let (_, parents) = slot.split_last()?;
    let mut out = body.clone();
    let mut at = &mut out;
    for name in parents {
        at = at.get_mut(name)?;
    }
    let mut changed = false;
    for value in at.as_object_mut()?.values_mut() {
        if value.as_bool() == Some(false) {
            *value = Value::Bool(true);
            changed = true;
        }
    }
    changed.then_some(out)
}

/// The one place answer `body` to `call` has for a token (see the module's doc).
pub(super) fn slot(
    bundle: &mb_mcm::Bundle,
    (op, call): (&Operation, &OperationCall),
    body: &Value,
    size_param: &str,
) -> Option<Slot> {
    let list = list_member(body)?;
    let schema = op.success_output()?.schema.as_ref()?;
    let mut held = None;
    let (_, item_parts) = lists::list_parts(bundle, schema, body, &mut held)?;
    let declared = op.input.query.as_ref()?.get("properties")?.as_object()?;
    let params: Vec<&str> = declared
        .iter()
        .filter(|(name, schema)| {
            name.as_str() != size_param
                && !mb_mcm::is_generic_id(name)
                && !carries(call, name)
                && lists::named_member(bundle, &item_parts, name).is_none()
                && is_free_text(bundle, schema)
        })
        .map(|(name, _)| name.as_str())
        .collect();
    let echoed = request_values(call);
    let param_of = |name: &str, generated: Option<&Value>| -> Option<String> {
        if generated
            .and_then(id_string)
            .is_some_and(|value| echoed.contains(&value))
        {
            return None;
        }
        let mut named = params.iter().filter(|param| names_param(name, param));
        let first = named.next()?;
        named.next().is_none().then(|| (*first).to_string())
    };
    let (parts, _) = schema_parts(bundle, Some(schema));
    let mut found: Vec<Slot> = Vec::new();
    for part in &parts {
        let Some(members) = part.get("properties").and_then(Value::as_object) else {
            continue;
        };
        for (name, member) in members.iter().filter(|(name, _)| name.as_str() != list) {
            if is_free_text(bundle, member) {
                if let Some(param) = param_of(name, body.get(name)) {
                    found.push(Slot {
                        path: vec![name.clone()],
                        param,
                    });
                }
                continue;
            }
            let (inner, _) = schema_parts(bundle, Some(member));
            for (inner_name, inner_member) in inner
                .iter()
                .filter_map(|p| p.get("properties").and_then(Value::as_object))
                .flatten()
            {
                let generated = body.get(name).and_then(|outer| outer.get(inner_name));
                if is_free_text(bundle, inner_member)
                    && let Some(param) = param_of(inner_name, generated)
                {
                    found.push(Slot {
                        path: vec![name.clone(), inner_name.clone()],
                        param,
                    });
                }
            }
        }
    }
    let first = found.pop()?;
    found.is_empty().then_some(first)
}

/// `body` with `value` at `path`, an object made for it where its parent is missing or
/// `null`; `None` for no value, which takes the member out.
fn placed(body: &Value, path: &[String], value: Option<Value>) -> Option<Value> {
    let mut out = body.clone();
    let (last, parents) = path.split_last()?;
    let mut at = &mut out;
    for name in parents {
        let object = at.as_object_mut()?;
        let slot = object
            .entry(name.clone())
            .or_insert_with(|| Value::Object(Map::new()));
        if slot.is_null() {
            *slot = Value::Object(Map::new());
        }
        at = slot;
    }
    let object = at.as_object_mut()?;
    match value {
        Some(value) => {
            object.insert(last.clone(), value);
        }
        None => {
            object.remove(last);
        }
    }
    Some(out)
}

/// A nonce not minted before, and the place its cut takes.
fn new_cut(session: &SessionKey, op: &Operation) -> (String, u64) {
    let count = MINTED.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let mut hasher = Sha256::new();
    for part in [
        session.workspace.as_str(),
        session.mock.as_str(),
        session.session.as_str(),
        op.id.as_str(),
        &count.to_string(),
        &nanos.to_string(),
    ] {
        hasher.update(part.as_bytes());
        hasher.update([0]);
    }
    let digest = hasher.finalize();
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    (hex[..NONCE_LEN].to_string(), count % MAX_CUTS)
}

/// The token of the item at `offset` of the cut `nonce` names in `place`.
fn token(nonce: &str, place: u64, offset: usize) -> String {
    format!("{nonce}{place:04x}{offset:08x}")
}

/// A token's nonce, place and offset, when `text` is one.
fn read_token(text: &str) -> Option<(&str, u64, usize)> {
    if text.len() != TOKEN_LEN || !text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return None;
    }
    let (nonce, rest) = text.split_at(NONCE_LEN);
    let (place, offset) = rest.split_at(PLACE_LEN);
    let place = u64::from_str_radix(place, 16).ok()?;
    let offset = usize::from_str_radix(offset, 16).ok()?;
    (place < MAX_CUTS).then_some((nonce, place, offset))
}

fn place_key(place: u64) -> String {
    format!("{PREFIX}{place:04x}")
}

/// The query values a token is bound to: the request's, but the token's and the page
/// size's.
fn bound_query(call: &OperationCall, param: &str, size_param: &str) -> Value {
    let mut query = call.query.clone();
    query.remove(param);
    query.remove(size_param);
    canonical(&Value::Object(query))
}

/// A kept cut.
struct Cut {
    nonce: String,
    op: String,
    path: Value,
    query: Value,
    slot: Vec<String>,
    param: String,
    size: usize,
    size_param: String,
    /// The page as answered, with its token.
    page: Value,
    rest: Vec<Value>,
}

impl Cut {
    fn to_value(&self) -> Value {
        serde_json::json!({
            "nonce": self.nonce,
            "op": self.op,
            "path": self.path,
            "query": self.query,
            "slot": self.slot,
            "param": self.param,
            "size": self.size,
            "size_param": self.size_param,
            "page": self.page,
            "rest": self.rest,
        })
    }

    fn of(mut value: Value) -> Option<Self> {
        let text = |value: &Value, key: &str| value.get(key)?.as_str().map(str::to_string);
        let rest = match value.get_mut("rest")?.take() {
            Value::Array(items) => items,
            _ => return None,
        };
        Some(Self {
            nonce: text(&value, "nonce")?,
            op: text(&value, "op")?,
            path: value.get("path")?.clone(),
            query: value.get("query")?.clone(),
            slot: value
                .get("slot")?
                .as_array()?
                .iter()
                .map(|s| s.as_str().map(str::to_string))
                .collect::<Option<_>>()?,
            param: text(&value, "param")?,
            size: usize::try_from(value.get("size")?.as_u64()?).ok()?,
            size_param: text(&value, "size_param")?,
            page: value.get_mut("page")?.take(),
            rest,
        })
    }
}

/// The path's values of `call`, as a cut keeps them.
fn path_of(call: &OperationCall) -> Value {
    canonical(&Value::Object(call.path_params.clone()))
}

/// `body` with its list's items `items`.
fn with_items(body: &Value, items: Vec<Value>) -> Option<Value> {
    let mut out = body.clone();
    *list_items_mut(&mut out)? = items;
    Some(out)
}

/// Writes a token for what `cut` left out of `generated`'s answer, when the answer has
/// a place for one, keeps the cut and seeds the items left out (see the module's doc).
/// Items of type `t`.
pub(super) async fn mint(
    state: &mut StateAccess<'_>,
    (plan, call): (&CallPlan<'_>, &OperationCall),
    t: Option<&str>,
    cut: lists::Cut,
    generated: &mut GeneratedResponse,
) {
    let rt = state.rt;
    let op = plan.op;
    let session = &plan.session;
    let Some(body) = generated.body.as_ref() else {
        return;
    };
    let Some(slot) = slot(rt.bundle(), (op, call), body, &cut.size_param) else {
        return;
    };
    // What a delete marker covers is no later page's.
    let mut rest = Vec::with_capacity(cut.rest.len().min(MAX_REST));
    // The records the store gave are stored already, as they stand.
    let generated_rest = cut.rest.len().saturating_sub(cut.stored);
    let mut seeded = Vec::new();
    let field = item_identity(rt.bundle(), op, body);
    let scope = deleted::scope(op, call);
    for (at, item) in cut.rest.into_iter().enumerate() {
        if rest.len() >= MAX_REST {
            break;
        }
        let gone = match (t, item.get(&field).and_then(id_string)) {
            (Some(t), Some(id)) if state.tracks_deleted => {
                deleted::is_deleted(state, session, t, scope.as_deref(), &id).await
            }
            _ => false,
        };
        if !gone {
            if at < generated_rest {
                seeded.push(item.clone());
            }
            rest.push(item);
        }
    }
    if rest.is_empty() {
        return;
    }
    let (nonce, place) = new_cut(session, op);
    let first = token(&nonce, place, 0);
    let valid = |candidate: &Value| {
        plan.engine
            .serve_stored(rt.bundle(), op, candidate, None)
            .is_some()
    };
    // A count of the list that counts less than the page and the rest counts them.
    let known = list_items(body).map_or(0, Vec::len) + rest.len();
    let counted = deleted::with_count_at_least(rt, op, body, known);
    let Some(page) = counted
        .iter()
        .chain(std::iter::once(body))
        .filter_map(|base| placed(base, &slot.path, Some(Value::String(first.clone()))))
        .find_map(|page| {
            // More to come, where the answer allows saying so.
            with_more(&page, &slot.path)
                .filter(|more| valid(more))
                .or_else(|| valid(&page).then_some(page))
        })
    else {
        return;
    };
    // The items left out are the session's, as a list's items are.
    if let Some(t) = t
        && !seeded.is_empty()
        && let Some(seeded) = with_items(&page, seeded)
    {
        let seeding = GeneratedResponse {
            body: Some(seeded),
            ..generated.clone()
        };
        write_state(
            state,
            WriteRequest {
                session,
                op,
                resource_type: t,
                target_id: None,
                generated: &seeding,
                request_body: None,
                collection: collection_of(op, call),
                origin: WriteOrigin::Seeded,
                held: false,
            },
        )
        .await;
    }
    let kept = Cut {
        nonce,
        op: op.id.clone(),
        path: path_of(call),
        query: bound_query(call, &slot.param, &cut.size_param),
        slot: slot.path,
        param: slot.param,
        size: cut.size,
        size_param: cut.size_param,
        page: page.clone(),
        rest,
    };
    let document = StoredResource {
        resource_type: INTERNAL_TYPE.to_string(),
        id: place_key(place),
        data: kept.to_value(),
    };
    // A cut not kept serves no token: the answer keeps the model's.
    if state.upsert(session, document).await {
        generated.body = Some(page);
    }
}

/// The next page a token `call` carries serves, with no model call (see the module's
/// doc); `None` when it carries none of a kept cut of this route, path and query.
pub(super) async fn serve(
    state: &mut StateAccess<'_>,
    plan: &CallPlan<'_>,
    call: &OperationCall,
) -> Option<GeneratedResponse> {
    let rt = state.rt;
    let op = plan.op;
    let session = &plan.session;
    if !matches!(op.kind, OperationKind::Query) || !deleted::applies(rt, plan) || !plain(plan) {
        return None;
    }
    let mut found = None;
    for (param, value) in &call.query {
        let Some((nonce, place, offset)) = value.as_str().map(str::trim).and_then(read_token)
        else {
            continue;
        };
        let Some(doc) = state.get(session, INTERNAL_TYPE, &place_key(place)).await else {
            continue;
        };
        if let Some(cut) = Cut::of(doc.data)
            && cut.nonce == nonce
            && cut.op == op.id
            && cut.param == *param
            && cut.path == path_of(call)
            && cut.query == bound_query(call, &cut.param, &cut.size_param)
            && offset <= cut.rest.len()
        {
            found = Some((cut, place, offset));
            break;
        }
    }
    let (cut, place, offset) = found?;
    let size = call
        .query
        .get(&cut.size_param)
        .and_then(|v| match v {
            Value::Number(n) => n.as_u64(),
            Value::String(s) => s.trim().parse().ok(),
            _ => None,
        })
        .and_then(|n| usize::try_from(n).ok())
        .filter(|n| *n > 0)
        .unwrap_or(cut.size);
    let next = offset.saturating_add(size).min(cut.rest.len());
    let items = cut.rest.get(offset..next)?.to_vec();
    let mut body = with_items(&cut.page, items)?;
    // What the session wrote since, and the request's filters, as on a first page.
    let t = op.resource_type.as_deref();
    lists::overlay_stored(state, (plan, call), t, &mut body).await;
    let _ = lists::narrow(rt, plan, call, &mut body);
    let valid = |candidate: &Value| plan.engine.serve_stored(rt.bundle(), op, candidate, None);
    let mut served = if next < cut.rest.len() {
        let more = placed(
            &body,
            &cut.slot,
            Some(Value::String(token(&cut.nonce, place, next))),
        )?;
        valid(&more)?
    } else {
        // The last page: no token, as the answer allows.
        let ends = [Some(Value::Null), None, Some(Value::String(String::new()))];
        let end = ends
            .into_iter()
            .filter_map(|end| placed(&body, &cut.slot, end))
            .find_map(|candidate| {
                // No more, as the first page said there was, where the answer allows.
                without_more(&candidate, &cut.page, &cut.slot)
                    .and_then(|done| valid(&done))
                    .or_else(|| valid(&candidate))
            });
        if end.is_none() {
            tracing::debug!(
                mock = %rt.name,
                operation = %op.id,
                "no end of a page token validates; the last page is generated"
            );
        }
        end?
    };
    if let Some(t) = t
        && state.tracks_deleted
    {
        let scope = deleted::scope(op, call);
        deleted::drop_from_list(
            state,
            session,
            (plan, call),
            t,
            scope.as_deref(),
            &mut served,
        )
        .await;
    }
    Some(served)
}
