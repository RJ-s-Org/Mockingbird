//! The per-bundle index of record schemas, routes, lists and references.

use super::*;

/// Stored resources scanned for a read by another field, or for the
/// numbers in use.
pub(super) const MAX_SCAN: usize = 500;

/// What the pipeline looks up in a runtime's (fixed) bundle on every call,
/// computed once when the runtime is built.
#[derive(Debug, Default)]
pub(crate) struct BundleIndex {
    /// Each operation's position in `bundle.operations`, by id (the first
    /// one of a repeated id, as [`mb_mcm::Bundle::operation`] finds).
    pub(super) by_id: HashMap<String, usize>,
    /// Each channel's position in `bundle.channels`, by id (the first one
    /// of a repeated id, as [`mb_mcm::Bundle::channel`] finds).
    #[cfg(any(test, feature = "events"))]
    pub(super) channel_by_id: HashMap<String, usize>,
    /// Per resource type: the fields its operations take from the path
    /// (GitHub's issue `number`), sorted and deduplicated.
    pub(super) path_fields: HashMap<String, Vec<String>>,
    /// Per resource type: the fields its operations identify it by, those
    /// [`BundleIndex::path_fields`] holds included, sorted and deduplicated.
    pub(super) identity_fields: HashMap<String, Vec<String>>,
    /// The members every query operation's success schema lists.
    pub(super) listed: HashSet<String>,
    /// Per operation, by position: the members its success schema lists.
    pub(super) list_members: Vec<Vec<String>>,
    /// The resource type each `$defs` name is, from the query operations
    /// whose success schema or list items are that `$ref`: a read by path
    /// parameter first (`/repos/{owner}/{repo}`, not `/search/repositories`,
    /// names GitHub's `repository`), then a list, then any other read. A
    /// list under a member naming a type read by path is that type.
    pub(super) types_by_def: HashMap<String, String>,
    /// The resource types of the query operations.
    pub(super) read_types: HashSet<String>,
    /// Per resource type, the position of its first query by path parameter
    /// with a success schema: the read a stored resource must suit.
    pub(super) detail_reads: HashMap<String, usize>,
    /// Per resource type with no such read, what a stored resource must
    /// suit instead, with the position of the read it comes from: a list's
    /// item schema, else a singleton read's success schema (`/user`).
    pub(super) other_shapes: HashMap<String, (usize, Value)>,
    /// Per resource type, the types whose answers have a reference field of
    /// it (see [`reference_targets`]), sorted and deduplicated.
    pub(super) referrers: HashMap<String, Vec<String>>,
    /// The types an effect can name, those with a read to check against, sorted.
    pub(super) effect_types: Vec<String>,
    /// Per path with no parameters whose `GET` answers a single object (see
    /// [`answers_a_list`]), that operation's position.
    pub(super) single_reads: HashMap<String, usize>,
    /// Per path a command and a list read by no path parameter share, that list.
    pub(super) written_lists: HashMap<String, WrittenList>,
    /// Per path of a command with a path parameter, the list reads one level under its
    /// parent: the path through its last parameter (see [`SiblingList`]).
    pub(super) sibling_lists: HashMap<String, Vec<SiblingList>>,
    /// The paths of those lists, whose reads are told what was written to them.
    pub(super) told_lists: HashSet<String>,
    /// Per type of items, how many list reads of it the contract has whose path has no
    /// parameter (`/tasks`, `/me/tasks`, `/tasks.list`), by any method: the type their
    /// `$defs` entry is, else the operation's, as [`BundleIndex::item_type`] finds it.
    /// A search, which a required query parameter narrows ([`searches`]), is counted
    /// apart.
    pub(super) unscoped_lists: HashMap<String, usize>,
    /// Per type of items, how many such searches the contract has.
    pub(super) unscoped_searches: HashMap<String, usize>,
    /// Per path of such a list read or search, the type of its items, so counted, and
    /// whether it is a search.
    pub(super) unscoped_paths: HashMap<String, (String, bool)>,
    /// Whether a create or an action may mint a value for a client to send back (see
    /// [`minted::may_mint`]).
    pub(super) mints: bool,
    /// Per read below a record's read (`GET /playlists/{id}/items` below
    /// `/playlists/{id}`), by position, when it answers exactly the schema of one of
    /// that record's members (`tracks`): where to find it (see [`member_read`]).
    pub(super) member_reads: HashMap<usize, MemberRead>,
    /// Per resource type whose read by path answers its record in an envelope or an
    /// array, the record's own schema: what a stored record of the type must suit.
    pub(super) record_schemas: HashMap<String, Value>,
    /// Per resource type, the string members of its items that a lookup keys on: a
    /// list read whose required query parameter names the member (see
    /// [`lists::filtered_member`]), sorted. Auth0's `email`, which
    /// `/users-by-email?email=` requires; never an optional filter such as `state`.
    pub(super) lookup_members: HashMap<String, Vec<String>>,
}

