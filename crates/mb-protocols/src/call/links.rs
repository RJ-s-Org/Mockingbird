//! The URL members of a record the mock stores, made to agree with what the call names.
//!
//! A member is a URL member when its schema says it holds a URL (a URL `format`, see
//! [`pages::is_url`]) or it is named `url`, `href`, `resourcePath`, `<x>_url` or `<x>Url`.
//! A model writes such a value blind (`https://example.invalid/issues/142`), so where it
//! shows the record's place, the place is rebuilt from what the server knows:
//!
//! - **A type read by path** (`/projects/{project}/tickets/{number}`): the value's path is
//!   matched against that template ([`align`]), a collection named in the singular
//!   matching one named in the plural. A value showing the template's end (the record
//!   itself) is the template filled: the call's path values for its parameters (by
//!   position against the call's own path), the record's own identity for its last
//!   one, and the base path the contract's `servers` give in front, unless the contract's
//!   example of the member starts it elsewhere (an `html_url` example under another host
//!   leaving `/repos` out), or the value itself starts after the template's leading
//!   literals. Its spelling of each literal stays. A value showing less of it (a parent's
//!   link, `repository_url`) has only the parameters it shows set. The record's own link
//!   (a member whose example shows the record's place, else `url`, `href`,
//!   `resourcePath`, `html_url` or `htmlUrl`) is built from the template whatever it
//!   held (a word the faker drew), a suffix of its last segment kept (`7.diff`);
//!   another link showing the whole template names another record of the type and
//!   keeps its identity, unless it goes on past it (`…/7/comments`); any other value the
//!   template does not match is left as it is.
//! - **A type with no read by path** (a GraphQL type): the value is placed under the URL
//!   the record's parent holds in the same member, the parent being the record a
//!   required id argument of the create names (`createIssue(input: {repositoryId})`'s
//!   repository): its collection segment and own identity follow the parent's path
//!   (`…/owner/repo` + `issues/7`), and a value pointing at the parent's own place
//!   (`…/issues/42#issuecomment-1` for a comment) is the parent's URL, its fragment kept.
//! - **The host**: a value rebuilt along the read path takes the contract's example host
//!   for its member, when it has one. Otherwise a placeholder (`example.com`,
//!   `example.org`, `example.net`, `.invalid`, `.example`) is replaced by the example
//!   host, else the parent's, else the mock's first host ([`crate::MockRuntime::hosts`]:
//!   its own `hosts`, then its contract's `servers`); the record's own link rebuilt along
//!   the read path, and a value placed under its parent, also move to that host when
//!   their own is not one the mock knows (its hosts, those hosts without a leading
//!   `api.`, the hosts of the contract's URL examples and the parent's). Nothing is
//!   changed where no host is known.
//!
//! A member the request sent, or the description sets, is the client's or the author's,
//! and is never changed.
//!
//! What a type's links are built from is worked out once per type ([`TypeLinks`]); a call
//! only matches its values against it. Only the record's top-level members are looked
//! at; a relative value (`resourcePath`) stays relative.

use std::collections::HashMap;
use std::sync::{Arc, PoisonError};

use mb_engine::consistency::{property_schema, required_members};

use super::bundle_index::schema_parts;
use super::entities::type_check;
use super::owners::{path_segments, segment_param};
use super::relations::{Reference, references_in};
use super::*;

/// What a type's URL members are built from, computed once per type.
#[derive(Debug, Default)]
pub(crate) struct TypeLinks {
    /// The path template of the type's read by path, by segment
    /// (`[projects, {project}, tickets, {number}]`); empty when it has none or its path
    /// does not end in a parameter.
    read: Vec<String>,
    /// How many literal segments lead `read` (`repos` of `/repos/{owner}/…`).
    leading: usize,
    /// The record member the read's last parameter names.
    own: Option<String>,
    /// The members whose schema says they hold a URL.
    members: Vec<String>,
    /// Per member, what the contract's example of it shows.
    examples: HashMap<String, Example>,
    /// The hosts of the contract's URL examples, placeholders aside.
    hosts: Vec<String>,
}

/// What a contract's example of a URL member shows.
#[derive(Debug, Default)]
struct Example {
    /// Its origin (`https://host`), unless its host is a placeholder.
    origin: Option<String>,
    /// Where its path starts in [`TypeLinks::read`], when it shows the record's place.
    start: Option<usize>,
    /// The segments before that.
    prefix: Vec<String>,
    /// Its spelling of the literals of `read` it shows, by position in `read`.
    spelled: HashMap<usize, String>,
}

