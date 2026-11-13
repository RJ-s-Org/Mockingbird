//! Finding the record a path names: by key, by path fields and by alternate members.

use super::*;

/// The target (when `target_id` is set), the related resources before any
/// hop, and what [`follow_hops`] needs to add those.
pub(super) async fn read_context(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    op: &Operation,
    target_id: Option<&str>,
    call: &OperationCall,
) -> (Option<StoredResource>, Vec<StoredResource>, Followed) {
    let mut owner = guarded(state.rt, op, call);
    if let Some(owner) = &mut owner {
        owner.writing = !matches!(op.kind, OperationKind::Query);
    }
    let owner = owner.as_ref();
    let rt = state.rt;
    let target = match (op.resource_type.as_deref(), target_id) {
        (Some(t), Some(id)) => match records::held_by_key(state, session, t, id)
            .await
            .filter(|hit| {
                owner.is_none_or(|o| {
                    placed_fits(state, hit, o) || key_fits(rt, &state.noted(session, hit), o)
                })
            })
            // Where the path names another member than the key (`/issues/{number}`),
            // a record keyed by the path's value is another one unless that member
            // says so too.
            .filter(|hit| {
                op.identity.as_ref().is_none_or(|identity| {
                    let named = identity.path_member();
                    named == identity.field
                        || mb_mcm::member_at(&hit.data, named)
                            .and_then(id_string)
                            .is_none_or(|value| value == id)
                })
            }) {
            Some(hit) => Some(hit),
            None => {
                let delete = matches!(op.kind, OperationKind::Command(Effect::Delete));
                let identity = op.identity.as_ref();
                let param = target_param(op);
                let param_is_id = param.as_deref().is_some_and(id_like)
                    || identity.is_some_and(path_names_identity);
                // A delete may not declare what its route's read goes by.
                let read_fields: Vec<&str> = rt
                    .index()
                    .path_fields(t)
                    .iter()
                    .filter(|_| delete && rt.consistency && !param_is_id)
                    .map(String::as_str)
                    .collect();
                // A request member naming another member than the key (GraphQL's
                // `product(handle:)`) finds the record holding that value there.
                let by_member = identity
                    .filter(|i| i.path_param.is_none())
                    .and_then(|i| i.input_field.as_deref().filter(|f| *f != i.field));
                let then: Vec<&str> = match identity.and_then(|i| i.path_field.as_deref()) {
                    Some(field) => vec![field],
                    None if rt.consistency && by_member.is_some() => {
                        by_member.into_iter().collect()
                    }
                    None if rt.consistency
                        && param.is_some()
                        && (delete || identity.is_some())
                        && identity.is_none_or(|i| !path_names_identity(i)) =>
                    {
                        name_fields(param.as_deref().unwrap_or_default())
                    }
                    None => Vec::new(),
                };
                let found = match by_path_fields(state, session, t, &read_fields, id, owner).await {
                    Some(hit) => Some(hit),
                    None => by_path_fields(state, session, t, &then, id, owner).await,
                };
                match found {
                    Some(hit) => Some(hit),
                    None if rt.consistency => {
                        match by_alternate_member(state, session, (op, t), id, owner).await {
                            Some(hit) => Some(hit),
                            // The members the description adds (a changed identity
                            // member, described alternates): one more scan.
                            None => {
                                let tried: Vec<&str> =
                                    read_fields.iter().chain(&then).copied().collect();
                                match records::by_extra_fields(
                                    state,
                                    session,
                                    t,
                                    (&tried, id),
                                    owner,
                                )
                                .await
                                {
                                    Some(hit) => Some(hit),
                                    // What the mock this one shares stored.
                                    None => {
                                        shared::by_shared(state, session, (op, t), id, owner).await
                                    }
                                }
                            }
                        }
                    }
                    None => None,
                }
            }
        },
        _ => None,
    };
    let (related, followed) = related(state, session, op, target_id, target.as_ref(), call).await;
    (target, related, followed)
}

/// The value `data` holds for each of `fields`, as (field, value).
pub(super) fn path_values<'f>(fields: &'f [String], data: &Value) -> Vec<(&'f str, String)> {
    fields
        .iter()
        .filter_map(|f| Some((f.as_str(), mb_mcm::member_at(data, f).and_then(id_string)?)))
        .collect()
}

