//! Owners and collections a call's path names, and whether a record belongs to them.

use super::*;

/// Fields a path parameter that names no field may mean: `/users/{username}`
/// finds a user seeded under its numeric id by its `login`. A last resort, when
/// no member is named for the parameter (see [`name_fields`]).
pub(super) const NAME_FIELDS: [&str; 4] = ["name", "login", "slug", "key"];

/// Whose a path parameter's value is, as the path says: `/orgs/{org}/teams/`
/// `{slug}` has `{org}` before `{slug}`, and the path up to it.
pub(super) struct Owner {
    /// The path of the collection the parameter names a member of.
    pub(super) collection: Collection,
    /// The name of the parameter nearest before it, and that parameter's value.
    pub(super) param: String,
    pub(super) value: String,
    /// The value of the parameter itself.
    pub(super) name: String,
    /// The path from `value` to `name`: `octo/teams/core`.
    pub(super) tail: String,
    /// For a write, which acts unless the resource is known to be another's.
    pub(super) writing: bool,
}

/// A collection a path names: `/orgs/octo/teams` of `/orgs/{}/teams`.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Collection {
    pub(super) template: String,
    pub(super) path: String,
}

/// `path` by segment, a trailing slash aside.
pub(super) fn path_segments(path: &str) -> Vec<&str> {
    path.trim_end_matches('/').split('/').collect()
}

/// The parameter a path segment wholly is.
pub(super) fn segment_param(segment: &str) -> Option<&str> {
    segment.strip_prefix('{')?.strip_suffix('}')
}

/// `segments` as a [`Collection`], when `call` gives each parameter a value.
pub(super) fn fill_path(call: &OperationCall, segments: &[&str]) -> Option<Collection> {
    let mut filled = Vec::with_capacity(segments.len());
    let mut template = Vec::with_capacity(segments.len());
    for segment in segments {
        match segment_param(segment) {
            Some(param) => {
                filled.push(call.path_params.get(param).and_then(id_string)?);
                template.push("{}");
            }
            None => {
                filled.push((*segment).to_string());
                template.push(segment);
            }
        }
    }
    Some(Collection {
        template: template.join("/"),
        path: filled.join("/"),
    })
}

/// The [`Owner`] of path parameter `name`, when a parameter comes before it.
pub(super) fn owner_of_param(op: &Operation, call: &OperationCall, name: &str) -> Option<Owner> {
    let Binding::Http { path, .. } = &op.binding else {
        return None;
    };
    let param = format!("{{{name}}}");
    let segments = path_segments(path);
    let at = segments.iter().position(|s| *s == param)?;
    let before = segments[..at]
        .iter()
        .rposition(|s| segment_param(s).is_some())?;
    let owner = segment_param(segments[before])?;
    let value = |p: &str| call.path_params.get(p).and_then(id_string);
    // A bare `{id}` is named by the collection before it: a project's.
    let owner_name = match before.checked_sub(1).map(|i| segments[i]) {
        Some(collection) if mb_mcm::is_generic_id(owner) && segment_param(collection).is_none() => {
            collection.strip_suffix('s').unwrap_or(collection)
        }
        _ => owner,
    };
    Some(Owner {
        collection: fill_path(call, &segments[..at])?,
        param: owner_name.to_string(),
        value: value(owner)?,
        name: value(name)?,
        tail: fill_path(call, &segments[before..=at])?.path,
        writing: false,
    })
}

/// The path parameter naming what `op` acts on (see [`target_id`]).
pub(super) fn target_param(op: &Operation) -> Option<String> {
    let identity = op.identity.as_ref();
    if let Some(param) = identity.and_then(|i| i.path_param.clone()) {
        return Some(param);
    }
    let Binding::Http { path, .. } = &op.binding else {
        return None;
    };
    matches!(
        op.kind,
        OperationKind::Command(Effect::Update | Effect::Delete)
    )
    .then(|| last_path_param(path))
    .flatten()
}

/// The [`Owner`] of what `op` targets, unless its parameter says it is an id:
/// an id stands on its own, whatever the route it is read by.
pub(super) fn owner_of(op: &Operation, call: &OperationCall) -> Option<Owner> {
    let param = target_param(op)?;
    if id_like(&param) {
        return None;
    }
    owner_of_param(op, call, &param)
}