impl TypeLinks {
    fn of(rt: &MockRuntime, t: &str) -> Self {
        let bundle = rt.bundle();
        let mut links = TypeLinks::default();
        let mut schemas: Vec<&Value> = Vec::new();
        if let Some((read, shape)) = type_check(rt, t) {
            if let Some(schema) = shape.or_else(|| read.success_output()?.schema.as_ref()) {
                schemas.push(schema);
            }
            if let Binding::Http { path, .. } = &read.binding {
                let read_segments: Vec<String> = path_segments(path)
                    .into_iter()
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .collect();
                if let Some(last) = read_segments.last().and_then(|s| segment_param(s)) {
                    links.own = read
                        .identity
                        .as_ref()
                        .filter(|i| i.path_param.as_deref() == Some(last))
                        .map(|i| i.path_field.clone().unwrap_or_else(|| i.field.clone()));
                    links.leading = read_segments
                        .iter()
                        .take_while(|s| segment_param(s).is_none())
                        .count();
                    links.read = read_segments;
                }
            }
        }
        // A type nothing reads by path (a GraphQL type) is its definition.
        if let Some(def) = bundle.defs.get(t).or_else(|| {
            bundle
                .defs
                .iter()
                .find(|(d, _)| d.eq_ignore_ascii_case(t))
                .map(|(_, s)| s)
        }) {
            schemas.push(def);
        }
        for schema in schemas {
            let (parts, _) = schema_parts(bundle, Some(schema));
            let whole_example = parts.iter().find_map(|p| {
                p.get("example")
                    .or_else(|| p.get("examples")?.as_array()?.first())
                    .filter(|e| e.is_object())
            });
            let Some(props) = shared::record_props(bundle, schema) else {
                continue;
            };
            for (name, member) in props {
                let declared = pages::is_url(bundle, member);
                if declared && !links.members.contains(name) {
                    links.members.push(name.clone());
                }
                if !(declared || url_named(name)) || links.examples.contains_key(name) {
                    continue;
                }
                let (member_parts, _) = schema_parts(bundle, Some(member));
                let example = member_parts
                    .iter()
                    .find_map(|p| {
                        p.get("example")
                            .or_else(|| p.get("examples")?.as_array()?.first())
                    })
                    .or_else(|| whole_example.and_then(|e| e.get(name)))
                    .and_then(Value::as_str);
                if let Some(example) = example.and_then(|e| links.example(e)) {
                    links.examples.insert(name.clone(), example);
                }
            }
        }
        links
    }

    /// What `text`, an example of a member, shows; `None` when it is no URL.
    fn example(&mut self, text: &str) -> Option<Example> {
        let url = Url::parse(text)?;
        let origin = url.origin.filter(|o| !placeholder(&host_of(o)));
        if let Some(origin) = origin {
            let host = host_of(origin);
            if !self.hosts.contains(&host) {
                self.hosts.push(host);
            }
        }
        let mut example = Example {
            origin: origin.map(str::to_owned),
            ..Example::default()
        };
        if let Some((i, j, k)) = align(&url.segments, &self.read)
            && j + k == self.read.len()
        {
            example.start = Some(j);
            example.prefix = url.segments[..i].iter().map(|s| (*s).to_owned()).collect();
            for x in j..j + k {
                if segment_param(&self.read[x]).is_none() {
                    example
                        .spelled
                        .insert(x, url.segments[i + x - j].to_owned());
                }
            }
        }
        Some(example)
    }

    fn is_url_member(&self, name: &str) -> bool {
        url_named(name) || self.members.iter().any(|m| m == name)
    }
}

/// The links of type `t`, worked out on first use and kept with the bundle's index.
fn type_links(rt: &MockRuntime, t: &str) -> Arc<TypeLinks> {
    let cache = &rt.index().links;
    let held = cache.read().unwrap_or_else(PoisonError::into_inner);
    if let Some(links) = held.get(t) {
        return Arc::clone(links);
    }
    drop(held);
    let made = Arc::new(TypeLinks::of(rt, t));
    cache
        .write()
        .unwrap_or_else(PoisonError::into_inner)
        .entry(t.to_owned())
        .or_insert_with(|| Arc::clone(&made));
    made
}

/// Whether member `name` is named as a URL is: `url`, `href`, `resourcePath`, `<x>_url`,
/// `<x>Url`.
fn url_named(name: &str) -> bool {
    matches!(name, "url" | "href" | "resourcePath")
        || (name.len() > 4 && name.ends_with("_url"))
        || (name.len() > 3 && name.ends_with("Url"))
}

