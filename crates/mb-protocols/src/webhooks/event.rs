//! Event types (`customer.created`) and the Stripe-shaped envelope a
//! webhook carries when the contract describes no payload.

use mb_mcm::{Binding, Effect, Operation, OperationKind};
use rand::RngExt;
use serde_json::{Value, json};

/// `<singular resource>.<created|updated|deleted>` for a command; `None`
/// for anything else (reads and streams send no webhooks).
///
/// The resource is the static path segment before a trailing `{param}`
/// (`/v1/customers/{customer}` → `customer`), else the operation's resource
/// type, else its id.
pub fn event_type(op: &Operation) -> Option<String> {
    let OperationKind::Command(effect) = op.kind else {
        return None;
    };
    let verb = match effect {
        Effect::Create => "created",
        Effect::Update | Effect::Other => "updated",
        Effect::Delete => "deleted",
    };
    let resource = path_resource(op)
        .or(op.resource_type.as_deref())
        .unwrap_or(&op.id);
    Some(format!("{}.{verb}", singular(resource)))
}

/// The event type a read sends to targets that name its operation (an
/// RPC-style read with side effects, Plaid's `/transactions/sync`):
/// `<resource>.updated`.
pub fn read_event_type(op: &Operation) -> String {
    let resource = op.resource_type.as_deref().unwrap_or(&op.id);
    format!("{}.updated", singular(resource))
}

/// The static segment directly before the trailing `{param}`s of an HTTP
/// path, when the path ends in a param.
fn path_resource(op: &Operation) -> Option<&str> {
    let Binding::Http { path, .. } = &op.binding else {
        return None;
    };
    let is_param = |s: &str| s.starts_with('{') && s.ends_with('}');
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if !segments.last().is_some_and(|s| is_param(s)) {
        return None;
    }
    segments.into_iter().rev().find(|s| !is_param(s))
}

/// English singular of a plural resource name, by suffix: `ies` → `y`;
/// `sses`, `xes`, `ches`, `shes` drop `es`; else one trailing `s` goes
/// unless the word ends in `ss` or `us`.
pub fn singular(word: &str) -> String {
    if let Some(stem) = word.strip_suffix("ies") {
        return format!("{stem}y");
    }
    for suffix in ["sses", "xes", "ches", "shes"] {
        if word.ends_with(suffix) {
            return word[..word.len() - 2].to_string();
        }
    }
    if word.ends_with("ss") || word.ends_with("us") {
        return word.to_string();
    }
    word.strip_suffix('s').unwrap_or(word).to_string()
}

/// `evt_` followed by 24 random ASCII alphanumerics.
pub fn new_event_id() -> String {
    let suffix: String = rand::rng()
        .sample_iter(rand::distr::Alphanumeric)
        .take(24)
        .map(char::from)
        .collect();
    format!("evt_{suffix}")
}

/// Stripe's event shape: `{id, object: "event", type, created, data: {object}}`.
pub fn envelope(id: &str, event_type: &str, created: i64, object: Value) -> Value {
    json!({
        "id": id,
        "object": "event",
        "type": event_type,
        "created": created,
        "data": {"object": object},
    })
}