/// [`owner_of`], when consistency is on.
pub(super) fn guarded(rt: &MockRuntime, op: &Operation, call: &OperationCall) -> Option<Owner> {
    owner_of(op, call).filter(|_| rt.consistency)
}

/// The collection a write or a list answer puts its resources in, when a path
/// parameter makes it one of many: `/orgs/octo/teams` for a list there, a create
/// there and an update of `/orgs/octo/teams/{slug}` alike. A target parameter in
/// no segment of its own (`{slug}.json`) gives none.
pub(super) fn collection_of(op: &Operation, call: &OperationCall) -> Option<Collection> {
    fill_path(call, &collection_segments(op)?)
}

/// The segments of `op`'s path naming the collection it writes or lists in, when a
/// path parameter makes it one of many (see [`collection_of`]).
pub(super) fn collection_segments(op: &Operation) -> Option<Vec<&str>> {
    let Binding::Http { path, .. } = &op.binding else {
        return None;
    };
    let mut segments = path_segments(path);
    let end = match target_param(op) {
        Some(target) => {
            let param = format!("{{{target}}}");
            segments.iter().position(|s| *s == param)?
        }
        None => segments.len(),
    };
    segments.truncate(end);
    segments
        .iter()
        .any(|s| segment_param(s).is_some())
        .then_some(segments)
}

/// Whether the contract creates records of type `t` in a collection under a parent
/// path (`POST /groups/{group}/contacts`): where such a record is then matters.
pub(super) fn created_in_collections(bundle: &mb_mcm::Bundle, t: &str) -> bool {
    bundle.operations.iter().any(|op| {
        op.kind == OperationKind::Command(Effect::Create)
            && op.resource_type.as_deref() == Some(t)
            && collection_segments(op).is_some()
    })
}

/// Whether `hit`, a stored record of the type `call`, a create of `op`, makes, is in the
/// collection the create runs in, as far as this runtime's writes tell: placed there, or,
/// placed nowhere known, when the create is top-level and the contract never creates the
/// type under a parent path (see [`created_in_collections`]). A record of a parent path
/// counts only under that parent: a label `bug` of one repository is no other's.
pub(super) fn in_create_collection(
    rt: &MockRuntime,
    session: &SessionKey,
    op: &Operation,
    call: &OperationCall,
    hit: &StoredResource,
) -> bool {
    let collection = collection_of(op, call);
    match rt.collections().placement(session, hit) {
        Some(at) => at == collection,
        None => collection.is_none() && !created_in_collections(rt.bundle(), &hit.resource_type),
    }
}

/// Whether `text` is a path ending in `owner`'s value and name: `octo/hello`
/// (a `full_name`, a `path_with_namespace`) or `…/octo/teams/core` (a `url`).
pub(super) fn path_shows(text: &str, owner: &Owner) -> bool {
    let text = text.trim_end_matches('/');
    let short = format!("{}/{}", owner.value, owner.name);
    [short.as_str(), owner.tail.as_str()].iter().any(|path| {
        text.strip_suffix(path)
            .is_some_and(|rest| rest.is_empty() || rest.ends_with('/'))
    })
}

/// Whether `text` is a path relative to nothing, ending in `name`: `octo/hello`.
pub(super) fn relative_path_ending(text: &str, name: &str) -> bool {
    !text.contains("://")
        && !text.starts_with('/')
        && text
            .trim_end_matches('/')
            .rsplit_once('/')
            .is_some_and(|(_, last)| last == name)
}

/// How a member relates to parameter `param`: named for it, or for its id.
/// `org` also means `organization`, but `org_ref` and `owner_previous` are
/// other things. A compound parameter (`organization_id_or_slug`) is tied to
/// what any of its alternatives is (see [`Compound`]).
pub(super) fn tied_to_param(member: &str, param: &str) -> bool {
    match Compound::of(param) {
        Some(compound) => compound.ties(member, &[]),
        None => tied_to_one_param(member, param),
    }
}