/// Whether `host` is a placeholder no real API serves: `example.com`, `example.org`,
/// `example.net` and their subdomains, and any host under `.invalid` or `.example`.
fn placeholder(host: &str) -> bool {
    let host = host.trim_end_matches('.');
    ["example.com", "example.org", "example.net"]
        .iter()
        .any(|d| host == *d || host.strip_suffix(d).is_some_and(|h| h.ends_with('.')))
        || ["invalid", "example"]
            .iter()
            .any(|tld| host == *tld || host.strip_suffix(tld).is_some_and(|h| h.ends_with('.')))
}

/// The host of `origin` (`https://user@Host:8443`), lower case.
fn host_of(origin: &str) -> String {
    let authority = origin.split_once("://").map_or(origin, |(_, a)| a);
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = match host.strip_prefix('[') {
        Some(v6) => v6.split_once(']').map_or(v6, |(h, _)| h),
        None => host.split(':').next().unwrap_or_default(),
    };
    host.to_ascii_lowercase()
}

/// A URL value, cut where its place can be rebuilt.
struct Url<'a> {
    /// `scheme://authority`; `None` for a path alone.
    origin: Option<&'a str>,
    /// The path's segments, empty ones left out.
    segments: Vec<&'a str>,
    /// Whether the path ends in `/`.
    slash: bool,
    /// The query and fragment, as written.
    rest: &'a str,
}

impl<'a> Url<'a> {
    /// `text` as an absolute URL or an absolute path; `None` for anything else.
    fn parse(text: &'a str) -> Option<Self> {
        if text.is_empty() || text.contains(char::is_whitespace) {
            return None;
        }
        let (origin, after) = match text.find("://") {
            Some(at)
                if at > 0
                    && text[..at]
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c)) =>
            {
                let end = text[at + 3..]
                    .find(['/', '?', '#'])
                    .map_or(text.len(), |p| at + 3 + p);
                if end == at + 3 {
                    return None;
                }
                (Some(&text[..end]), &text[end..])
            }
            _ if text.starts_with('/') && !text.starts_with("//") => (None, text),
            _ => return None,
        };
        let cut = after.find(['?', '#']).unwrap_or(after.len());
        let (path, rest) = after.split_at(cut);
        Some(Url {
            origin,
            segments: path.split('/').filter(|s| !s.is_empty()).collect(),
            slash: path.ends_with('/'),
            rest,
        })
    }

    /// The URL with `origin` and `segments` in place of its own, ending in `/` when
    /// `slash`.
    fn render(&self, origin: Option<&str>, segments: &[String], slash: bool) -> String {
        let mut out = origin.unwrap_or_default().to_owned();
        for segment in segments {
            out.push('/');
            out.push_str(segment);
        }
        if slash || (segments.is_empty() && origin.is_none()) {
            out.push('/');
        }
        out.push_str(self.rest);
        out
    }
}

/// Whether path segment `segment` is template segment `template`'s: anything but a
/// template of its own for a parameter, else the literal regardless of case, in the
/// singular or the plural (`pull` and `pulls`).
fn fits(segment: &str, template: &str) -> bool {
    if segment_param(template).is_some() {
        return !segment.contains(['{', '}']);
    }
    let (a, b) = (segment.to_ascii_lowercase(), template.to_ascii_lowercase());
    a == b || a.strip_suffix('s') == Some(&b) || b.strip_suffix('s') == Some(&a)
}

/// The longest run of `segments` that fits a run of `template`, holding a literal and a
/// parameter of it: `(start in segments, start in template, length)`. Of runs as long,
/// one reaching the template's end, then the earliest.
fn align(segments: &[&str], template: &[String]) -> Option<(usize, usize, usize)> {
    let mut best: Option<(usize, usize, usize)> = None;
    for i in 0..segments.len() {
        for j in 0..template.len() {
            let mut k = 0;
            while i + k < segments.len()
                && j + k < template.len()
                && fits(segments[i + k], &template[j + k])
            {
                k += 1;
            }
            let window = &template[j..j + k];
            let params = window.iter().filter(|s| segment_param(s).is_some()).count();
            if params == 0 || params == k {
                continue;
            }
            let better = match best {
                None => true,
                Some((_, bj, bk)) => {
                    k > bk || (k == bk && j + k == template.len() && bj + bk != template.len())
                }
            };
            if better {
                best = Some((i, j, k));
            }
        }
    }
    best
}