/// The schema of the record an answer of `schema` holds where `identity` says: an
/// envelope's member, or an array's items. `None` for an answer that is its record.
pub(super) fn record_schema_of(
    bundle: &mb_mcm::Bundle,
    identity: &mb_mcm::Identity,
    schema: &Value,
) -> Option<Value> {
    match identity.record.as_ref()? {
        mb_mcm::RecordAt::Member(member) => property_schema(bundle, schema, member),
        mb_mcm::RecordAt::Item => {
            let (parts, _) = schema_parts(bundle, Some(schema));
            parts.iter().find_map(|p| p.get("items")).cloned()
        }
        mb_mcm::RecordAt::MemberItem(member) => {
            let list = property_schema(bundle, schema, member)?;
            let (parts, _) = schema_parts(bundle, Some(&list));
            parts.iter().find_map(|p| p.get("items")).cloned()
        }
        // An array's items at a `"0"` where the schema there is an array's, else
        // the member each token names.
        mb_mcm::RecordAt::Pointer(tokens) => tokens.iter().try_fold(schema.clone(), |at, token| {
            let items = (token == "0")
                .then(|| {
                    let (parts, _) = schema_parts(bundle, Some(&at));
                    parts.iter().find_map(|p| p.get("items")).cloned()
                })
                .flatten();
            items.or_else(|| property_schema(bundle, &at, token))
        }),
    }
}

/// The [`BundleIndex::record_schemas`] of `bundle`, from the reads by path chosen.
pub(super) fn record_schemas_of(
    bundle: &mb_mcm::Bundle,
    detail_reads: &HashMap<String, usize>,
) -> HashMap<String, Value> {
    detail_reads
        .iter()
        .filter_map(|(t, i)| {
            let op = bundle.operations.get(*i)?;
            let schema = op.success_output()?.schema.as_ref()?;
            let record = record_schema_of(bundle, op.identity.as_ref()?, schema)?;
            Some((t.clone(), record))
        })
        .collect()
}

/// Where a read of a member of its parent finds it.
#[derive(Debug, Clone)]
pub(super) struct MemberRead {
    /// The position of the parent's read.
    pub(super) parent: usize,
    /// The parent's member it answers.
    pub(super) member: String,
    /// The path parameter of this read that holds the parent's id.
    pub(super) param: String,
}

/// The [`BundleIndex::member_reads`] of `bundle`.
pub(super) fn member_reads_of(bundle: &mb_mcm::Bundle) -> HashMap<usize, MemberRead> {
    fn get(op: &Operation) -> Option<Vec<&str>> {
        match &op.binding {
            Binding::Http { method, path }
                if *method == mb_mcm::HttpMethod::Get
                    && matches!(op.kind, OperationKind::Query) =>
            {
                Some(path_segments(path))
            }
            _ => None,
        }
    }
    // A route, whatever it names its parameters.
    let route = |segments: &[&str]| {
        let shape: Vec<&str> = segments
            .iter()
            .map(|s| if segment_param(s).is_some() { "{}" } else { s })
            .collect();
        shape.join("/")
    };
    let mut records: HashMap<String, usize> = HashMap::new();
    for (i, op) in bundle.operations.iter().enumerate() {
        let by_path = op.identity.as_ref().is_some_and(|i| {
            i.path_param.is_some() && i.path_field.is_none() && i.record.is_none()
        });
        if let (Some(segments), true) = (get(op), by_path)
            && segments.last().is_some_and(|s| segment_param(s).is_some())
        {
            records.entry(route(&segments)).or_insert(i);
        }
    }
    let mut reads = HashMap::new();
    if records.is_empty() {
        return reads;
    }
    for (i, op) in bundle.operations.iter().enumerate() {
        let (Some(segments), None, Some(schema)) = (
            get(op),
            op.identity.as_ref(),
            op.success_output().and_then(|o| o.schema.as_ref()),
        ) else {
            continue;
        };
        let [.., param, last] = segments.as_slice() else {
            continue;
        };
        let (Some(param), None) = (segment_param(param), segment_param(last)) else {
            continue;
        };
        let parent_route = route(&segments[..segments.len() - 1]);
        let Some((parent, read)) = records
            .get(&parent_route)
            .and_then(|&p| Some((p, bundle.operations.get(p)?)))
        else {
            continue;
        };
        let parent_schema = read.success_output().and_then(|o| o.schema.as_ref());
        let (parts, _) = schema_parts(bundle, parent_schema);
        let mut members = parts
            .iter()
            .filter_map(|part| part.get("properties").and_then(Value::as_object))
            .flatten()
            .filter(|(_, member)| *member == schema)
            .map(|(name, _)| name.clone());
        let (Some(member), None) = (members.next(), members.next()) else {
            continue;
        };
        let read = MemberRead {
            parent,
            member,
            param: param.to_string(),
        };
        reads.insert(i, read);
    }
    reads
}