/// The members that may hold a name path parameter `param` gives: one named for
/// the parameter, which the contract ties to it, and [`NAME_FIELDS`]. Any other
/// string could hold anything: a `description`, a `default_branch`.
pub(super) fn name_fields(param: &str) -> Vec<&str> {
    let mut fields = NAME_FIELDS.to_vec();
    if !fields.contains(&param) {
        fields.push(param);
    }
    fields
}

/// Whether any owner's resource of type `t` goes by `value`: a route that cannot
/// tell whose it is cannot call it missing.
pub(super) async fn any_owner_holds(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    t: &str,
    value: &str,
    param: &str,
) -> bool {
    if records::held_by_key(state, session, t, value)
        .await
        .is_some()
    {
        return true;
    }
    let rt = state.rt;
    let mut fields = name_fields(param);
    fields.extend(rt.index().path_fields(t).iter().map(String::as_str));
    if by_path_fields(state, session, t, &fields, value, None)
        .await
        .is_some()
    {
        return true;
    }
    records::by_extra_fields(state, session, t, (&fields, value), None)
        .await
        .is_some()
}

/// The resource of type `t` whose field is `value`, trying `fields` in order
/// (a read by GitHub's issue number, or a repository's name) and, with
/// `owner`, only one that is theirs: the one [`PathAliases`] names when it still
/// holds the value, else the first of one scan.
pub(super) async fn by_path_fields(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    t: &str,
    fields: &[&str],
    value: &str,
    owner: Option<&Owner>,
) -> Option<StoredResource> {
    if fields.is_empty() {
        return None;
    }
    let rt = state.rt;
    // `placed`: found under the parent the call names ([`placed_fits`]).
    let holds = |noted: &Noted<'_>, field: &str, placed: bool| {
        mb_mcm::member_at(&noted.r.data, field)
            .and_then(id_string)
            .as_deref()
            == Some(value)
            && owner.is_none_or(|o| placed || owner_fits(rt, noted, o, field))
    };
    for field in fields {
        if let Some(id) = state.rt.path_aliases().id_of(session, t, field, value)
            && let Some(hit) = state.get(session, t, &id).await
            && holds(
                &state.noted(session, &hit),
                field,
                owner.is_some_and(|o| placed_fits(state, &hit, o)),
            )
        {
            return Some(hit);
        }
    }
    let stored = state.list(session, t, MAX_SCAN).await;
    let placed = |r: &StoredResource| owner.is_some_and(|o| placed_fits(state, r, o));
    let found = fields.iter().find_map(|field| {
        let found = stored
            .iter()
            .find(|r| holds(&state.noted(session, r), field, placed(r)))?;
        Some((*field, found))
    });
    let Some((field, found)) = found else {
        // No member holds the name alone, but a path may hold it with its owner.
        let owner = owner?;
        return stored
            .iter()
            .find(|r| {
                r.data.as_object().is_some_and(|m| {
                    m.values()
                        .filter_map(Value::as_str)
                        .any(|t| path_shows(t, owner))
                }) && (placed(r) || owner_fits(rt, &state.noted(session, r), owner, ""))
            })
            .cloned();
    };
    let values = vec![(field, value.to_string())];
    state
        .rt
        .path_aliases()
        .remember(session, t, &found.id, values);
    Some(found.clone())
}

