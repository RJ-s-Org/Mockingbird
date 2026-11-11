//! Records another mock of the same API wrote, for mocks that `shares` one
//! name (GitHub's REST beside its GraphQL): they keep their sessions under
//! it, so each finds what the others stored. A record is the same type
//! when the names agree beyond case, punctuation and a plural `s` (`issues`,
//! `issue`), or when the world says so (`types.<t>.same_as`). It is answered
//! in the reading mock's shape: members renamed between camelCase and
//! snake_case where only that differs, enum values in the reading schema's case.

use super::*;

/// Records of one equivalent type read, at most.
const MAX_SHARED: usize = 200;

/// The type names a record of type `t` may be stored under by the mock this
/// one shares; none when it shares with no other.
pub(crate) fn equivalents(rt: &MockRuntime, t: &str) -> Vec<String> {
    if rt.shares.is_none() {
        return Vec::new();
    }
    let base: String = t
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect::<String>()
        .to_lowercase();
    let mut names = vec![base.clone(), format!("{base}s")];
    names.extend(base.strip_suffix("es").map(str::to_string));
    names.extend(base.strip_suffix('s').map(str::to_string));
    if let Some(desc) = rt.described().type_desc(t) {
        names.extend(desc.same_as.iter().map(|n| n.to_lowercase()));
    }
    let mut out: Vec<String> = Vec::new();
    for name in names {
        if !name.is_empty() && name != t && !out.contains(&name) {
            out.push(name);
        }
    }
    out
}

/// The record of a type equivalent to `t` that `id` names, by its key or by
/// the member `op` reads it by, drawn into `op`'s answer shape as a record of
/// `t` under `id`.
pub(super) async fn by_shared(
    state: &mut StateAccess<'_>,
    session: &SessionKey,
    (op, t): (&Operation, &str),
    id: &str,
) -> Option<StoredResource> {
    let rt = state.rt;
    let mut members: Vec<String> = Vec::new();
    if let Some(identity) = &op.identity {
        members.push(identity.path_member().to_string());
        members.push(identity.field.clone());
    }
    let members: Vec<String> = members
        .iter()
        .flat_map(|m| [m.clone(), snake(m), camel(m)])
        .collect();
    let props = op
        .success_output()
        .and_then(|o| o.schema.as_ref())
        .and_then(|s| record_props(&rt.bundle, s));
    for other in equivalents(rt, t) {
        for record in state.list(session, &other, MAX_SHARED).await {
            let named = record.id == id
                || members.iter().any(|m| {
                    mb_mcm::member_at(&record.data, m)
                        .and_then(id_string)
                        .is_some_and(|v| v == id)
                });
            if named {
                let data = match props {
                    Some(props) => reshape(props, &rt.bundle.defs, &record.data),
                    None => record.data.clone(),
                };
                return Some(StoredResource {
                    resource_type: t.to_string(),
                    id: id.to_string(),
                    data,
                });
            }
        }
    }
    None
}

/// The properties of the object `schema` is (through a `$ref` and a nullable
/// `anyOf`).
pub(crate) fn record_props<'a>(
    bundle: &'a mb_mcm::Bundle,
    schema: &'a Value,
) -> Option<&'a Map<String, Value>> {
    let mut schema = schema;
    for _ in 0..4 {
        if let Some(props) = schema.get("properties").and_then(Value::as_object) {
            return Some(props);
        }
        if let Some(name) = schema
            .get("$ref")
            .and_then(Value::as_str)
            .and_then(|r| r.strip_prefix("#/$defs/"))
        {
            schema = bundle.defs.get(name)?;
            continue;
        }
        schema = schema
            .get("anyOf")
            .and_then(Value::as_array)?
            .iter()
            .find(|b| b.get("type").and_then(Value::as_str) != Some("null"))?;
    }
    None
}

/// `data`'s members `props` declares, under the name `props` uses (as is, in
/// snake_case or in camelCase), each enum value in the case `props` lists it.
pub(crate) fn reshape(
    props: &Map<String, Value>,
    defs: &Map<String, Value>,
    data: &Value,
) -> Value {
    let mut out = Map::new();
    for (key, value) in data.as_object().into_iter().flatten() {
        let Some((name, schema)) = [key.clone(), snake(key), camel(key)]
            .into_iter()
            .find_map(|n| props.get(&n).map(|s| (n, s)))
        else {
            continue;
        };
        out.insert(name, fit_enum(schema, defs, value));
    }
    Value::Object(out)
}

/// `value` as the enum `schema` lists spells it, when only its case differs.
fn fit_enum(schema: &Value, defs: &Map<String, Value>, value: &Value) -> Value {
    let Value::String(s) = value else {
        return value.clone();
    };
    let mut schema = schema;
    for _ in 0..4 {
        if let Some(values) = schema.get("enum").and_then(Value::as_array) {
            return values
                .iter()
                .filter_map(Value::as_str)
                .find(|v| *v == s.as_str() || v.eq_ignore_ascii_case(s))
                .map_or_else(|| value.clone(), |v| Value::String(v.to_string()));
        }
        if let Some(def) = schema
            .get("$ref")
            .and_then(Value::as_str)
            .and_then(|r| r.strip_prefix("#/$defs/"))
            .and_then(|n| defs.get(n))
        {
            schema = def;
            continue;
        }
        match schema.get("anyOf").and_then(Value::as_array).and_then(|b| {
            b.iter()
                .find(|b| b.get("type").and_then(Value::as_str) != Some("null"))
        }) {
            Some(branch) => schema = branch,
            None => break,
        }
    }
    value.clone()
}

/// `createdAt` → `created_at`.
fn snake(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        if c.is_ascii_uppercase() {
            if !out.is_empty() {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// `created_at` → `createdAt`.
fn camel(name: &str) -> String {
    let mut out = String::new();
    let mut upper = false;
    for c in name.chars() {
        if c == '_' {
            upper = !out.is_empty();
        } else if upper {
            out.push(c.to_ascii_uppercase());
            upper = false;
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reshape_renames_by_case_style_and_fits_enums() {
        let props = json!({
            "number": {"type": "integer"},
            "created_at": {"type": "string"},
            "state": {"type": "string", "enum": ["open", "closed"]}
        });
        let data = json!({"number": 7, "createdAt": "2024-01-01T00:00:00Z", "state": "CLOSED", "author": {}});
        assert_eq!(
            reshape(props.as_object().unwrap(), &Map::new(), &data),
            json!({"number": 7, "created_at": "2024-01-01T00:00:00Z", "state": "closed"})
        );
    }
}