/// A path parameter whose name joins alternatives with `_or_`
/// (`organization_id_or_slug`), naming one thing by any of them: the thing
/// itself, by its stem (`organization`, the first alternative before its last
/// `_`), and its keys, the first alternative (`organization_id`) and each later
/// one as a member of the thing (`slug` as `organization_slug`). A compound
/// whose first alternative has no stem (`id_or_name`) names nothing another
/// record could be named for, and is read as a plain name. `_or_` is the one
/// separator read.
#[derive(Debug, PartialEq)]
pub(super) struct Compound {
    pub(super) stem: String,
    pub(super) keys: Vec<String>,
}

impl Compound {
    pub(super) fn of(param: &str) -> Option<Self> {
        let lower = param.to_lowercase();
        let mut parts = lower.split("_or_").filter(|p| !p.is_empty());
        let (first, second) = (parts.next()?, parts.next()?);
        let (stem, _) = first
            .rsplit_once('_')
            .filter(|(stem, _)| !stem.is_empty())?;
        let mut keys = vec![first.to_string()];
        for later in std::iter::once(second).chain(parts) {
            if later.starts_with(&format!("{stem}_")) {
                keys.push(later.to_string());
            } else {
                keys.push(format!("{stem}_{later}"));
            }
        }
        Some(Self {
            stem: stem.to_string(),
            keys,
        })
    }

    /// Whether `member` is named for the thing: its stem exactly, or the stem
    /// then a word boundary (`organization_ref`, `organizationSlug`; not
    /// `organizations`, nor `username` for `user`), or one of its keys as
    /// [`tied_to_one_param`] has it. A key the record's own type declares,
    /// among `own`, is the record's own member and ties nothing.
    pub(super) fn ties(&self, member: &str, own: &[&str]) -> bool {
        let lower = member.to_lowercase();
        let at_boundary = member.len() > self.stem.len()
            && lower.starts_with(&self.stem)
            && member
                .get(self.stem.len()..)
                .and_then(|rest| rest.chars().next())
                .is_some_and(|c| c == '_' || c.is_ascii_uppercase());
        let keyed = self.keys.iter().any(|key| tied_to_one_param(member, key));
        let declared = own.iter().any(|o| o.eq_ignore_ascii_case(member));
        if keyed && declared {
            return false;
        }
        lower == self.stem || at_boundary || keyed
    }
}

/// [`tied_to_param`] for one parameter name.
pub(super) fn tied_to_one_param(member: &str, param: &str) -> bool {
    let (member, param) = (member.to_lowercase(), param.to_lowercase());
    member == param
        || member == format!("{param}_id")
        || member == format!("{param}id")
        || (param.len() >= 3
            && member.starts_with(&param)
            && member.chars().all(char::is_alphanumeric))
}

/// What `r` itself says of `owner`, `None` when it says nothing: a relative path
/// ending in the name decides, all of them having to show the owner (a fork
/// names its source, and its `full_name` says whose it is); else a member tied
/// to the owner's parameter, as a value or an object holding it, not one the
/// contract types as `r`'s own type; else a link showing the owner.
pub(super) fn owner_evidence(rt: &MockRuntime, r: &StoredResource, owner: &Owner) -> Option<bool> {
    let members = r.data.as_object()?;
    let strings = || members.values().filter_map(Value::as_str);
    let mut paths = strings()
        .filter(|text| relative_path_ending(text, &owner.name))
        .peekable();
    if paths.peek().is_some() {
        return Some(paths.all(|text| path_shows(text, owner)));
    }
    let own_type = |member: &str| typed_as_own(rt, &r.resource_type, member);
    let holds = |value: &Value| match value {
        Value::Object(inner) => inner
            .values()
            .any(|v| v.as_str() == Some(owner.value.as_str())),
        value => id_string(value).as_deref() == Some(owner.value.as_str()),
    };
    let is_link = |value: &Value| value.as_str().is_some_and(|text| text.contains("://"));
    let numeric_owner = owner.value.parse::<f64>().is_ok();
    let compound = Compound::of(&owner.param);
    let declared = match compound {
        Some(_) => declared_members(rt, &r.resource_type),
        None => Vec::new(),
    };
    let ties = |member: &str| match &compound {
        Some(compound) => compound.ties(member, &declared),
        None => tied_to_param(member, &owner.param),
    };
    let mut tied = members
        .iter()
        .filter(|(member, value)| {
            !mb_mcm::is_generic_id(member) && ties(member) && !is_link(value) && !own_type(member)
        })
        // A number says nothing of an owner the route names by a string.
        .filter(|(_, value)| {
            value.is_string() || value.is_object() || (value.is_number() && numeric_owner)
        })
        // Nor does a reference by id alone (GraphQL's `repository: {id}`) to an
        // owner the route names by something else: unless it holds the value.
        .filter(|(_, value)| !(by_id_alone(value) && !holds(value)))
        .peekable();
    if tied.peek().is_some() {
        let held = tied.any(|(_, value)| holds(value));
        // A compound parameter names its owner by one key of several: a member
        // holding another (the id, under a route given the slug) says nothing.
        if held || compound.is_none() {
            return Some(held);
        }
    }
    strings()
        .any(|text| text.contains("://") && path_shows(text, owner))
        .then_some(true)
}