/// The values of `read`'s parameters `call` gives: its path values, by position where
/// its own path runs as `read` does (`/projects/{project}/tickets` under
/// `/projects/{project}/tickets/{number}`).
fn read_params(read: &[String], op: &Operation, call: &OperationCall) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let Binding::Http { path, .. } = &op.binding else {
        return out;
    };
    let own: Vec<&str> = path_segments(path)
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect();
    for (segment, template) in own.iter().zip(read) {
        match (segment_param(segment), segment_param(template)) {
            (Some(param), Some(name)) => {
                if let Some(value) = call.path_params.get(param).and_then(id_string) {
                    out.insert(name.to_owned(), value);
                }
            }
            (None, None) if fits(segment, template) => {}
            _ => break,
        }
    }
    out
}

/// What a call knows of the record whose links it settles.
struct Known<'a> {
    links: &'a TypeLinks,
    /// The read's parameters' values.
    params: HashMap<String, String>,
    /// The value of the read's last parameter: the record's own.
    own: Option<String>,
    /// The record's identity values, its own first.
    owns: Vec<String>,
    /// The base path's segments.
    base: Vec<String>,
    /// The mock's first host.
    host: Option<&'a str>,
    /// The hosts a value may keep.
    accepted: Vec<String>,
    /// The records the call's required id arguments name.
    parents: &'a [StoredResource],
}