/// A list read whose path also takes writes.
#[derive(Debug, Clone)]
pub(super) struct WrittenList {
    pub(super) resource_type: String,
    /// The schema of its items: all of a write's body that it is told.
    pub(super) items: Value,
}

/// A list read one level under the parent of a command's path: the path through the
/// command's last parameter, then one literal segment, that the command's route ends in
/// (`/lists/{id}/profiles` of `POST /lists/{id}/relationships/profiles`), or of a command
/// on the parent itself (`PUT /lists/{id}`). A deeper parameter is another record, a
/// longer tail another list (`/lists/{id}/archived/profiles`), and an ancestor's lists
/// are not this parent's.
#[derive(Debug, Clone)]
pub(super) struct SiblingList {
    /// The list read's path template.
    pub(super) path: String,
    /// How many segments of the command's path are the parent's.
    pub(super) prefix: usize,
    /// The list read's one segment after the parent's.
    pub(super) rest: Vec<String>,
    /// The command's route ends in the list's segment (`relationships/profiles` and
    /// `profiles`): it writes that list. Otherwise the command is on the parent itself,
    /// and the list is told only what a body member named for it says.
    pub(super) by_route: bool,
    pub(super) list: WrittenList,
}

impl BundleIndex {
    pub(crate) fn new(bundle: &mb_mcm::Bundle) -> Self {
        let mut by_id = HashMap::with_capacity(bundle.operations.len());
        let mut path_fields: HashMap<String, Vec<String>> = HashMap::new();
        let mut identity_fields: HashMap<String, Vec<String>> = HashMap::new();
        let mut list_members_of = Vec::with_capacity(bundle.operations.len());
        let mut listed = HashSet::new();
        let mut read_types = HashSet::new();
        let mut detail_reads = HashMap::new();
        let mut ranked_defs = RankedDefs::new();
        let mut shapes = TypeShapes::new();
        let mut referrers: HashMap<String, Vec<String>> = HashMap::new();
        for (i, op) in bundle.operations.iter().enumerate() {
            by_id.entry(op.id.clone()).or_insert(i);
            if let (Some(t), Some(field)) = (
                op.resource_type.as_deref(),
                op.identity.as_ref().and_then(|i| i.path_field.as_deref()),
            ) {
                path_fields
                    .entry(t.to_string())
                    .or_default()
                    .push(field.to_string());
            }
            if let (Some(t), Some(identity)) = (op.resource_type.as_deref(), op.identity.as_ref()) {
                let fields = identity_fields.entry(t.to_string()).or_default();
                fields.push(identity.field.clone());
                fields.extend(identity.path_field.iter().cloned());
            }
            let members = op
                .success_output()
                .and_then(|o| o.schema.as_ref())
                .map(|schema| list_members(bundle, schema))
                .unwrap_or_default();
            if let Some(t) = op.resource_type.as_deref() {
                record_referrers(bundle, op, t, &members, &mut referrers);
            }
            if matches!(op.kind, OperationKind::Query) {
                listed.extend(members.iter().cloned());
                if let Some(t) = op.resource_type.as_deref() {
                    read_types.insert(t.to_string());
                    let by_path = op.identity.as_ref().is_some_and(|i| i.path_param.is_some())
                        && !op.is_action();
                    if by_path && op.success_output().is_some_and(|o| o.schema.is_some()) {
                        detail_reads.entry(t.to_string()).or_insert(i);
                    }
                    let lists = list_item_schemas(bundle, op, &members);
                    // An answer with an identity of its own is one record: the lists it
                    // holds are not of its type (a weather reading's conditions).
                    let own = op.identity.as_ref().is_some_and(|i| i.record.is_none());
                    rank_defs(op, (t, own), by_path, &lists, &mut ranked_defs);
                    record_shapes(
                        bundle,
                        op,
                        i,
                        (t, own),
                        by_path,
                        &members,
                        &lists,
                        &mut shapes,
                    );
                }
            }
            list_members_of.push(members);
        }
        for fields in path_fields
            .values_mut()
            .chain(identity_fields.values_mut())
            .chain(referrers.values_mut())
        {
            fields.sort_unstable();
            fields.dedup();
        }
        // Lists under a member of a type read by path hold that type: the
        // `offers` of a flight search are offers.
        let by_path_types: HashSet<String> = detail_reads.keys().cloned().collect();
        let in_detail_reads = |name: &String| by_path_types.contains(name);
        let (single_reads, written_lists, unscoped_lists, sibling_lists) =
            routes_of(bundle, &read_types, &list_members_of);
        let told_lists = sibling_lists
            .values()
            .flatten()
            .map(|s| s.path.clone())
            .collect();
        let record_schemas = record_schemas_of(bundle, &detail_reads);
        let mut effect_types: Vec<String> =
            detail_reads.keys().chain(shapes.keys()).cloned().collect();
        effect_types.sort_unstable();
        effect_types.dedup();
        #[cfg(any(test, feature = "events"))]
        let mut channel_by_id = HashMap::with_capacity(bundle.channels.len());
        #[cfg(any(test, feature = "events"))]
        for (i, channel) in bundle.channels.iter().enumerate() {
            channel_by_id.entry(channel.id.clone()).or_insert(i);
        }
        let types_by_def: HashMap<String, String> = ranked_defs
            .into_iter()
            .map(|(def, (_, t, member))| {
                let t = member
                    .and_then(|m| [format!("{m}s"), m].into_iter().find(in_detail_reads))
                    .unwrap_or(t);
                (def, t)
            })
            .collect();
        let lookup_members = lookup_members(bundle, &types_by_def, &list_members_of);
        // Each list counts under its items' type, as [`BundleIndex::item_type`] finds it.
        let mut unscoped_counts: HashMap<String, usize> = HashMap::new();
        let mut unscoped_searches: HashMap<String, usize> = HashMap::new();
        let mut unscoped_paths: HashMap<String, (String, bool)> = HashMap::new();
        for list in unscoped_lists {
            let t = list
                .def
                .as_deref()
                .and_then(|d| types_by_def.get(d).cloned())
                .unwrap_or(list.resource_type);
            let counts = match list.search {
                true => &mut unscoped_searches,
                false => &mut unscoped_counts,
            };
            *counts.entry(t.clone()).or_default() += 1;
            unscoped_paths.insert(list.path, (t, list.search));
        }
        Self {
            by_id,
            #[cfg(any(test, feature = "events"))]
            channel_by_id,
            path_fields,
            identity_fields,
            listed,
            list_members: list_members_of,
            types_by_def,
            lookup_members,
            read_types,
            detail_reads,
            other_shapes: shapes
                .into_iter()
                .map(|(t, (_, i, schema))| (t, (i, schema)))
                .collect(),
            referrers,
            effect_types,
            single_reads,
            written_lists,
            sibling_lists,
            told_lists,
            unscoped_lists: unscoped_counts,
            unscoped_searches,
            unscoped_paths,
            mints: bundle
                .operations
                .iter()
                .any(|op| minted::may_mint(bundle, op)),
            member_reads: member_reads_of(bundle),
            record_schemas,
        }
    }

