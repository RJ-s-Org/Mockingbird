//! Executable directives the SDL declares (`directive @inContext(…) on
//! QUERY`, as Shopify's Storefront schema does).
//!
//! async-graphql's dynamic schema registers only the built-in directives
//! and has no API for custom ones, so a query using a declared directive
//! would fail validation with `Unknown directive`. The mock does not act on
//! them: before validation each use of a declared executable directive is
//! removed from the parsed document, together with the variables that only
//! those uses referenced (which would otherwise fail the unused-variable
//! rule). Directives the SDL does not declare, or declares only for
//! type-system locations, still fail validation.

use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::sync::Arc;

use async_graphql::dynamic::Schema;
use async_graphql::parser::Positioned;
use async_graphql::parser::types::{
    Directive, DirectiveDefinition, DirectiveLocation, DocumentOperations, ExecutableDocument,
    OperationDefinition, Selection, SelectionSet,
};
use async_graphql::{Data, Executor, Name, Request, Response, Value};
use futures::stream::BoxStream;

/// Directives async-graphql implements itself; never removed.
const BUILTIN: [&str; 5] = ["skip", "include", "deprecated", "specifiedBy", "oneOf"];

/// The name of `def` when it is a custom directive usable in a query.
pub(super) fn executable(def: &DirectiveDefinition) -> Option<Name> {
    let name = &def.name.node;
    let usable = def.locations.iter().any(|l| {
        matches!(
            l.node,
            DirectiveLocation::Query
                | DirectiveLocation::Mutation
                | DirectiveLocation::Subscription
                | DirectiveLocation::Field
                | DirectiveLocation::FragmentDefinition
                | DirectiveLocation::FragmentSpread
                | DirectiveLocation::InlineFragment
                | DirectiveLocation::VariableDefinition
        )
    });
    (usable && !BUILTIN.contains(&name.as_str())).then(|| name.clone())
}

/// The schema with the custom executable directives its SDL declares; as
/// an [`Executor`] (for subscriptions over WebSocket) it removes their uses
/// from each request before running it.
#[derive(Clone)]
pub(crate) struct Served {
    pub(super) schema: Schema,
    directives: Arc<HashSet<Name>>,
}

impl Served {
    pub(super) fn new(schema: Schema, directives: HashSet<Name>) -> Self {
        Self {
            schema,
            directives: Arc::new(directives),
        }
    }

    /// Removes every use of a declared directive from `doc`, and each
    /// variable definition only those uses referenced.
    pub(super) fn strip(&self, doc: &mut ExecutableDocument) {
        if self.directives.is_empty() {
            return;
        }
        let before = used_by_operation(doc);
        let names = &*self.directives;
        for (_, op) in operations_mut(&mut doc.operations) {
            retain(&mut op.directives, names);
            for v in &mut op.variable_definitions {
                retain(&mut v.node.directives, names);
            }
            strip_set(&mut op.selection_set.node, names);
        }
        for fragment in doc.fragments.values_mut() {
            retain(&mut fragment.node.directives, names);
            strip_set(&mut fragment.node.selection_set.node, names);
        }
        let after = used_by_operation(doc);
        let empty = HashSet::new();
        for (key, op) in operations_mut(&mut doc.operations) {
            let was = before.get(&key).unwrap_or(&empty);
            let is = after.get(&key).unwrap_or(&empty);
            op.variable_definitions.retain(|v| {
                let name = &v.node.name.node;
                is.contains(name) || !was.contains(name)
            });
        }
    }

    /// `request` with the declared directives removed; one that does not
    /// parse is left for the executor to report.
    fn prepare(&self, mut request: Request) -> Request {
        if self.directives.is_empty() {
            return request;
        }
        if let Ok(mut doc) = async_graphql::parser::parse_query(&request.query) {
            self.strip(&mut doc);
            request.set_parsed_query(doc);
        }
        request
    }
}

impl Executor for Served {
    async fn execute(&self, request: Request) -> Response {
        self.schema.execute(self.prepare(request)).await
    }

    fn execute_stream(
        &self,
        request: Request,
        session_data: Option<Arc<Data>>,
    ) -> BoxStream<'static, Response> {
        self.schema.execute_stream_with_session_data(
            self.prepare(request),
            session_data.unwrap_or_default(),
        )
    }
}

fn retain(directives: &mut Vec<Positioned<Directive>>, names: &HashSet<Name>) {
    directives.retain(|d| !names.contains(&d.node.name.node));
}

fn strip_set(set: &mut SelectionSet, names: &HashSet<Name>) {
    for item in &mut set.items {
        retain(item.node.directives_mut(), names);
        match &mut item.node {
            Selection::Field(f) => strip_set(&mut f.node.selection_set.node, names),
            Selection::InlineFragment(f) => strip_set(&mut f.node.selection_set.node, names),
            Selection::FragmentSpread(_) => {}
        }
    }
}