impl Known<'_> {
    /// `text`, member `name`'s value, with its place and host as the call says; `None`
    /// when they already are, or nothing better is known.
    fn settle(&self, name: &str, text: &str) -> Option<String> {
        let example = self.links.examples.get(name);
        let own_link = self.own_link(name, example);
        let Some(url) = Url::parse(text) else {
            // No URL at all (a drawn word): the record's own link is built, when its
            // read says where it is.
            if !own_link {
                return None;
            }
            let path = self.build(example)?;
            let origin = match name {
                "resourcePath" => None,
                _ => Some(
                    example
                        .and_then(|e| e.origin.clone())
                        .or_else(|| self.host.map(|h| format!("https://{h}")))?,
                ),
            };
            let blank = Url {
                origin: None,
                segments: Vec::new(),
                slash: false,
                rest: "",
            };
            return Some(blank.render(origin.as_deref(), &path, false));
        };
        let parent = self.parent_url(name);
        let rebuilt = if self.links.read.is_empty() {
            parent
                .as_ref()
                .and_then(|p| self.under_parent(&url.segments, &p.segments))
        } else {
            self.along_read(&url.segments, example, own_link)
                .or_else(|| own_link.then(|| self.build(example)).flatten())
        };
        let origin = url.origin.map(|origin| {
            let host = host_of(origin);
            let example_origin = example.and_then(|e| e.origin.as_deref());
            let parent_origin = parent
                .as_ref()
                .and_then(|p| p.origin)
                .filter(|o| !placeholder(&host_of(o)));
            if rebuilt.is_some()
                && !self.links.read.is_empty()
                && let Some(example) = example_origin
            {
                return example.to_owned();
            }
            // A link to anything but the record (an avatar) may live on any host.
            let off = placeholder(&host)
                || (rebuilt.is_some()
                    && (own_link || self.links.read.is_empty())
                    && !self.accepted.contains(&host));
            if !off {
                return origin.to_owned();
            }
            let scheme = origin.split_once("://").map_or("https", |(s, _)| s);
            example_origin
                .map(str::to_owned)
                .or_else(|| parent_origin.map(str::to_owned))
                .or_else(|| self.host.map(|h| format!("{scheme}://{h}")))
                .unwrap_or_else(|| origin.to_owned())
        });
        let segments: Vec<String> =
            rebuilt.unwrap_or_else(|| url.segments.iter().map(|s| (*s).to_owned()).collect());
        let same_path = segments.len() == url.segments.len()
            && segments.iter().zip(&url.segments).all(|(a, b)| a == b);
        if same_path && origin.as_deref() == url.origin {
            return None;
        }
        let slash = url.slash && (same_path || !url.segments.is_empty());
        Some(url.render(origin.as_deref(), &segments, slash))
    }

    /// Whether member `name` is the record's own link, which may be built whatever it
    /// held: one the contract's example shows the record's place in, else one named
    /// `url`, `href`, `resourcePath`, `html_url` or `htmlUrl`. A link to anything else
    /// (`avatar_url`, `comments_url`) is only ever corrected.
    fn own_link(&self, name: &str, example: Option<&Example>) -> bool {
        !self.links.read.is_empty()
            && match example {
                Some(e) => e.start.is_some(),
                None => matches!(
                    name,
                    "url" | "href" | "resourcePath" | "html_url" | "htmlUrl"
                ),
            }
    }

    /// The record's place along the type's read by path: from where the member's example
    /// starts it, else the base path and the whole read path; `None` when a parameter is
    /// unknown.
    fn build(&self, example: Option<&Example>) -> Option<Vec<String>> {
        match example.and_then(|e| e.start.map(|s| (e, s))) {
            Some((e, start)) => self.fill(e.prefix.clone(), start, &|_| None, (Some(e), true)),
            None => self.fill(self.base.clone(), 0, &|_| None, (None, true)),
        }
    }

    /// `out` followed by the type's read path from `start`, filled: the record's own
    /// identity for its last parameter when `own` (a suffix the value gave it, `.diff`,
    /// kept), the call's values for the others, else what `shown` (the value's segment
    /// at that place) says; each literal as `shown` spells it, else as the example does.
    /// `None` when a parameter has no value.
    fn fill(
        &self,
        mut out: Vec<String>,
        start: usize,
        shown: &dyn Fn(usize) -> Option<String>,
        (example, own): (Option<&Example>, bool),
    ) -> Option<Vec<String>> {
        let read = &self.links.read;
        let n = read.len();
        for (x, template) in read.iter().enumerate().skip(start) {
            let value = match segment_param(template) {
                Some(_) if x + 1 == n && own && self.own.is_some() => {
                    let suffix = shown(x)
                        .and_then(|s| s.find('.').map(|at| s[at..].to_owned()))
                        .unwrap_or_default();
                    self.own.as_ref().map(|o| format!("{o}{suffix}"))
                }
                Some(param) => self.params.get(param).cloned().or_else(|| shown(x)),
                None => shown(x)
                    .or_else(|| example.and_then(|e| e.spelled.get(&x).cloned()))
                    .or_else(|| Some(template.clone())),
            };
            out.push(value?);
        }
        Some(out)
    }

    /// The path of a value following the type's read by path; `None` when the value
    /// does not, or a parameter it needs is unknown. The record's own identity goes in
    /// only for its own link (`own_link`), or a value going on past the read path (its
    /// `/comments`): another link of the full path (a `duplicate_of_url`) names another
    /// record of the type.
    fn along_read(
        &self,
        segments: &[&str],
        example: Option<&Example>,
        own_link: bool,
    ) -> Option<Vec<String>> {
        let read = &self.links.read;
        let n = read.len();
        let (i, j, k) = align(segments, read)?;
        let shown = |x: usize| {
            (j..j + k)
                .contains(&x)
                .then(|| segments[i + x - j].to_owned())
        };
        if j + k < n {
            // A link to what holds the record: only the parameters it shows are set.
            let mut out: Vec<String> = segments.iter().map(|s| (*s).to_owned()).collect();
            for x in j..j + k {
                if let Some(value) = segment_param(&read[x]).and_then(|p| self.params.get(p)) {
                    out[i + x - j].clone_from(value);
                }
            }
            return Some(out);
        }
        let (prefix, start) = match example.and_then(|e| e.start.map(|s| (e, s))) {
            Some((e, start)) => (e.prefix.clone(), start),
            None if j == 0 || (i == 0 && j == self.links.leading) => {
                (segments[..i].iter().map(|s| (*s).to_owned()).collect(), j)
            }
            None => (self.base.clone(), 0),
        };
        let own = own_link || i + k < segments.len();
        let mut out = self.fill(prefix, start, &shown, (example, own))?;
        out.extend(segments[i + k..].iter().map(|s| (*s).to_owned()));
        Some(out)
    }

    /// The path of a value placed under its parent's `parent` path; `None` when it shows
    /// no identity to place.
    fn under_parent(&self, segments: &[&str], parent: &[&str]) -> Option<Vec<String>> {
        if parent.is_empty() {
            return None;
        }
        let exact = segments
            .iter()
            .rposition(|s| self.owns.iter().any(|o| o == s));
        let (at, exact) = match exact {
            Some(at) => (at, true),
            None => {
                let last = segments.len().checked_sub(1)?;
                let digits = segments[last].bytes().all(|b| b.is_ascii_digit());
                if !digits {
                    return None;
                }
                (last, false)
            }
        };
        if at == 0 {
            return None;
        }
        let collection = segments[at - 1];
        let mut out: Vec<String> = parent.iter().map(|s| (*s).to_owned()).collect();
        let names_parent =
            parent.len() >= 2 && fits(collection, parent[parent.len() - 2]) && !exact;
        if !names_parent {
            let own = match exact {
                true => segments[at].to_owned(),
                false => self
                    .owns
                    .iter()
                    .find(|o| !o.is_empty() && o.bytes().all(|b| b.is_ascii_digit()))
                    .cloned()
                    .unwrap_or_else(|| segments[at].to_owned()),
            };
            out.push(collection.to_owned());
            out.push(own);
        }
        out.extend(segments[at + 1..].iter().map(|s| (*s).to_owned()));
        Some(out)
    }

    /// The URL the first parent holds in member `name`.
    fn parent_url(&self, name: &str) -> Option<Url<'_>> {
        self.parents.iter().find_map(|p| {
            let url = Url::parse(p.data.get(name)?.as_str()?)?;
            (!url.segments.is_empty()).then_some(url)
        })
    }
}