/// The envelope's `data.object`: the response body; for a delete without
/// one, `{"id": <target id>, "deleted": true}`; otherwise `{}`.
pub fn envelope_object(op: &Operation, body: Option<&Value>, target_id: Option<&str>) -> Value {
    match body {
        Some(body) => body.clone(),
        None if op.kind == OperationKind::Command(Effect::Delete) => match target_id {
            Some(id) => json!({"id": id, "deleted": true}),
            None => json!({"deleted": true}),
        },
        None => json!({}),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use mb_mcm::{HttpMethod, InputSchemas, Protocol};
    use serde_json::json;

    fn op(method: &str, path: &str, effect: Effect, resource_type: Option<&str>) -> Operation {
        let method = match method {
            "get" => HttpMethod::Get,
            "post" => HttpMethod::Post,
            "put" => HttpMethod::Put,
            "patch" => HttpMethod::Patch,
            "delete" => HttpMethod::Delete,
            other => panic!("method {other}"),
        };
        Operation {
            id: "op".into(),
            protocol: Protocol::Http,
            binding: Binding::Http {
                method,
                path: path.into(),
            },
            kind: OperationKind::Command(effect),
            summary: None,
            input: InputSchemas {
                path: None,
                query: None,
                headers: None,
                body: None,
                body_required: false,
            },
            outputs: Vec::new(),
            resource_type: resource_type.map(Into::into),
            identity: None,
            semantic_hints: Vec::new(),
            security: Vec::new(),
        }
    }

    #[test]
    fn event_types_follow_resource_and_effect() {
        assert_eq!(
            event_type(&op(
                "post",
                "/v1/customers",
                Effect::Create,
                Some("customers")
            ))
            .as_deref(),
            Some("customer.created")
        );
        assert_eq!(
            event_type(&op(
                "post",
                "/v1/payment_intents",
                Effect::Create,
                Some("payment_intents")
            ))
            .as_deref(),
            Some("payment_intent.created")
        );
        // Stripe updates are POST /x/{id}: Effect::Other.
        assert_eq!(
            event_type(&op(
                "post",
                "/v1/customers/{customer}",
                Effect::Other,
                Some("customers")
            ))
            .as_deref(),
            Some("customer.updated")
        );
        assert_eq!(
            event_type(&op(
                "put",
                "/orders/{orderId}",
                Effect::Update,
                Some("orders")
            ))
            .as_deref(),
            Some("order.updated")
        );
        assert_eq!(
            event_type(&op(
                "delete",
                "/orders/{orderId}",
                Effect::Delete,
                Some("orders")
            ))
            .as_deref(),
            Some("order.deleted")
        );
        // Action sub-path: last static segment followed by a param.
        assert_eq!(
            event_type(&op(
                "post",
                "/v1/payment_intents/{intent}/confirm",
                Effect::Create,
                Some("confirm")
            ))
            .as_deref(),
            Some("confirm.created")
        );
        // A path ending in a param names its resource, whatever the type says.
        assert_eq!(
            event_type(&op("put", "/v1/customers/{id}", Effect::Update, None)).as_deref(),
            Some("customer.updated")
        );
        // No resource type and no param: the operation id.
        assert_eq!(
            event_type(&op("post", "/run", Effect::Other, None)).as_deref(),
            Some("op.updated")
        );
    }

    #[test]
    fn queries_have_no_event_type() {
        let mut get = op("get", "/orders", Effect::Create, Some("orders"));
        get.kind = OperationKind::Query;
        assert_eq!(event_type(&get), None);
    }

    #[test]
    fn singularises() {
        for (plural, one) in [
            ("orders", "order"),
            ("categories", "category"),
            ("addresses", "address"),
            ("boxes", "box"),
            ("branches", "branch"),
            ("status", "status"),
            ("class", "class"),
            ("data", "data"),
        ] {
            assert_eq!(singular(plural), one, "{plural}");
        }
    }

    #[test]
    fn envelope_is_stripe_shaped() {
        let e = envelope("evt_1", "order.created", 1_700_000_000, json!({"id": "o1"}));
        assert_eq!(
            e,
            json!({"id": "evt_1", "object": "event", "type": "order.created",
                   "created": 1_700_000_000, "data": {"object": {"id": "o1"}}})
        );
        let id = new_event_id();
        assert!(
            id.starts_with("evt_")
                && id.len() == 28
                && id[4..].chars().all(|c| c.is_ascii_alphanumeric())
        );
    }

    #[test]
    fn bodyless_delete_object() {
        let del = op(
            "delete",
            "/orders/{orderId}",
            Effect::Delete,
            Some("orders"),
        );
        assert_eq!(
            envelope_object(&del, None, Some("o1")),
            json!({"id": "o1", "deleted": true})
        );
        let upd = op("put", "/orders/{orderId}", Effect::Update, Some("orders"));
        assert_eq!(envelope_object(&upd, None, Some("o1")), json!({}));
        assert_eq!(
            envelope_object(&upd, Some(&json!({"id": "o1"})), Some("o1")),
            json!({"id": "o1"})
        );
    }
}
