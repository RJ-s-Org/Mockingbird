//! The rest of a list answer cut to its page size, served by a token.
//!
//! [`lists::narrow`] cuts a generated list to the page size a request asks for. A
//! client then asks for the next page by the token the answer carried, which the model
//! made up: the request repeats the first's, or names a token no list knows, and the
//! answer is the first page again (from the cache) or a list made up anew. So when a cut
//! leaves items out, and the contract has a place for a token, the engine mints one.
//!
//! The place is one text member of the answer (no `const` or `enum`), at its top level
//! or inside an object member beside the list, whose name is, or ends with the words
//! of, a text query parameter of the route other than the page size: Zoom's
//! `next_page_token` for `next_page_token`, Square's `cursor` for `cursor`, Pipedrive's
//! `additional_data.next_cursor` for `cursor`. Exactly one, or none is used; nor is a
//! token written that leaves the answer invalid. With none, the answer is as
//! [`lists::narrow`] left it.
//!
//! The token is opaque: 40 hex digits, a hash of the session, the route and a counter.
//! What it serves is kept under the store's reserved type ([`mb_state::INTERNAL_TYPE`]),
//! like the deleted markers: the route, the path's values, the parameter, the page
//! size, the page as answered and the items left out, the newest [`MAX_TOKENS`] per
//! session. The items left out are seeded as a list's items are, so a read of one finds
//! it. A request on the same route and path carrying a known token under that
//! parameter is served from it without a model call: the next items, as many as the
//! request's page size or the first's, each showing its stored record (see
//! [`lists::overlay_stored`]), and a new token when more remain; the last page's token
//! is `null`, left out or empty, whichever the answer validates with. Its query differs
//! from the first page's by the token, so the response cache never answers it with the
//! first page; it is not cached itself, as the token already serves it.
//!
//! An LLM and `consistency: on` only, as [`deleted::applies`] says, and plain calls
//! (no scenario or forced status).

use std::sync::atomic::{AtomicU64, Ordering};

use mb_engine::consistency::{list_items_mut, list_member};
use mb_state::INTERNAL_TYPE;
use sha2::{Digest, Sha256};

use super::*;

/// Tokens kept per session, the newest.
const MAX_TOKENS: usize = 256;
/// Items left out that a token keeps, at most.
const MAX_REST: usize = 500;
/// Hex digits of a token.
const TOKEN_LEN: usize = 40;
/// The prefix of a token's document, and the document listing them, oldest first.
const PREFIX: &str = "#page/";
const INDEX: &str = "#pages";

/// Tokens minted by this process, which makes each unlike the last.
static MINTED: AtomicU64 = AtomicU64::new(0);