/// The members of type `t`'s records that `op`'s target parameter names its record by
/// besides the identity, when the parameter offers either of two of them: in its name
/// (`templateIdOrAlias` names `TemplateId` and `Alias`) or in an either-or phrase of
/// its description ("the project's id or slug", "its id or the alias"); see
/// [`either_or`]. An alternative that is a bare `id` while the identity is no id by its
/// name (an `Alias` key) is the one other member that is an id by its name. A member
/// the description only mentions, or that a negation keeps out ("not its number"), is
/// none, and so is a phrase neither of whose sides is a key of the record (the
/// identity, the member the path parameter names, or one the type's operations
/// identify it by): "the ID of the user or organization membership" offers two
/// other members, not two keys. A parameter whose schema is not a string, or that
/// offers fewer than two members, the identity counted, has none.
pub(super) fn alternate_members(rt: &MockRuntime, op: &Operation, t: &str) -> Vec<String> {
    let Some(param) = target_param(op) else {
        return Vec::new();
    };
    let declared = op
        .input
        .path
        .as_ref()
        .and_then(|p| p.get("properties"))
        .and_then(|p| p.get(&param));
    let is_string = declared
        .and_then(|d| d.get("type"))
        .and_then(Value::as_str)
        .is_none_or(|t| t == "string");
    if !is_string {
        return Vec::new();
    }
    let description = declared
        .and_then(|d| d.get("description"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let identity = op.identity.as_ref().map_or("id", |i| i.field.as_str());
    let index = rt.index();
    let bundle = rt.bundle();
    let read = index
        .detail_reads
        .get(t)
        .and_then(|i| bundle.operations.get(*i)?.success_output()?.schema.as_ref());
    let schema = index.record_schemas.get(t).or(read);
    let scalar = |member: &str| {
        schema
            .and_then(|s| property_schema(bundle, s, member))
            .and_then(|m| m.get("type").and_then(Value::as_str).map(str::to_string))
            .is_some_and(|t| matches!(t.as_str(), "string" | "integer"))
    };
    let mut members: Vec<String> = type_members(rt, t)
        .into_iter()
        .filter(|m| scalar(m))
        .collect();
    if !members.iter().any(|m| m == identity) {
        members.push(identity.to_string());
    }
    let is_id = |member: &str| mb_mcm::ends_with_words(member, "id");
    let declared_keys = index.identity_fields(t);
    // The one member other than the identity that is an id of the record itself, for
    // a bare `id` alternative of a key that is none: an id naming the record's own kind
    // (`recipeId` on recipes), or one the type's operations identify it by. An id of
    // another kind (`ownerId`) is a foreign key, never the record's.
    let other_id = || {
        let mut ids = members.iter().filter(|m| {
            m.as_str() != identity
                && is_id(m)
                && (names_own_kind(m, t) || declared_keys.iter().any(|k| k == *m))
        });
        match (ids.next(), ids.next()) {
            (Some(only), None) if !is_id(mb_mcm::leaf_member(identity)) => Some(only.clone()),
            _ => None,
        }
    };
    // A bare `id` stands for the identity when the identity is an id by its name.
    let bare_id = |runs: &[String]| {
        (is_id(mb_mcm::leaf_member(identity)) && runs.iter().any(|r| r == "id"))
            .then(|| identity.to_string())
    };
    let resolve = |runs: &[String]| {
        runs.iter()
            .find_map(|run| {
                members
                    .iter()
                    .find(|m| mb_mcm::joined_name(m) == *run)
                    .cloned()
            })
            .or_else(|| runs.iter().any(|r| r == "id").then(other_id).flatten())
            .or_else(|| bare_id(runs))
    };
    // The members that name the record itself: the identity, the response field the
    // path parameter names when it is another, and those the type's other operations
    // identify it by (a delete whose answer declares no key reads as its read does).
    let keys: Vec<&str> = std::iter::once(identity)
        .chain(op.identity.as_ref().and_then(|i| i.path_field.as_deref()))
        .chain(declared_keys.iter().map(String::as_str))
        .collect();
    let mut offered: Vec<String> = Vec::new();
    for pair in either_or(&[param.as_str()]).into_iter().chain(either_or(
        &description
            .split(['.', ',', ';', ':', '!', '?', '(', ')'])
            .collect::<Vec<_>>(),
    )) {
        let [Some(a), Some(b)] = pair.map(|runs| resolve(&runs)) else {
            continue;
        };
        // An offer of two ways to name the record has the record's key on one side:
        // "the user or organization" names two other records, not this one's keys.
        if a == b || !keys.iter().any(|k| *k == a || *k == b) {
            continue;
        }
        for m in [a, b] {
            if !offered.contains(&m) {
                offered.push(m);
            }
        }
    }
    if offered.len() < 2 {
        return Vec::new();
    }
    offered.retain(|m| m != identity);
    offered
}

/// Whether the id `member` names type `t`'s own kind: its words before `id` are the
/// type's name or its last words, plural aside (`recipeId` for `recipes`, `lineId`
/// for `ticket_lines`).
pub(super) fn names_own_kind(member: &str, t: &str) -> bool {
    let words = mb_mcm::words(mb_mcm::leaf_member(member));
    let Some((_, before)) = words.split_last() else {
        return false;
    };
    if before.is_empty() {
        return false;
    }
    let named = mb_mcm::stem(&mb_mcm::joined_name(&before.concat()));
    let kind = mb_mcm::words(t);
    (0..kind.len()).any(|from| mb_mcm::stem(&mb_mcm::joined_name(&kind[from..].concat())) == named)
}

/// Words before an `or` that make the phrase no offer of two ways to name a record.
pub(super) const NEGATIONS: [&str; 7] =
    ["not", "no", "never", "neither", "nor", "without", "except"];

/// Words skipped after an `or` ("or its alias", "or the slug", "or by name").
pub(super) const LEADING: [&str; 7] = ["the", "its", "their", "a", "an", "by", "his"];

/// The either-or phrases in `clauses` (a name, or a description cut at its
/// punctuation): for each `or` with no negation before it in its clause, the words
/// nearest it on either side, as the runs that may name a member, longest first (the
/// last two words before it joined, then the last; the first two after it, then the
/// first). `templateIdOrAlias` is `[["templateid", "id"], ["alias"]]`; "the order's
/// id, not its order number" has none.
pub(super) fn either_or(clauses: &[&str]) -> Vec<[Vec<String>; 2]> {
    let mut found = Vec::new();
    for clause in clauses {
        let words: Vec<String> = mb_mcm::words(clause)
            .into_iter()
            .map(str::to_lowercase)
            // A possessive's `s` ("the template's id").
            .filter(|w| w != "s")
            .collect();
        for (at, word) in words.iter().enumerate() {
            if word != "or" {
                continue;
            }
            let before = &words[..at];
            if before.iter().any(|w| NEGATIONS.contains(&w.as_str())) {
                continue;
            }
            let after: Vec<&String> = words[at + 1..]
                .iter()
                .skip_while(|w| LEADING.contains(&w.as_str()))
                .collect();
            let runs = |side: &[&String]| -> Vec<String> {
                let mut runs = Vec::new();
                if side.len() >= 2 {
                    runs.push(format!("{}{}", side[0], side[1]));
                }
                if let Some(first) = side.first() {
                    runs.push((*first).clone());
                }
                runs
            };
            let mut back: Vec<&String> = before.iter().rev().take(2).collect();
            back.reverse();
            let back_runs = match back.as_slice() {
                [one, two] => vec![format!("{one}{two}"), (*two).clone()],
                [one] => vec![(*one).clone()],
                _ => Vec::new(),
            };
            let ahead: Vec<&String> = after.into_iter().take(2).collect();
            let ahead_runs = runs(&ahead);
            if !back_runs.is_empty() && !ahead_runs.is_empty() {
                found.push([back_runs, ahead_runs]);
            }
        }
    }
    found
}

/// The one held record of type `t` that holds the path value `value`, which missed its
/// key, in one of the members `op`'s parameter also names it by (see
/// [`alternate_members`]): a template read by its id where its alias is its key. With
/// `owner`, only one that is theirs; with two such records, none.
pub(super) async fn by_alternate_member(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    (op, t): (&Operation, &str),
    value: &str,
    owner: Option<&Owner>,
) -> Option<StoredResource> {
    let rt = state.rt;
    let members = alternate_members(rt, op, t);
    if members.is_empty() {
        return None;
    }
    let stored = state.list(session, t, MAX_SCAN).await;
    let mut holding = stored.into_iter().filter(|r| {
        members.iter().any(|field| {
            mb_mcm::member_at(&r.data, field)
                .and_then(id_string)
                .is_some_and(|held| held == value)
                && owner.is_none_or(|o| {
                    placed_fits(state, r, o) || owner_fits(rt, &state.noted(session, r), o, field)
                })
        })
    });
    match (holding.next(), holding.next()) {
        (Some(found), None) => Some(found),
        _ => None,
    }
}
