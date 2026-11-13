//! The state an action's verb names, for an action the description gives no
//! `transition` nor `set`, when no model answers it: `closeIssue` → `state: CLOSED`,
//! `POST /orders/{id}/cancel` → `status: canceled`.
//!
//! The verb is the first word of the operation's name (its last part: `closeIssue` of
//! `Mutation.closeIssue`, `lock` of `issues/lock`), else the last literal segment of
//! its HTTP path. A value of a state member of the target's type (its lifecycle's
//! member, else a top-level member named as a state, `state` or `status`, with an
//! `enum`) is the verb's when it reads as the verb done ([`done_forms`]): `close` →
//! `closed`, `lock` → `locked`, `cancel` → `canceled` or `cancelled`, `apply` →
//! `applied`; a verb repeating a state (`reopen`, `open`) names that state.

use mb_mcm::{Binding, Operation};
use serde_json::Value;

use super::create::fresh::state_named;
use super::create::whole_parts;

/// The member of type `t`'s records and the value `op`'s verb sets it to, when one of
/// its state members' values reads as the verb done (see the module).
pub(super) fn inferred(
    rt: &crate::MockRuntime,
    op: &Operation,
    t: &str,
) -> Option<(String, Value)> {
    let bundle = rt.bundle();
    let verbs = verbs_of(op);
    if verbs.is_empty() {
        return None;
    }
    let lifecycle = rt
        .described()
        .type_desc(t)
        .and_then(|d| d.lifecycle.as_ref())
        .map(|l| l.member.clone());
    for schema in super::read_schemas(rt, t) {
        let props: Vec<(String, Value)> = whole_parts(bundle, &schema)
            .iter()
            .filter_map(|p| p.get("properties").and_then(Value::as_object))
            .flatten()
            .map(|(name, member)| (name.clone(), member.clone()))
            .collect();
        for (name, member) in &props {
            if lifecycle.as_deref() != Some(name.as_str()) && !state_named(name) {
                continue;
            }
            let values: Vec<&Value> = whole_parts(bundle, member)
                .iter()
                .filter_map(|p| p.get("enum").and_then(Value::as_array))
                .flatten()
                .filter(|v| v.is_string())
                .collect();
            for (verb, bare) in &verbs {
                let forms = done_forms(verb, *bare);
                if let Some(value) = values
                    .iter()
                    .find(|v| v.as_str().map(squeeze).is_some_and(|v| forms.contains(&v)))
                {
                    return Some((name.clone(), (*value).clone()));
                }
            }
        }
    }
    None
}

/// Words that name how a request is sent, not what it does (`PostCharges`).
const METHODS: [&str; 6] = ["get", "post", "put", "patch", "delete", "head"];

/// The verbs `op` may be named by, lower-cased, each with whether its bare form names a
/// state too: the first word of its name's last part (`closeIssue`), then the last
/// literal segment of its HTTP path (`/orders/{id}/cancel`), then the name's last word
/// (`orderCancel`), whose bare form is a noun as often as a verb (`activateTrial`'s
/// `trial`) and so names a state only done; never an HTTP method's name.
fn verbs_of(op: &Operation) -> Vec<(String, bool)> {
    let mut out: Vec<(String, bool)> = Vec::new();
    let mut add = |word: Option<String>, bare: bool| {
        if let Some(word) = word.filter(|w| !METHODS.contains(&w.as_str()))
            && !out.iter().any(|(w, _)| *w == word)
        {
            out.push((word, bare));
        }
    };
    let last = op.id.rsplit(['/', '.', ':']).next().unwrap_or(&op.id);
    let words = words_of(last);
    add(words.first().cloned(), true);
    if let Binding::Http { path, .. } = &op.binding
        && let Some(segment) = path
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .filter(|s| !s.starts_with('{') && !s.is_empty())
    {
        add(words_of(segment).first().cloned(), true);
    }
    add(words.last().cloned(), false);
    out
}

/// The words of `name` (split at camel humps, `_`, `-` and spaces), lower-cased, those
/// of three letters or more.
fn words_of(name: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    for c in name.chars() {
        if !c.is_ascii_alphabetic() || c.is_ascii_uppercase() {
            if word.len() >= 3 {
                words.push(std::mem::take(&mut word));
            }
            word.clear();
        }
        if c.is_ascii_alphabetic() {
            word.push(c.to_ascii_lowercase());
        }
    }
    if word.len() >= 3 {
        words.push(word);
    }
    words
}

/// The forms of `verb` done: its past participles (`closed`, `locked`, `canceled`,
/// `cancelled`, `applied`), and, with `bare`, the state the verb names as it is
/// (`open`) and, for a verb repeating another (`reopen`), that verb alone (`open`), of
/// four letters or more: never its past (`review` is no `viewed`, `renew` no `new`).
fn done_forms(verb: &str, bare: bool) -> Vec<String> {
    let mut out = Vec::new();
    if bare {
        out.push(verb.to_owned());
    }
    if verb.ends_with('e') {
        out.push(format!("{verb}d"));
    } else if let Some(base) = verb.strip_suffix('y') {
        out.push(format!("{base}ied"));
        out.push(format!("{verb}ed"));
    } else {
        out.push(format!("{verb}ed"));
        if let Some(last) = verb.chars().last().filter(|c| !"aeiouwxy".contains(*c)) {
            out.push(format!("{verb}{last}ed"));
        }
    }
    if let Some(rest) = verb.strip_prefix("re").filter(|r| bare && r.len() >= 4) {
        out.push(rest.to_owned());
    }
    out
}

/// `text` lower-cased with only its ASCII letters and digits.
fn squeeze(text: &str) -> String {
    text.chars()
        .filter(char::is_ascii_alphanumeric)
        .collect::<String>()
        .to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verbs_read_as_done() {
        assert!(done_forms("close", true).contains(&"closed".to_owned()));
        assert!(done_forms("lock", true).contains(&"locked".to_owned()));
        assert!(done_forms("merge", true).contains(&"merged".to_owned()));
        assert!(done_forms("archive", true).contains(&"archived".to_owned()));
        let cancel = done_forms("cancel", true);
        assert!(
            cancel.contains(&"canceled".to_owned()) && cancel.contains(&"cancelled".to_owned())
        );
        assert!(done_forms("apply", true).contains(&"applied".to_owned()));
        assert!(done_forms("reopen", true).contains(&"open".to_owned()));
    }

    #[test]
    fn a_verb_repeating_another_names_its_bare_state_only() {
        assert!(done_forms("reopen", true).contains(&"open".to_owned()));
        assert!(!done_forms("review", true).contains(&"viewed".to_owned()));
        assert!(!done_forms("refund", true).contains(&"funded".to_owned()));
        assert!(!done_forms("remove", true).contains(&"moved".to_owned()));
        assert!(!done_forms("renew", true).contains(&"new".to_owned()));
        // A name's last word names a state only done: `activateTrial` sets no `trial`.
        assert!(!done_forms("trial", false).contains(&"trial".to_owned()));
        assert!(done_forms("cancel", false).contains(&"canceled".to_owned()));
    }

    #[test]
    fn a_name_s_words() {
        assert_eq!(words_of("closeIssue"), ["close", "issue"]);
        assert_eq!(words_of("orderCancel"), ["order", "cancel"]);
        assert_eq!(words_of("cancel_order"), ["cancel", "order"]);
        assert_eq!(words_of("PostCharges"), ["post", "charges"]);
        assert!(words_of("of").is_empty());
    }
}