    /// The position of operation `id`.
    pub(crate) fn operation(&self, id: &str) -> Option<usize> {
        self.by_id.get(id).copied()
    }

    /// The position of channel `id`.
    #[cfg(any(test, feature = "events"))]
    pub(crate) fn channel(&self, id: &str) -> Option<usize> {
        self.channel_by_id.get(id).copied()
    }

    pub(super) fn path_fields(&self, resource_type: &str) -> &[String] {
        self.path_fields
            .get(resource_type)
            .map_or(&[], Vec::as_slice)
    }

    pub(super) fn identity_fields(&self, resource_type: &str) -> &[String] {
        self.identity_fields
            .get(resource_type)
            .map_or(&[], Vec::as_slice)
    }

    pub(super) fn list_members(&self, op: usize) -> &[String] {
        self.list_members.get(op).map_or(&[], Vec::as_slice)
    }

    /// Whether `path`, with no parameter, is the one list read of `resource_type` with
    /// no path parameter the contract has, and a command of the type shares its path:
    /// the type's records are all that list's (`POST /tasks` beside `GET /tasks`).
    pub(super) fn sole_written_list(&self, path: &str, resource_type: &str) -> bool {
        !path.contains('{')
            && self.sole_unscoped(path, resource_type)
            && self.written_lists.contains_key(path)
    }

    /// Whether `path` is a list read of `resource_type` with no path parameter, and the
    /// type's only one: a search ([`searches`]) beside the type's written collection
    /// (a list that is no search, with a write on its path) is no second list
    /// (`/search/tickets?query=` beside `/tickets`), and is never the only one itself.
    /// Beside any other list (`/tasks?org=` beside `/me/tasks`), and where every such
    /// list of the type is a search, each counts.
    pub(super) fn sole_unscoped(&self, path: &str, resource_type: &str) -> bool {
        let Some((t, search)) = self.unscoped_paths.get(path) else {
            return false;
        };
        let lists = self.unscoped_lists.get(t).copied().unwrap_or_default();
        let searches = self.unscoped_searches.get(t).copied().unwrap_or_default();
        let count = match (lists, search) {
            (_, false) if self.written_lists.contains_key(path) => lists,
            _ => lists + searches,
        };
        t == resource_type && count == 1
    }