/// Whether `value` is an object holding nothing but ids and its type's name
/// (`{__typename, id}`, or nothing at all once another mock's id was left out):
/// a reference by id alone.
fn by_id_alone(value: &Value) -> bool {
    value.as_object().is_some_and(|members| {
        members
            .keys()
            .all(|k| k == "__typename" || mb_mcm::is_generic_id(k))
    })
}

/// The members the record schema of `resource_type` declares, as [`typed_as_own`]
/// finds that schema.
pub(super) fn declared_members<'a>(rt: &'a MockRuntime, resource_type: &str) -> Vec<&'a str> {
    let index = rt.index();
    let schema = index.record_schemas.get(resource_type).or_else(|| {
        let i = index.detail_reads.get(resource_type)?;
        rt.bundle()
            .operations
            .get(*i)?
            .success_output()?
            .schema
            .as_ref()
    });
    schema_members(rt, schema)
}

/// The members `schema` declares, its `$ref`s followed and its branches flattened.
pub(super) fn schema_members<'a>(rt: &'a MockRuntime, schema: Option<&'a Value>) -> Vec<&'a str> {
    let mut names = Vec::new();
    for name in schema_parts(rt.bundle(), schema)
        .0
        .iter()
        .filter_map(|p| p.get("properties")?.as_object())
        .flat_map(|p| p.keys())
    {
        if !names.contains(&name.as_str()) {
            names.push(name.as_str());
        }
    }
    names
}

/// Whether the contract types `member` of a `resource_type` record as that same type
/// (a task's `parent`, a user's `manager`), by the type's record schema: such a member
/// names another of its kind, not an owner.
pub(super) fn typed_as_own(rt: &MockRuntime, resource_type: &str, member: &str) -> bool {
    let index = rt.index();
    let schema = index.record_schemas.get(resource_type).or_else(|| {
        let i = index.detail_reads.get(resource_type)?;
        rt.bundle()
            .operations
            .get(*i)?
            .success_output()?
            .schema
            .as_ref()
    });
    let (parts, _) = schema_parts(rt.bundle(), schema);
    let inner = parts.iter().find_map(|p| p.get("properties")?.get(member));
    let (_, defs) = schema_parts(rt.bundle(), inner);
    defs.iter()
        .any(|d| index.types_by_def.get(*d).map(String::as_str) == Some(resource_type))
}

/// Whether `r` shows it is `owner`'s (see [`owner_evidence`]).
pub(super) fn shows_owner(rt: &MockRuntime, r: &StoredResource, owner: &Owner) -> bool {
    owner_evidence(rt, r, owner) == Some(true)
}

/// Whether a string of `r` ending in the name does not show `owner`: another
/// owner's link, or none at all. Weak: a `homepage` may end so.
pub(super) fn links_elsewhere(r: &StoredResource, owner: &Owner) -> bool {
    r.data.as_object().is_some_and(|members| {
        members.values().filter_map(Value::as_str).any(|text| {
            text.trim_end_matches('/')
                .rsplit_once('/')
                .is_some_and(|(_, last)| last == owner.name)
                && !path_shows(text, owner)
        })
    })
}