/// Where an answer of `op` carries its token: the member's path from the top, and the
/// query parameter the token comes back under.
struct Slot {
    path: Vec<String>,
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

/// The one place an answer `body` of `op` has for a token (see the module's doc).
fn slot(bundle: &mb_mcm::Bundle, op: &Operation, body: &Value, size_param: &str) -> Option<Slot> {
    let list = list_member(body)?;
    let schema = op.success_output()?.schema.as_ref()?;
    let declared = op.input.query.as_ref()?.get("properties")?.as_object()?;
    let params: Vec<&str> = declared
        .iter()
        .filter(|(name, schema)| {
            name.as_str() != size_param
                && !mb_mcm::is_generic_id(name)
                && is_free_text(bundle, schema)
        })
        .map(|(name, _)| name.as_str())
        .collect();
    let param_of = |name: &str| -> Option<String> {
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
                if let Some(param) = param_of(name) {
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
                if is_free_text(bundle, inner_member)
                    && let Some(param) = param_of(inner_name)
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

/// A token not minted before.
fn new_token(session: &SessionKey, op: &Operation) -> String {
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
    hex[..TOKEN_LEN].to_string()
}

/// Whether `text` is a token's shape: [`TOKEN_LEN`] lower-case hex digits.
fn is_token(text: &str) -> bool {
    text.len() == TOKEN_LEN && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// What a token serves.
struct Page {
    op: String,
    path: Value,
    slot: Vec<String>,
    param: String,
    size: usize,
    size_param: String,
    /// The page as answered, with its token.
    page: Value,
    rest: Vec<Value>,
}

impl Page {
    fn to_value(&self) -> Value {
        serde_json::json!({
            "op": self.op,
            "path": self.path,
            "slot": self.slot,
            "param": self.param,
            "size": self.size,
            "size_param": self.size_param,
            "page": self.page,
            "rest": self.rest,
        })
    }

    fn of(value: &Value) -> Option<Self> {
        Some(Self {
            op: value.get("op")?.as_str()?.to_string(),
            path: value.get("path")?.clone(),
            slot: value
                .get("slot")?
                .as_array()?
                .iter()
                .map(|s| s.as_str().map(str::to_string))
                .collect::<Option<_>>()?,
            param: value.get("param")?.as_str()?.to_string(),
            size: usize::try_from(value.get("size")?.as_u64()?).ok()?,
            size_param: value.get("size_param")?.as_str()?.to_string(),
            page: value.get("page")?.clone(),
            rest: value.get("rest")?.as_array()?.clone(),
        })
    }
}

/// The path's values of `call`, as a token keeps them.
fn path_of(call: &OperationCall) -> Value {
    canonical(&Value::Object(call.path_params.clone()))
}

/// Keeps `page` under a new token and lists it, ending the oldest past the bound.
async fn keep(state: &mut StateAccess<'_>, session: &SessionKey, token: &str, page: &Page) {
    let key = format!("{PREFIX}{token}");
    let document = StoredResource {
        resource_type: INTERNAL_TYPE.to_string(),
        id: key.clone(),
        data: page.to_value(),
    };
    if !state.upsert(session, document).await {
        return;
    }
    let mut listed: Vec<String> = match state.get(session, INTERNAL_TYPE, INDEX).await {
        Some(doc) => serde_json::from_value(doc.data).unwrap_or_default(),
        None => Vec::new(),
    };
    listed.retain(|held| held != &key);
    listed.push(key);
    let excess = listed.len().saturating_sub(MAX_TOKENS);
    for oldest in listed.drain(..excess) {
        state.delete(session, INTERNAL_TYPE, &oldest).await;
    }
    let index = StoredResource {
        resource_type: INTERNAL_TYPE.to_string(),
        id: INDEX.to_string(),
        data: Value::from(listed),
    };
    state.upsert(session, index).await;
}

/// `body` with its list's items `items`.
fn with_items(body: &Value, items: Vec<Value>) -> Option<Value> {
    let mut out = body.clone();
    *list_items_mut(&mut out)? = items;
    Some(out)
}

/// Writes a token for what `cut` left out of `generated`'s answer, when the answer has
/// a place for one, keeps what it serves and seeds the items left out (see the
/// module's doc). Items of type `t`.
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
    let Some(slot) = slot(rt.bundle(), op, body, &cut.size_param) else {
        return;
    };
    let token = new_token(session, op);
    let Some(page) = placed(body, &slot.path, Some(Value::String(token.clone()))) else {
        return;
    };
    let valid = |candidate: &Value| {
        plan.engine
            .serve_stored(rt.bundle(), op, candidate, None)
            .is_some()
    };
    if !valid(&page) {
        return;
    }
    let mut rest = cut.rest;
    rest.truncate(MAX_REST);
    // The items left out are the session's, as a list's items are.
    if let Some(t) = t
        && let Some(seeded) = with_items(&page, rest.clone())
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
    let kept = Page {
        op: op.id.clone(),
        path: path_of(call),
        slot: slot.path,
        param: slot.param,
        size: cut.size,
        size_param: cut.size_param,
        page: page.clone(),
        rest,
    };
    keep(state, session, &token, &kept).await;
    generated.body = Some(page);
}

/// The next page a token `call` carries serves, with no model call (see the module's
/// doc); `None` when it carries none this route minted.
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
        let Some(token) = value.as_str().map(str::trim).filter(|t| is_token(t)) else {
            continue;
        };
        let Some(doc) = state
            .get(session, INTERNAL_TYPE, &format!("{PREFIX}{token}"))
            .await
        else {
            continue;
        };
        if let Some(page) = Page::of(&doc.data)
            && page.op == op.id
            && page.param == *param
            && page.path == path_of(call)
        {
            found = Some(page);
            break;
        }
    }
    let mut page = found?;
    let size = call
        .query
        .get(&page.size_param)
        .and_then(|v| match v {
            Value::Number(n) => n.as_u64(),
            Value::String(s) => s.trim().parse().ok(),
            _ => None,
        })
        .and_then(|n| usize::try_from(n).ok())
        .filter(|n| *n > 0)
        .unwrap_or(page.size);
    let remaining = page.rest.split_off(size.min(page.rest.len()));
    let mut body = with_items(&page.page, std::mem::take(&mut page.rest))?;
    lists::overlay_stored(state, (plan, call), op.resource_type.as_deref(), &mut body).await;
    let valid = |candidate: &Value| plan.engine.serve_stored(rt.bundle(), op, candidate, None);
    if remaining.is_empty() {
        // The last page: no token, as the answer allows.
        let ends = [Some(Value::Null), None, Some(Value::String(String::new()))];
        return ends
            .into_iter()
            .filter_map(|end| placed(&body, &page.slot, end))
            .find_map(|candidate| valid(&candidate));
    }
    let token = new_token(session, op);
    let next = placed(&body, &page.slot, Some(Value::String(token.clone())))?;
    let served = valid(&next)?;
    let kept = Page {
        page: next,
        rest: remaining,
        size,
        ..page
    };
    keep(state, session, &token, &kept).await;
    Some(served)
}