    pub(super) fn referrers(&self, resource_type: &str) -> &[String] {
        self.referrers.get(resource_type).map_or(&[], Vec::as_slice)
    }

    pub(super) fn lookup_members(&self, resource_type: &str) -> &[String] {
        self.lookup_members
            .get(resource_type)
            .map_or(&[], Vec::as_slice)
    }

    /// The resource type of the items of `op`'s list answer `body`: the one their
    /// `$defs` entry is, else `op`'s.
    pub(super) fn item_type(
        &self,
        bundle: &mb_mcm::Bundle,
        op: &Operation,
        body: &Value,
    ) -> Option<String> {
        let schema = op.success_output().and_then(|o| o.schema.as_ref());
        let items = schema.and_then(|schema| item_schema(bundle, schema, body));
        items
            .as_ref()
            .and_then(def_name)
            .and_then(|d| self.types_by_def.get(d).cloned())
            .or_else(|| op.resource_type.clone())
    }
}

/// Per resource type, the string members of its items a list read's required query
/// parameter keys on (see [`BundleIndex::lookup_members`]).
pub(super) fn lookup_members(
    bundle: &mb_mcm::Bundle,
    types_by_def: &HashMap<String, String>,
    members_of: &[Vec<String>],
) -> HashMap<String, Vec<String>> {
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    for (i, op) in bundle.operations.iter().enumerate() {
        let query = op.input.query.as_ref();
        let params = query
            .and_then(|q| q.get("properties"))
            .and_then(Value::as_object);
        let (true, Some(params)) = (matches!(op.kind, OperationKind::Query), params) else {
            continue;
        };
        let members = members_of.get(i).map_or(&[][..], Vec::as_slice);
        for (_, items) in list_item_schemas(bundle, op, members) {
            let t = def_name(&items)
                .and_then(|d| types_by_def.get(d).cloned())
                .or_else(|| op.resource_type.clone());
            let Some(t) = t else { continue };
            let (item_parts, _) = schema_parts(bundle, Some(&items));
            // Only a parameter the route requires keys a lookup; an optional one
            // (`state`, `status`) narrows a list of many.
            let required = query
                .and_then(|q| q.get("required"))
                .and_then(Value::as_array);
            let keys = params.keys().filter(|param| {
                required.is_some_and(|names| names.iter().any(|n| n.as_str() == Some(param)))
            });
            for param in keys {
                if let Some(member) = lists::filtered_member(bundle, query, &item_parts, param)
                    && member.is_text()
                {
                    out.entry(t.clone())
                        .or_default()
                        .push(member.name.to_string());
                }
            }
        }
    }
    for fields in out.values_mut() {
        fields.sort_unstable();
        fields.dedup();
    }
    out
}

/// A list read whose path has no parameter, by any method: its path, the `$defs` name
/// of its items, when they are one, and its operation's resource type.
pub(super) struct UnscopedList {
    pub(super) path: String,
    pub(super) def: Option<String>,
    pub(super) resource_type: String,
    /// Whether it is a search ([`searches`]).
    pub(super) search: bool,
}

/// Whether list read `op` is a search: its query requires a parameter, so it answers
/// only the records that parameter picks (`/search/tickets?query=`,
/// `/users-by-email?email=`), never every record of its type.
pub(super) fn searches(op: &Operation) -> bool {
    op.input
        .query
        .as_ref()
        .and_then(|q| q.get("required"))
        .and_then(Value::as_array)
        .is_some_and(|required| !required.is_empty())
}

/// What [`routes_of`] finds: single reads, written lists, unscoped lists and sibling lists.
pub(super) type Routes = (
    HashMap<String, usize>,
    HashMap<String, WrittenList>,
    Vec<UnscopedList>,
    HashMap<String, Vec<SiblingList>>,
);