/// The records `call`'s required id arguments name (`input.repositoryId`), as the
/// session holds them: what the record of a create is placed under.
async fn parents(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    op: &Operation,
    call: &OperationCall,
) -> Vec<StoredResource> {
    fn required(
        rt: &MockRuntime,
        schema: &Value,
        body: &Map<String, Value>,
        depth: usize,
        out: &mut Vec<Reference>,
    ) {
        if depth > 2 {
            return;
        }
        let bundle = rt.bundle();
        for name in required_members(bundle, schema) {
            match body.get(&name) {
                Some(Value::Object(inner)) => {
                    if let Some(inner_schema) = property_schema(bundle, schema, &name) {
                        required(rt, &inner_schema, inner, depth + 1, out);
                    }
                }
                Some(value @ (Value::String(_) | Value::Number(_))) => {
                    references_in(&name, value, 0, Some(rt.index()), out);
                }
                _ => {}
            }
        }
    }
    let (Some(Value::Object(body)), Some(schema)) = (&call.body, op.input.body.as_ref()) else {
        return Vec::new();
    };
    let rt = state.rt;
    let mut references = Vec::new();
    required(rt, schema, body, 0, &mut references);
    let mut out = Vec::new();
    for (types, id) in references.into_iter().take(4) {
        for t in types {
            if let Some(held) = state.get(session, &t, &id).await {
                out.push(held);
                break;
            }
        }
    }
    out
}

/// The members `call`'s body sends: its top-level names and those one object down
/// (GraphQL's `input: {...}`). What the client sent of a record is its own, and stays.
pub(super) fn sent_members(call: &OperationCall) -> Vec<String> {
    let Some(Value::Object(body)) = &call.body else {
        return Vec::new();
    };
    let inner = body
        .values()
        .filter_map(Value::as_object)
        .flat_map(|o| o.keys());
    body.keys().chain(inner).cloned().collect()
}