/// The operations of `ops`, keyed by name (`None` for a lone operation).
fn operations_mut(ops: &mut DocumentOperations) -> Vec<(Option<Name>, &mut OperationDefinition)> {
    match ops {
        DocumentOperations::Single(op) => vec![(None, &mut op.node)],
        DocumentOperations::Multiple(ops) => ops
            .iter_mut()
            .map(|(name, op)| (Some(name.clone()), &mut op.node))
            .collect(),
    }
}

/// The variables each operation references, through the fragments it
/// spreads.
fn used_by_operation(doc: &ExecutableDocument) -> HashMap<Option<Name>, HashSet<Name>> {
    doc.operations
        .iter()
        .map(|(name, op)| {
            let mut used = Uses {
                doc,
                vars: HashSet::new(),
                visited: HashSet::new(),
            };
            used.directives(&op.node.directives);
            for v in &op.node.variable_definitions {
                used.directives(&v.node.directives);
            }
            used.set(&op.node.selection_set.node);
            (name.cloned(), used.vars)
        })
        .collect()
}

/// Collects into `vars` each variable a value references, for
/// `into_const_with` (the parser's non-constant value type is not exported).
fn record(vars: &mut HashSet<Name>) -> impl FnMut(Name) -> Result<Value, Infallible> + '_ {
    |name| {
        vars.insert(name);
        Ok(Value::Null)
    }
}

struct Uses<'a> {
    doc: &'a ExecutableDocument,
    vars: HashSet<Name>,
    /// Fragments already walked.
    visited: HashSet<&'a Name>,
}

impl<'a> Uses<'a> {
    fn directives(&mut self, directives: &[Positioned<Directive>]) {
        for d in directives {
            for (_, arg) in &d.node.arguments {
                let Ok(_) = arg.node.clone().into_const_with(record(&mut self.vars));
            }
        }
    }

    fn set(&mut self, set: &'a SelectionSet) {
        for item in &set.items {
            self.directives(item.node.directives());
            match &item.node {
                Selection::Field(f) => {
                    for (_, arg) in &f.node.arguments {
                        let Ok(_) = arg.node.clone().into_const_with(record(&mut self.vars));
                    }
                    self.set(&f.node.selection_set.node);
                }
                Selection::InlineFragment(f) => self.set(&f.node.selection_set.node),
                Selection::FragmentSpread(s) => {
                    let name = &s.node.fragment_name.node;
                    if let Some((name, fragment)) = self.doc.fragments.get_key_value(name)
                        && self.visited.insert(name)
                    {
                        self.directives(&fragment.node.directives);
                        self.set(&fragment.node.selection_set.node);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn served(sdl: &str) -> Served {
        let doc = async_graphql::parser::parse_schema(sdl).unwrap();
        let names = doc
            .definitions
            .iter()
            .filter_map(|d| match d {
                async_graphql::parser::types::TypeSystemDefinition::Directive(d) => {
                    executable(&d.node)
                }
                _ => None,
            })
            .collect();
        use async_graphql::dynamic::{Field, FieldFuture, FieldValue, Object, TypeRef};
        let query =
            Object::new("Query").field(Field::new("f", TypeRef::named(TypeRef::STRING), |_| {
                FieldFuture::new(async { Ok(None::<FieldValue>) })
            }));
        let schema = Schema::build("Query", None, None).register(query);
        Served::new(schema.finish().unwrap(), names)
    }

    fn variables(doc: &ExecutableDocument, op: &str) -> Vec<String> {
        let op = doc
            .operations
            .iter()
            .find(|(n, _)| n.map(Name::as_str) == Some(op))
            .unwrap()
            .1;
        op.node
            .variable_definitions
            .iter()
            .map(|v| v.node.name.node.to_string())
            .collect()
    }

    #[test]
    fn variables_shared_with_fields_or_other_fragments_are_kept() {
        let s = served("directive @c(x: String) on QUERY | FIELD | FRAGMENT_SPREAD");
        let mut doc = async_graphql::parser::parse_query(
            r#"
            query A($a: String, $b: String, $only: String, $none: Int) @c(x: $only) {
              f(x: $a) @c(x: $b) ...F @c(x: $only)
            }
            query B($b: String) { ...F }
            fragment F on Query { g(y: $b) @c(x: "lit") }
            "#,
        )
        .unwrap();
        s.strip(&mut doc);
        // `$only` was used by removed directives only; `$none` was never
        // used and is left for validation to report; `$b` is still used
        // through fragment F.
        assert_eq!(variables(&doc, "A"), ["a", "b", "none"]);
        assert_eq!(variables(&doc, "B"), ["b"]);
    }
}