/// What routes the contract's operations share: see [`BundleIndex::single_reads`],
/// [`BundleIndex::written_lists`] and [`BundleIndex::unscoped_lists`].
pub(super) fn routes_of(
    bundle: &mb_mcm::Bundle,
    read_types: &HashSet<String>,
    members_of: &[Vec<String>],
) -> Routes {
    let (mut single_reads, mut written_lists) = (HashMap::new(), HashMap::new());
    let mut unscoped_lists: Vec<UnscopedList> = Vec::new();
    let mut lists: HashMap<&str, WrittenList> = HashMap::new();
    for (i, op) in bundle.operations.iter().enumerate() {
        let Binding::Http { method, path } = &op.binding else {
            continue;
        };
        if !matches!(op.kind, OperationKind::Query) {
            continue;
        }
        let members = members_of.get(i).map_or(&[][..], Vec::as_slice);
        let items = listed_items(bundle, op, read_types, members);
        if let (Some(items), Some(t)) = (&items, op.resource_type.as_deref())
            && !path.contains('{')
        {
            unscoped_lists.push(UnscopedList {
                path: path.clone(),
                def: def_name(items).map(str::to_string),
                resource_type: t.to_string(),
                search: searches(op),
            });
        }
        if *method != mb_mcm::HttpMethod::Get {
            continue;
        }
        if let (Some(items), Some(t), true) = (
            &items,
            op.resource_type.as_deref(),
            op.identity.as_ref().is_none_or(|i| i.path_param.is_none()),
        ) {
            let list = WrittenList {
                resource_type: t.to_string(),
                items: items.clone(),
            };
            lists.insert(path.as_str(), list);
        }
        let parameters = path.contains('{') || op.input.path.is_some() || op.input.query.is_some();
        if items.is_none() && !parameters && op.success_output().is_some_and(|o| o.schema.is_some())
        {
            single_reads.insert(path.clone(), i);
        }
    }
    let mut sibling_lists: HashMap<String, Vec<SiblingList>> = HashMap::new();
    for op in &bundle.operations {
        let (OperationKind::Command(_), Binding::Http { path, .. }) = (&op.kind, &op.binding)
        else {
            continue;
        };
        if let Some(list) = lists.get(path.as_str()) {
            written_lists.insert(path.clone(), list.clone());
        }
        if !sibling_lists.contains_key(path) {
            let siblings = siblings_of(path, &lists);
            if !siblings.is_empty() {
                sibling_lists.insert(path.clone(), siblings);
            }
        }
    }
    (single_reads, written_lists, unscoped_lists, sibling_lists)
}