/// Settles the URL members of `record`, a record of type `t` that `call` (of `op`) stores,
/// as the module says, but those named in `kept` (what the request sent or the
/// description sets).
pub(super) async fn settle(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    (op, call): (&Operation, &OperationCall),
    (t, kept): (&str, &[String]),
    record: &mut Value,
) {
    let rt = state.rt;
    let links = type_links(rt, t);
    let Value::Object(members) = &*record else {
        return;
    };
    let urls: Vec<String> = members
        .iter()
        .filter(|(name, value)| {
            value.is_string() && links.is_url_member(name) && !kept.contains(*name)
        })
        .map(|(name, _)| name.clone())
        .collect();
    if urls.is_empty() {
        return;
    }
    let held = match links.read.is_empty() {
        true => parents(state, session, op, call).await,
        false => Vec::new(),
    };
    let own = links
        .own
        .as_deref()
        .and_then(|m| mb_mcm::member_at(record, m))
        .and_then(id_string);
    let mut owns: Vec<String> = own.iter().cloned().collect();
    let index = rt.index();
    let identity = index.identity_fields(t).iter().map(String::as_str);
    for member in identity.chain(["number", "id"]) {
        if let Some(value) = mb_mcm::member_at(record, member).and_then(id_string)
            && !owns.contains(&value)
        {
            owns.push(value);
        }
    }
    let mut accepted: Vec<String> = Vec::new();
    let parent_hosts = held
        .iter()
        .flat_map(|p| p.data.as_object())
        .flat_map(|data| {
            data.values()
                .filter_map(Value::as_str)
                .filter_map(Url::parse)
                .filter_map(|u| u.origin.map(host_of))
                .collect::<Vec<_>>()
        });
    let family = rt
        .hosts
        .iter()
        .filter_map(|h| h.strip_prefix("api."))
        .map(str::to_owned);
    for host in rt
        .hosts
        .iter()
        .cloned()
        .chain(family)
        .chain(links.hosts.iter().cloned())
        .chain(parent_hosts)
    {
        if !placeholder(&host) && !accepted.contains(&host) {
            accepted.push(host);
        }
    }
    let known = Known {
        links: &links,
        params: read_params(&links.read, op, call),
        own,
        owns,
        base: rt
            .base_paths
            .first()
            .map(|b| {
                b.split('/')
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default(),
        host: rt
            .hosts
            .iter()
            .map(String::as_str)
            .find(|h| !placeholder(h)),
        accepted,
        parents: &held,
    };
    let Value::Object(members) = record else {
        return;
    };
    for name in urls {
        let Some(Value::String(text)) = members.get(&name) else {
            continue;
        };
        if let Some(settled) = known.settle(&name, text) {
            members.insert(name, Value::String(settled));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn template(path: &str) -> Vec<String> {
        path.split('/')
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn placeholders_are_the_reserved_example_hosts() {
        for host in [
            "example.com",
            "api.example.org",
            "example.invalid",
            "x.example",
        ] {
            assert!(placeholder(host), "{host}");
        }
        for host in [
            "tracker.test",
            "notexample.com",
            "example.co",
            "examples.com",
        ] {
            assert!(!placeholder(host), "{host}");
        }
    }

    #[test]
    fn urls_parse_and_render_unchanged() {
        for text in [
            "https://api.tracker.test/v1/projects/a/tickets/7",
            "https://tracker.test/a/tickets/7#comment-3",
            "https://tracker.test/a/tickets/?q=1",
            "https://tracker.test/",
            "https://tracker.test",
            "/a/tickets/7",
        ] {
            let url = Url::parse(text).unwrap();
            let segments: Vec<String> = url.segments.iter().map(|s| (*s).to_owned()).collect();
            assert_eq!(url.render(url.origin, &segments, url.slash), text);
        }
        for text in ["", "tickets/7", "//host/x", "https://", "has space"] {
            assert!(Url::parse(text).is_none(), "{text}");
        }
        assert_eq!(
            host_of("https://u@API.Tracker.test:8443"),
            "api.tracker.test"
        );
    }

    #[test]
    fn a_value_aligns_with_the_read_path_it_shows() {
        let read = template("/repos/{owner}/{repo}/pulls/{number}");
        // The whole read path.
        assert_eq!(
            align(&["repos", "a", "r", "pulls", "5"], &read),
            Some((0, 0, 5))
        );
        // A link leaving the leading literal out, naming the collection in the singular.
        assert_eq!(align(&["a", "r", "pull", "5"], &read), Some((0, 1, 4)));
        // Only the collection and the number.
        assert_eq!(align(&["pulls", "5"], &read), Some((0, 3, 2)));
        // The parent's link.
        assert_eq!(align(&["repos", "a", "r"], &read), Some((0, 0, 3)));
        // Nothing of it.
        assert_eq!(align(&["u", "1"], &read), None);
    }

    /// What a call under `/repos/a/r/…` knows of record 7 of a type read at
    /// `/repos/{owner}/{repo}/pulls/{number}`, under host `api.tracker.test`, with an
    /// `html_url` example under `tracker.test`.
    fn known_of(links: &TypeLinks) -> Known<'_> {
        Known {
            links,
            params: HashMap::from([
                ("owner".to_owned(), "a".to_owned()),
                ("repo".to_owned(), "r".to_owned()),
            ]),
            own: Some("7".to_owned()),
            owns: vec!["7".to_owned()],
            base: Vec::new(),
            host: Some("api.tracker.test"),
            accepted: vec!["api.tracker.test".to_owned(), "tracker.test".to_owned()],
            parents: &[],
        }
    }

    fn pulls() -> TypeLinks {
        let mut links = TypeLinks {
            read: template("/repos/{owner}/{repo}/pulls/{number}"),
            leading: 1,
            own: Some("number".to_owned()),
            ..TypeLinks::default()
        };
        let example = links
            .example("https://tracker.test/octo/demo/pull/1")
            .unwrap();
        links.examples.insert("html_url".to_owned(), example);
        links
    }

    #[test]
    fn values_along_a_read_path_take_the_call_s_place() {
        let links = pulls();
        let known = known_of(&links);
        let settle = |name: &str, text: &str| known.settle(name, text);
        // The own link, from a collection on, under a placeholder host.
        assert_eq!(
            settle("url", "https://example.invalid/pulls/142").as_deref(),
            Some("https://api.tracker.test/repos/a/r/pulls/7")
        );
        // Another link of the whole path takes the call's parameters and keeps the
        // record it names; a placeholder host moves.
        assert_eq!(
            settle("diff_url", "https://example.com/x/y/pull/9.diff").as_deref(),
            Some("https://api.tracker.test/a/r/pull/9.diff")
        );
        // The page link as the example lays it out, a suffix of its last segment kept.
        assert_eq!(
            settle("html_url", "https://github.invalid/x/y/pull/9.diff").as_deref(),
            Some("https://tracker.test/a/r/pull/7.diff")
        );
        // A word the faker drew for the own link is built.
        assert_eq!(
            settle("html_url", "sable").as_deref(),
            Some("https://tracker.test/a/r/pull/7")
        );
        // Another link of the whole path keeps the record it names, and its real host.
        assert_eq!(
            settle("duplicate_of_url", "https://mirror.test/repos/x/y/pulls/3").as_deref(),
            Some("https://mirror.test/repos/a/r/pulls/3")
        );
        // One going on past it is the record's.
        assert_eq!(
            settle(
                "comments_url",
                "https://api.tracker.test/repos/x/y/pulls/3/comments"
            )
            .as_deref(),
            Some("https://api.tracker.test/repos/a/r/pulls/7/comments")
        );
        // A link to what holds the record: only the parameters it shows.
        assert_eq!(
            settle("repository_url", "https://api.tracker.test/repos/x/y").as_deref(),
            Some("https://api.tracker.test/repos/a/r")
        );
        // A link to anything else keeps its real host, a placeholder moves.
        assert_eq!(settle("avatar_url", "https://cdn.test/u/1"), None);
        assert_eq!(
            settle("avatar_url", "https://example.com/u/1").as_deref(),
            Some("https://api.tracker.test/u/1")
        );
        // Already right.
        assert_eq!(
            settle("url", "https://api.tracker.test/repos/a/r/pulls/7"),
            None
        );
    }

    #[test]
    fn with_no_host_known_hosts_stay() {
        let links = TypeLinks {
            read: template("/repos/{owner}/{repo}/pulls/{number}"),
            leading: 1,
            own: Some("number".to_owned()),
            ..TypeLinks::default()
        };
        let known = Known {
            host: None,
            accepted: Vec::new(),
            ..known_of(&links)
        };
        assert_eq!(
            known
                .settle("url", "https://example.invalid/pulls/142")
                .as_deref(),
            Some("https://example.invalid/repos/a/r/pulls/7")
        );
        assert_eq!(known.settle("avatar_url", "https://example.com/u/1"), None);
        // No URL and no host to build one under.
        assert_eq!(known.settle("url", "sable"), None);
    }

    #[test]
    fn values_with_no_read_path_go_under_their_parent() {
        let links = TypeLinks::default();
        let parents = [StoredResource {
            resource_type: "issue".into(),
            id: "I_1".into(),
            data: serde_json::json!({"url": "https://tracker.test/acme/roadmap/issues/7"}),
        }];
        let known = Known {
            params: HashMap::new(),
            own: None,
            owns: vec!["C_1".to_owned(), "42".to_owned()],
            parents: &parents,
            ..known_of(&links)
        };
        // A comment's link pointing at its issue's place is the issue's URL.
        assert_eq!(
            known
                .settle("url", "https://example.com/nectar/issues/9#comment-42")
                .as_deref(),
            Some("https://tracker.test/acme/roadmap/issues/7#comment-42")
        );
        // Its own place goes under the parent's.
        assert_eq!(
            known
                .settle("url", "https://example.invalid/comments/42")
                .as_deref(),
            Some("https://tracker.test/acme/roadmap/issues/7/comments/42")
        );
        // A member the parent holds no URL in only has its placeholder host replaced.
        assert_eq!(
            known
                .settle("href", "https://example.invalid/comments/42")
                .as_deref(),
            Some("https://api.tracker.test/comments/42")
        );
    }

    #[test]
    fn url_names() {
        for name in [
            "url",
            "html_url",
            "htmlUrl",
            "href",
            "resourcePath",
            "comments_url",
        ] {
            assert!(url_named(name), "{name}");
        }
        for name in ["_url", "Url", "curl", "urls", "title"] {
            assert!(!url_named(name), "{name}");
        }
    }
}