/// Whether `r` is known to be another's: by a record, by what it says, or, when
/// it says nothing, by a link elsewhere.
pub(super) fn known_foreign(
    rt: &MockRuntime,
    session: &SessionKey,
    r: &StoredResource,
    owner: &Owner,
) -> bool {
    rt.collections().fits(session, r, owner).map_or_else(
        || match owner_evidence(rt, r, owner) {
            Some(shown) => !shown,
            None => links_elsewhere(r, owner),
        },
        |fits| !fits,
    )
}

/// Whether stored `r` may be taken for `owner`'s under the value in `field`. A
/// write acts on what it cannot tell is another's, or it would leave it behind.
/// A read needs a record or what `r` shows, as an answer can be generated. A
/// number cannot show its owner, so it passes unless `r` links elsewhere.
pub(super) fn owner_fits(
    rt: &MockRuntime,
    session: &SessionKey,
    r: &StoredResource,
    owner: &Owner,
    field: &str,
) -> bool {
    if owner.writing {
        return !known_foreign(rt, session, r, owner);
    }
    rt.collections()
        .fits(session, r, owner)
        .unwrap_or_else(|| match owner_evidence(rt, r, owner) {
            Some(shown) => shown,
            None => {
                mb_mcm::member_at(&r.data, field).is_some_and(Value::is_number)
                    && !links_elsewhere(r, owner)
            }
        })
}

/// Whether `r`, stored under the value a path gave, is `owner`'s to answer with.
/// A write takes it unless it is known to be another's. For a read a record
/// decides; else a key that is no name of `r` is `r`'s id, and stands on its
/// own unless `r` says otherwise. A name (in one of [`NAME_FIELDS`], or the end of a relative path) must
/// show its owner, or differ from `r`'s own `id`: only a write through the route
/// keys it so, as a list or a mention keys by the `id`.
pub(super) fn key_fits(
    rt: &MockRuntime,
    session: &SessionKey,
    r: &StoredResource,
    owner: &Owner,
) -> bool {
    if owner.writing {
        return !known_foreign(rt, session, r, owner);
    }
    if let Some(fits) = rt.collections().fits(session, r, owner) {
        return fits;
    }
    let Value::Object(members) = &r.data else {
        return true;
    };
    let is_name = NAME_FIELDS
        .iter()
        .any(|field| members.get(*field).and_then(id_string).as_ref() == Some(&r.id))
        || members
            .values()
            .filter_map(Value::as_str)
            .any(|text| relative_path_ending(text, &r.id));
    let written = members
        .get("id")
        .and_then(id_string)
        .is_some_and(|id| id != r.id);
    let evidence = owner_evidence(rt, r, owner);
    if is_name {
        written || evidence == Some(true)
    } else {
        evidence != Some(false)
    }
}

#[cfg(test)]
mod compound_param_tests {
    use super::{Compound, tied_to_param};

    #[test]
    fn a_compound_parameter_stands_for_each_alternative() {
        assert_eq!(
            Compound::of("organization_id_or_slug"),
            Some(Compound {
                stem: "organization".into(),
                keys: vec!["organization_id".into(), "organization_slug".into()],
            })
        );
        assert_eq!(Compound::of("organization"), None);
        assert_eq!(Compound::of("id_or_name"), None);
    }

    #[test]
    fn a_member_ties_to_a_compound_parameter_by_its_alternatives() {
        for member in [
            "organization",
            "organization_id",
            "organizationId",
            "organization_slug",
            "organizationSlug",
        ] {
            assert!(tied_to_param(member, "organization_id_or_slug"), "{member}");
        }
        // The record's own `slug` or `username` is not its owner's, and `_or_` is the
        // only separator.
        assert!(!tied_to_param("slug", "organization_id_or_slug"));
        assert!(!tied_to_param("organizations", "organization_id_or_slug"));
        assert!(!tied_to_param("username", "user_id_or_username"));
        assert!(!tied_to_param("team", "organization_id_or_slug"));
        assert!(!tied_to_param("organization", "organizationIdOrSlug"));
        assert!(!tied_to_param("organization", "organization_id"));
        // A key the record's own type declares is its own.
        let compound = Compound::of("organization_id_or_slug").unwrap();
        assert!(!compound.ties("organization_id", &["id", "organization_id"]));
        assert!(compound.ties("organization", &["id", "organization_id"]));
    }
}