/// The list reads of `lists` one level under the parent of a command's `path` that it
/// can be told to (see [`SiblingList`]), sorted by path. None when the path has no
/// parameter.
pub(super) fn siblings_of(path: &str, lists: &HashMap<&str, WrittenList>) -> Vec<SiblingList> {
    let segments = path_segments(path);
    let Some(last) = segments.iter().rposition(|s| segment_param(s).is_some()) else {
        return Vec::new();
    };
    let prefix = last + 1;
    let own_end = segments.last().filter(|_| segments.len() > prefix);
    let on_parent = segments.len() == prefix;
    // Its own route's list is told by its path (see [`BundleIndex::written_lists`]).
    let mut out: Vec<SiblingList> = lists
        .iter()
        .filter_map(|(list, read)| {
            let below = path_segments(list);
            let rest = below.get(prefix..).filter(|rest| rest.len() == 1)?;
            let by_route = own_end.is_some_and(|end| rest.last() == Some(end));
            if !(by_route || on_parent)
                || write_memory::same_route(&segments, &below)
                || !write_memory::same_route(&segments[..prefix], &below[..prefix])
                || rest.iter().any(|s| segment_param(s).is_some())
            {
                return None;
            }
            Some(SiblingList {
                path: (*list).to_string(),
                prefix,
                rest: rest.iter().map(|s| (*s).to_string()).collect(),
                by_route,
                list: read.clone(),
            })
        })
        .collect();
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// The item schema of `op`'s success answer when it is a list: an array, or an object
/// holding one in an envelope's member or under a member that is itself a type read. An
/// object that only has a list among its members (a profile's `images`) is a single object.
pub(super) fn listed_items(
    bundle: &mb_mcm::Bundle,
    op: &Operation,
    read_types: &HashSet<String>,
    members: &[String],
) -> Option<Value> {
    list_item_schemas(bundle, op, members)
        .into_iter()
        .find(|(member, _)| {
            member.as_deref().is_none_or(|m| {
                ENVELOPE_KEYS.contains(&m)
                    || op.resource_type.as_deref() == Some(m)
                    || read_types.contains(m)
                    || read_types.contains(&format!("{m}s"))
            })
        })
        .map(|(_, items)| items)
}

/// Records in `referrers` the types the fields of `op`'s answers (as type
/// `t`, its list items included) refer to.
pub(super) fn record_referrers(
    bundle: &mb_mcm::Bundle,
    op: &Operation,
    t: &str,
    members: &[String],
    referrers: &mut HashMap<String, Vec<String>>,
) {
    let Some(schema) = op.success_output().and_then(|o| o.schema.as_ref()) else {
        return;
    };
    let items = list_item_schemas(bundle, op, members);
    let shapes = std::iter::once(schema).chain(items.iter().map(|(_, s)| s));
    for shape in shapes {
        for part in schema_parts(bundle, Some(shape)).0 {
            let Some(properties) = part.get("properties").and_then(Value::as_object) else {
                continue;
            };
            for (field, property) in properties {
                for target in reference_targets(bundle, field, property) {
                    referrers.entry(target).or_default().push(t.to_string());
                }
            }
        }
    }
}

/// The types a field of an answer's schema can refer to: `<x>s` and `<x>`
/// for `<x>Id`/`<x>_id`, or for a member `<x>` that can hold a string.
pub(super) fn reference_targets(
    bundle: &mb_mcm::Bundle,
    field: &str,
    property: &Value,
) -> Vec<String> {
    let prefix = reference_prefix(field)
        .or_else(|| (!field.is_empty() && holds_a_string(bundle, property)).then_some(field));
    prefix.map_or_else(Vec::new, |x| vec![format!("{x}s"), x.to_string()])
}

/// Whether `schema` admits a string, or a list of them.
pub(super) fn holds_a_string(bundle: &mb_mcm::Bundle, schema: &Value) -> bool {
    let string = |s: &Value| {
        schema_parts(bundle, Some(s))
            .0
            .iter()
            .any(|p| match p.get("type") {
                Some(Value::String(t)) => t == "string",
                Some(Value::Array(types)) => types.iter().any(|t| t == "string"),
                _ => false,
            })
    };
    string(schema)
        || schema_parts(bundle, Some(schema))
            .0
            .iter()
            .filter_map(|p| p.get("items"))
            .any(string)
}

/// The item schemas of `op`'s success schema: each of its list `members`'
/// (with the member), and the root's when it is an array itself.
pub(super) fn list_item_schemas(
    bundle: &mb_mcm::Bundle,
    op: &Operation,
    members: &[String],
) -> Vec<(Option<String>, Value)> {
    let Some(schema) = op.success_output().and_then(|o| o.schema.as_ref()) else {
        return Vec::new();
    };
    let items_of = |list: &Value| {
        let (parts, _) = schema_parts(bundle, Some(list));
        parts.iter().find_map(|p| p.get("items")).cloned()
    };
    let mut out: Vec<(Option<String>, Value)> = members
        .iter()
        .filter_map(|m| {
            let items = items_of(&property_schema(bundle, schema, m)?)?;
            Some((Some(m.clone()), items))
        })
        .collect();
    out.extend(items_of(schema).map(|items| (None, items)));
    out
}

/// Per `$defs` name: the best rank seen, the type of the read it was seen in
/// and, for a list, the non-envelope member holding it.
pub(super) type RankedDefs = HashMap<String, (u8, String, Option<String>)>;

/// Records in `ranked` the `$defs` names `op` reads as resource type `t`,
/// keeping the best-ranked type of each (see [`BundleIndex::types_by_def`]).
pub(super) fn rank_defs(
    op: &Operation,
    (t, own): (&str, bool),
    by_path: bool,
    lists: &[(Option<String>, Value)],
    ranked: &mut RankedDefs,
) {
    const DETAIL: u8 = 0;
    const LIST: u8 = 1;
    const OTHER: u8 = 2;
    let Some(schema) = op.success_output().and_then(|o| o.schema.as_ref()) else {
        return;
    };
    let mut found: Vec<(u8, String, Option<String>)> = Vec::new();
    if let Some(def) = def_name(schema) {
        found.push((if by_path { DETAIL } else { OTHER }, def.to_string(), None));
    }
    let items = lists.iter().filter_map(|(member, items)| {
        let member = member
            .as_ref()
            .filter(|m| !ENVELOPE_KEYS.contains(&m.as_str()));
        Some((LIST, def_name(items)?.to_string(), member.cloned()))
    });
    found.extend(items);
    for (rank, def, member) in found {
        // A list held by one record says nothing of what its items are: they are of
        // a type only where another read serves exactly their schema.
        if own && rank == LIST {
            continue;
        }
        let held = ranked
            .entry(def)
            .or_insert_with(|| (rank, t.to_string(), member.clone()));
        if rank < held.0 {
            *held = (rank, t.to_string(), member);
        }
    }
}

/// Per resource type: the best rank seen, the read's position and the schema.
pub(super) type TypeShapes = HashMap<String, (u8, usize, Value)>;

/// Records in `shapes` what query `op` (at position `i`, of type `t`) says a
/// resource looks like (see [`BundleIndex::other_shapes`]): a list's items
/// describe the type the member names and, for the root, an envelope or an
/// only member, `t`; with no list, its success schema describes `t`, or the
/// record its identity says the answer holds.
pub(super) fn record_shapes(
    bundle: &mb_mcm::Bundle,
    (op, i): (&Operation, usize),
    (t, own): (&str, bool),
    by_path: bool,
    members: &[String],
    lists: &[(Option<String>, Value)],
    shapes: &mut TypeShapes,
) {
    const ITEMS: u8 = 0;
    const SINGLETON: u8 = 1;
    let mut record = |t: &str, rank: u8, schema: &Value| {
        let held = shapes
            .entry(t.to_string())
            .or_insert_with(|| (rank, i, schema.clone()));
        if rank < held.0 {
            *held = (rank, i, schema.clone());
        }
    };
    for (member, items) in lists {
        match member.as_deref() {
            // What a record holds is no shape of a type, its own or one named for
            // the member.
            Some(_) if own => {}
            None => record(t, ITEMS, items),
            Some(m) if ENVELOPE_KEYS.contains(&m) || members.len() == 1 => {
                record(t, ITEMS, items);
                if !ENVELOPE_KEYS.contains(&m) {
                    record(m, ITEMS, items);
                }
            }
            Some(m) => record(m, ITEMS, items),
        }
    }
    if (lists.is_empty() || own)
        && !by_path
        && let Some(schema) = op.success_output().and_then(|o| o.schema.as_ref())
    {
        // A read that names its record otherwise than by path (a body member: Plaid's
        // `POST /item/get {access_token}`) and holds it in an envelope (`{item,
        // request_id}`) answers the record's own schema there, not the envelope's.
        match op
            .identity
            .as_ref()
            .and_then(|identity| record_schema_of(bundle, identity, schema))
        {
            Some(held) => record(t, SINGLETON, &held),
            None => record(t, SINGLETON, schema),
        }
    }
}

/// The `$defs` name `schema` is a `$ref` to.
pub(super) fn def_name(schema: &Value) -> Option<&str> {
    schema.get("$ref")?.as_str()?.strip_prefix("#/$defs/")
}

/// Schemas [`schema_parts`] returns, at most.
pub(super) const MAX_SCHEMA_PARTS: usize = 32;

/// What `schema` is made of, its `$ref`s followed and its
/// `anyOf`/`oneOf`/`allOf` branches flattened, with the `$defs` names met.
pub(super) fn schema_parts<'a>(
    bundle: &'a mb_mcm::Bundle,
    schema: Option<&'a Value>,
) -> (Vec<&'a Value>, Vec<&'a str>) {
    fn visit<'a>(
        bundle: &'a mb_mcm::Bundle,
        schema: &'a Value,
        depth: usize,
        parts: &mut Vec<&'a Value>,
        defs: &mut Vec<&'a str>,
    ) {
        if depth > 8 || parts.len() >= MAX_SCHEMA_PARTS {
            return;
        }
        if let Some(def) = def_name(schema) {
            defs.push(def);
            if let Some(target) = bundle.defs.get(def) {
                visit(bundle, target, depth + 1, parts, defs);
            }
            return;
        }
        parts.push(schema);
        for key in ["anyOf", "oneOf", "allOf"] {
            for branch in schema
                .get(key)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                visit(bundle, branch, depth + 1, parts, defs);
            }
        }
    }
    let (mut parts, mut defs) = (Vec::new(), Vec::new());
    if let Some(schema) = schema {
        visit(bundle, schema, 0, &mut parts, &mut defs);
    }
    (parts, defs)
}

/// The resource type of an object found under `member`, shaped by `$defs`
/// entry `def`: the def's type, else the listed type named `member` or
/// `<member>s`, one a read can name by path first (GitHub's `user` member is
/// a `/users/{username}`, not the caller's `/user`).
pub(crate) fn entity_type(index: &BundleIndex, def: Option<&str>, member: &str) -> Option<String> {
    if let Some(t) = def.and_then(|d| index.types_by_def.get(d)) {
        return Some(t.clone());
    }
    if member.is_empty() {
        return None;
    }
    let names = [member.to_string(), format!("{member}s")];
    // An envelope's member (`items`) names no type of its own.
    let is_type = |name: &String| {
        index.read_types.contains(name)
            || (index.listed.contains(name) && !ENVELOPE_KEYS.contains(&name.as_str()))
    };
    names
        .iter()
        .find(|name| index.detail_reads.contains_key(*name))
        .or_else(|| names.iter().find(|name| is_type(name)))
        .cloned()
}

impl MockRuntime {
    /// The operation `id` of [`MockRuntime::bundle`] (the first of a
    /// repeated id), without scanning the operations. (SOAP keeps its own
    /// operation positions.)
    #[cfg(any(test, feature = "http-rest", feature = "graphql", feature = "grpc"))]
    pub(crate) fn operation(&self, id: &str) -> Option<&Operation> {
        self.bundle.operations.get(self.index.operation(id)?)
    }

    /// The channel `id` of [`MockRuntime::bundle`] (the first of a repeated
    /// id), without scanning the channels.
    #[cfg(any(test, feature = "events"))]
    pub(crate) fn channel(&self, id: &str) -> Option<&mb_mcm::Channel> {
        self.bundle.channels.get(self.index.channel(id)?)
    }
}
