//! Formats a persona declares, kept after generation.
//!
//! Models follow a persona's formats most of the time: in the Stripe trial a
//! few `client_secret` suffixes were shorter than the 24 characters asked
//! for. Two phrasings are read from the persona and checked on every
//! generated object, like disallowed nulls are patched:
//!
//! - a template, `client_secret = "<id>_secret_<24 random chars>"`: literal
//!   text, `<field>` (a sibling string member) and `<N [random]
//!   [alphanumeric|hex|digit|lowercase] chars>`;
//! - id prefixes, `Ids use Stripe prefixes (pi_, cus_, pm_) followed by 24
//!   random alphanumeric chars`: an `id` starting with a listed prefix has
//!   exactly that many characters after it.
//!
//! A value that breaks its format keeps the characters it has that fit and
//! is padded with random ones or cut. Values the request or the path set are
//! left alone, and a patched body must still validate.

use rand::{Rng, RngExt};
use regex::Regex;
use serde_json::{Map, Value};

use crate::validate::SchemaValidator;

/// The character class of a random run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Class {
    Alphanumeric,
    Hex,
    Digit,
    Lowercase,
}

impl Class {
    fn parse(word: Option<&str>) -> Option<Self> {
        match word.map(str::to_ascii_lowercase).as_deref() {
            None | Some("alphanumeric" | "alnum" | "random" | "alpha-numeric") => {
                Some(Class::Alphanumeric)
            }
            Some("hex" | "hexadecimal") => Some(Class::Hex),
            Some("digit" | "digits" | "numeric") => Some(Class::Digit),
            Some("lowercase") => Some(Class::Lowercase),
            _ => None,
        }
    }

    fn contains(self, c: char) -> bool {
        match self {
            Class::Alphanumeric => c.is_ascii_alphanumeric(),
            Class::Hex => c.is_ascii_hexdigit(),
            Class::Digit => c.is_ascii_digit(),
            Class::Lowercase => c.is_ascii_lowercase() || c.is_ascii_digit(),
        }
    }

    fn draw(self, rng: &mut impl Rng) -> char {
        const ALNUM: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
        let set: &[u8] = match self {
            Class::Alphanumeric => ALNUM,
            Class::Hex => b"0123456789abcdef",
            Class::Digit => b"0123456789",
            Class::Lowercase => b"abcdefghijklmnopqrstuvwxyz0123456789",
        };
        char::from(set[rng.random_range(0..set.len())])
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Part {
    Literal(String),
    Field(String),
    Random(usize, Class),
}

#[derive(Clone, Debug)]
struct Template {
    field: String,
    parts: Vec<Part>,
}

#[derive(Clone, Debug)]
struct Prefixed {
    prefixes: Vec<String>,
    len: usize,
    class: Class,
}

/// The formats one persona declares.
#[derive(Clone, Debug, Default)]
pub struct PersonaFormats {
    templates: Vec<Template>,
    ids: Vec<Prefixed>,
    /// Bare id templates (`gid://shopify/Cart/<26 chars>?key=<32 hex>`),
    /// kept on ids that start with their leading literal.
    id_templates: Vec<Template>,
}

/// `<N [random] [class] chars>`, or `<N class>` (`<32 hex>`).
fn random_part(text: &str) -> Option<Part> {
    let re = Regex::new(
        r"(?i)^(\d{1,3})(?:[\s-]+random)?(?:[\s-]+([a-z-]+))?(?:[\s-]+(chars?|characters?))?$",
    )
    .ok()?;
    let caps = re.captures(text.trim())?;
    let n = caps.get(1)?.as_str().parse().ok()?;
    let word = caps.get(2).map(|m| m.as_str());
    let unit = word.is_some_and(|w| {
        ["char", "chars", "character", "characters"].contains(&w.to_ascii_lowercase().as_str())
    });
    if caps.get(3).is_none() && (word.is_none() || unit) {
        return (unit).then_some(Part::Random(n, Class::Alphanumeric));
    }
    let class = Class::parse(word.filter(|_| !unit))?;
    Some(Part::Random(n, class))
}

fn template(field: &str, text: &str) -> Option<Template> {
    let mut parts = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find('<') {
        let close = open + rest[open..].find('>')?;
        if open > 0 {
            parts.push(Part::Literal(rest[..open].to_string()));
        }
        let inner = &rest[open + 1..close];
        let part =
            if inner.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') && !inner.is_empty() {
                Part::Field(inner.to_string())
            } else {
                random_part(inner)?
            };
        parts.push(part);
        rest = &rest[close + 1..];
    }
    if !rest.is_empty() {
        parts.push(Part::Literal(rest.to_string()));
    }
    parts
        .iter()
        .any(|p| matches!(p, Part::Random(..)))
        .then(|| Template {
            field: field.to_string(),
            parts,
        })
}

impl PersonaFormats {
    /// The formats `persona` declares; empty when it declares none.
    #[must_use]
    pub fn parse(persona: &str) -> Self {
        let mut formats = PersonaFormats::default();
        if let Ok(re) =
            Regex::new(r#"([A-Za-z_][A-Za-z0-9_]*)\s*(?:=|:|is)\s*["'`]([^"'`]*<[^"'`]+)["'`]"#)
        {
            for caps in re.captures_iter(persona) {
                if let (Some(field), Some(text)) = (caps.get(1), caps.get(2))
                    && let Some(t) = template(field.as_str(), text.as_str())
                {
                    formats.templates.push(t);
                }
            }
        }
        if let Ok(re) = Regex::new(
            r"(?i)\bids?\b[^.()]*\(([^)]*)\)\s*(?:followed by|then|\+)\s*(\d{1,3})(?:\s+random)?(?:\s+([a-z-]+))?\s+(?:chars?|characters?)",
        ) {
            for caps in re.captures_iter(persona) {
                let prefixes: Vec<String> = caps
                    .get(1)
                    .map_or("", |m| m.as_str())
                    .split([',', ' ', '/'])
                    .map(|p| p.trim().trim_matches(|c| c == '`' || c == '"' || c == '\''))
                    .filter(|p| {
                        !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                    })
                    .map(str::to_string)
                    .collect();
                let len = caps.get(2).and_then(|m| m.as_str().parse().ok());
                let class = Class::parse(caps.get(3).map(|m| m.as_str()));
                if let (false, Some(len), Some(class)) = (prefixes.is_empty(), len, class) {
                    formats.ids.push(Prefixed {
                        prefixes,
                        len,
                        class,
                    });
                }
            }
        }
        // Bare templates outside quotes: `gid://shopify/Cart/<26 chars>`.
        if let Ok(re) =
            Regex::new(r#"(?:[^\s<>"'`]|<[^<>"'`\n]+>)*<[^<>"'`\n]+>(?:[^\s<>"'`]|<[^<>"'`\n]+>)*"#)
        {
            for m in re.find_iter(persona) {
                let text = m.as_str().trim_end_matches(['.', ',', ';', ':', ')']);
                let Some(t) = template("id", text) else {
                    continue;
                };
                let lead = matches!(t.parts.first(), Some(Part::Literal(l)) if l.len() >= 3);
                if lead && !formats.templates.iter().any(|q| q.parts == t.parts) {
                    formats.id_templates.push(t);
                }
            }
        }
        formats
    }

    /// Whether the persona declares no format.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.templates.is_empty() && self.ids.is_empty() && self.id_templates.is_empty()
    }

    /// `body` with every object's declared formats kept; values equal to
    /// one in `protected` (what the request or path set) are not touched.
    /// `None` when nothing changed or the patched body does not validate.
    pub fn apply(
        &self,
        validator: &SchemaValidator,
        body: &Value,
        protected: &[&Value],
    ) -> Option<Value> {
        if self.is_empty() {
            return None;
        }
        let mut out = body.clone();
        let mut rng = rand::rng();
        if !self.walk(&mut out, protected, &mut rng) {
            return None;
        }
        validator.is_valid(&out).then_some(out)
    }

    fn walk(&self, value: &mut Value, protected: &[&Value], rng: &mut impl Rng) -> bool {
        match value {
            Value::Object(obj) => {
                let mut changed = self.fix_object(obj, protected, rng);
                for v in obj.values_mut() {
                    changed |= self.walk(v, protected, rng);
                }
                changed
            }
            Value::Array(items) => {
                let mut changed = false;
                for v in items {
                    changed |= self.walk(v, protected, rng);
                }
                changed
            }
            _ => false,
        }
    }

    fn fix_object(
        &self,
        obj: &mut Map<String, Value>,
        protected: &[&Value],
        rng: &mut impl Rng,
    ) -> bool {
        let mut changed = false;
        let free = |v: &Value| !protected.contains(&v);
        if let Some(id @ Value::String(_)) = obj.get("id")
            && free(id)
            && let Some(fixed) = self.fix_id(id.as_str().unwrap_or_default(), rng)
        {
            obj.insert("id".into(), Value::String(fixed));
            changed = true;
        }
        let id_keys: Vec<String> = obj
            .iter()
            .filter(|(k, v)| {
                (k.as_str() == "id" || k.ends_with("Id") || k.ends_with("_id"))
                    && v.is_string()
                    && free(v)
            })
            .map(|(k, _)| k.clone())
            .collect();
        for key in id_keys {
            let current = obj[&key].as_str().unwrap_or_default().to_string();
            let fixed = self.id_templates.iter().find_map(|t| {
                let Some(Part::Literal(lead)) = t.parts.first() else {
                    return None;
                };
                current
                    .starts_with(lead.as_str())
                    .then(|| t.fix(&current, obj, rng))?
            });
            if let Some(fixed) = fixed {
                obj.insert(key, Value::String(fixed));
                changed = true;
            }
        }
        for t in &self.templates {
            let Some(current @ Value::String(_)) = obj.get(&t.field) else {
                continue;
            };
            if !free(current) {
                continue;
            }
            let current = current.as_str().unwrap_or_default().to_string();
            if let Some(fixed) = t.fix(&current, obj, rng) {
                obj.insert(t.field.clone(), Value::String(fixed));
                changed = true;
            }
        }
        changed
    }

    fn fix_id(&self, id: &str, rng: &mut impl Rng) -> Option<String> {
        for rule in &self.ids {
            let Some(prefix) = rule
                .prefixes
                .iter()
                .filter(|p| id.starts_with(p.as_str()))
                .max_by_key(|p| p.len())
            else {
                continue;
            };
            let tail = &id[prefix.len()..];
            if tail.chars().count() == rule.len && tail.chars().all(|c| rule.class.contains(c)) {
                return None;
            }
            return Some(format!("{prefix}{}", run(tail, rule.len, rule.class, rng)));
        }
        None
    }
}

impl Template {
    /// Whether `current` is exactly the template for `obj`: each literal
    /// and field value in turn, and exactly `n` characters of the class for
    /// each random run (what the anchored regex `^lit[class]{n}...$` would
    /// match, without compiling one per object). `None` when a field part
    /// is not a string member of `obj`.
    fn matches(&self, current: &str, obj: &Map<String, Value>) -> Option<bool> {
        let mut fits = true;
        let mut rest = current;
        for part in &self.parts {
            let next = match part {
                Part::Literal(l) => rest.strip_prefix(l.as_str()),
                Part::Field(f) => rest.strip_prefix(obj.get(f)?.as_str()?),
                Part::Random(n, class) => {
                    let mut chars = rest.char_indices();
                    let run = chars
                        .by_ref()
                        .take(*n)
                        .take_while(|(_, c)| class.contains(*c));
                    (run.count() == *n).then(|| chars.next().map_or("", |(at, _)| &rest[at..]))
                }
            };
            // Keep going after a mismatch: a missing field still answers None.
            match next {
                Some(next) if fits => rest = next,
                _ => fits = false,
            }
        }
        Some(fits && rest.is_empty())
    }

    /// The value of the template for `obj`, keeping what fits of `current`;
    /// `None` when `current` already matches (or a field part is missing).
    fn fix(&self, current: &str, obj: &Map<String, Value>, rng: &mut impl Rng) -> Option<String> {
        if self.matches(current, obj)? {
            return None;
        }
        let mut out = String::new();
        for (i, part) in self.parts.iter().enumerate() {
            match part {
                Part::Literal(l) => out.push_str(l),
                Part::Field(f) => out.push_str(obj.get(f)?.as_str()?),
                Part::Random(n, class) => {
                    // The run the model wrote: what follows the literal
                    // before it (or the start).
                    let after = match i.checked_sub(1).map(|j| &self.parts[j]) {
                        Some(Part::Literal(l)) => {
                            current.rfind(l.as_str()).map(|at| &current[at + l.len()..])
                        }
                        None => Some(current),
                        _ => None,
                    };
                    out.push_str(&run(after.unwrap_or(""), *n, *class, rng));
                }
            }
        }
        Some(out)
    }
}

/// `n` characters of `class`: the leading ones of `from` that fit, then
/// random ones.
fn run(from: &str, n: usize, class: Class, rng: &mut impl Rng) -> String {
    let mut out: String = from
        .chars()
        .take_while(|c| class.contains(*c))
        .take(n)
        .collect();
    while out.chars().count() < n {
        out.push(class.draw(rng));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const STRIPE: &str = "Stripe API in test mode. Ids use Stripe prefixes (pi_, cus_, pm_) followed by 24 random alphanumeric chars, unique per object. A PaymentIntent always has client_secret = \"<id>_secret_<24 random chars>\", livemode false.";

    fn any() -> SchemaValidator {
        SchemaValidator::new(&json!({})).unwrap()
    }

    #[test]
    fn the_stripe_persona_is_read() {
        let f = PersonaFormats::parse(STRIPE);
        assert_eq!(f.templates.len(), 1, "{f:?}");
        assert_eq!(f.templates[0].field, "client_secret");
        assert_eq!(f.ids[0].prefixes, ["pi_", "cus_", "pm_"]);
        assert_eq!(f.ids[0].len, 24);
        assert!(PersonaFormats::parse("Realistic GitHub data.").is_empty());
    }

    #[test]
    fn short_secrets_and_ids_are_padded_keeping_what_fits() {
        let f = PersonaFormats::parse(STRIPE);
        let body = json!({"id": "pi_3QxYzAbC", "client_secret": "pi_3QxYzAbC_secret_short", "nested": {"id": "cus_ok"}});
        let out = f.apply(&any(), &body, &[]).unwrap();
        let id = out["id"].as_str().unwrap();
        assert!(id.starts_with("pi_3QxYzAbC") && id.len() == 27, "{id}");
        let secret = out["client_secret"].as_str().unwrap();
        let re = Regex::new(r"^pi_[A-Za-z0-9]{24}_secret_[A-Za-z0-9]{24}$").unwrap();
        assert!(re.is_match(secret), "{secret}");
        assert!(
            secret.starts_with(&format!("{id}_secret_short")),
            "{secret}"
        );
        assert_eq!(out["nested"]["id"].as_str().unwrap().len(), 4 + 24);
    }

    #[test]
    fn conforming_and_protected_values_are_kept() {
        let f = PersonaFormats::parse(STRIPE);
        let id = format!("pi_{}", "A".repeat(24));
        let body = json!({"id": id, "client_secret": format!("{id}_secret_{}", "b".repeat(24))});
        assert!(f.apply(&any(), &body, &[]).is_none());
        let path_id = json!("pi_fromthepath");
        let body = json!({"id": "pi_fromthepath", "client_secret": "x"});
        let out = f.apply(&any(), &body, &[&path_id]).unwrap();
        assert_eq!(out["id"], "pi_fromthepath");
        assert!(
            out["client_secret"]
                .as_str()
                .unwrap()
                .starts_with("pi_fromthepath_secret_")
        );
    }

    const SHOPIFY: &str = "Global ids look like gid://shopify/Product/8123456789012 (13 digits), gid://shopify/Collection/..., gid://shopify/Cart/<26 chars>?key=<32 hex>. Images at https://cdn.shopify.com/files/<handle>-<n>.png.";

    #[test]
    fn id_templates_with_a_query_part_are_kept() {
        let f = PersonaFormats::parse(SHOPIFY);
        assert_eq!(f.id_templates.len(), 1, "{f:?}");
        let body = json!({"cart": {"id": "gid://shopify/Cart/8f3a1c6d9b2e4f70a5c8d13e6b", "lines": []},
                          "product": {"id": "gid://shopify/Product/8123456789012"}});
        let out = f.apply(&any(), &body, &[]).unwrap();
        let id = out["cart"]["id"].as_str().unwrap();
        let re = Regex::new(r"^gid://shopify/Cart/8f3a1c6d9b2e4f70a5c8d13e6b\?key=[0-9a-f]{32}$")
            .unwrap();
        assert!(re.is_match(id), "{id}");
        assert_eq!(out["product"], body["product"]);
        let kept = json!({"cartId": id});
        assert!(f.apply(&any(), &kept, &[]).is_none());
    }

    /// The anchored regex `Template::fix` compiled before it matched by
    /// hand: the reference `Template::matches` must agree with.
    fn reference(t: &Template, obj: &Map<String, Value>) -> Option<Regex> {
        let mut pattern = String::from("^");
        for part in &t.parts {
            match part {
                Part::Literal(l) => pattern.push_str(&regex::escape(l)),
                Part::Field(f) => pattern.push_str(&regex::escape(obj.get(f)?.as_str()?)),
                Part::Random(n, class) => {
                    let set = match class {
                        Class::Alphanumeric => "[A-Za-z0-9]",
                        Class::Hex => "[0-9A-Fa-f]",
                        Class::Digit => "[0-9]",
                        Class::Lowercase => "[a-z0-9]",
                    };
                    pattern.push_str(&format!("{set}{{{n}}}"));
                }
            }
        }
        pattern.push('$');
        Regex::new(&pattern).ok()
    }

    #[test]
    fn templates_match_like_the_anchored_regex() {
        let t = Template {
            field: "secret".into(),
            parts: vec![
                Part::Literal("pi_".into()),
                Part::Random(4, Class::Alphanumeric),
                Part::Literal("_s_".into()),
                Part::Field("id".into()),
                Part::Random(2, Class::Hex),
                Part::Random(0, Class::Digit),
                Part::Random(1, Class::Lowercase),
            ],
        };
        let obj = json!({"id": "a.b"}).as_object().unwrap().clone();
        let re = reference(&t, &obj).unwrap();
        let candidates = [
            "pi_Ab12_s_a.bfFz",
            "pi_Ab12_s_a.bfF9",
            "pi_Ab12_s_a.bfFZ",
            "pi_Ab12_s_a.bfFzz",
            "pi_Ab1_s_a.bfFz",
            "pi_Ab12_s_axbfFz",
            "pi_Ab12_s_a.bgFz",
            "pi_Ab1é_s_a.bfFz",
            "pi_Ab12_s_a.bfF",
            "xpi_Ab12_s_a.bfFz",
            "pi_Ab12_s_a.bfFz\n",
            "",
            "pi_",
        ];
        for c in candidates {
            assert_eq!(t.matches(c, &obj), Some(re.is_match(c)), "{c:?}");
        }
        // A field part that is not a string member answers None, as the
        // regex could not be built.
        let none = Map::new();
        assert!(reference(&t, &none).is_none());
        assert_eq!(t.matches("pi_Ab12_s_a.bfFz", &none), None);
        assert_eq!(t.matches("nope", &none), None);
    }

    #[test]
    fn a_patch_that_breaks_the_schema_is_dropped() {
        let f = PersonaFormats::parse(STRIPE);
        let v = SchemaValidator::new(&json!({"properties": {"client_secret": {"maxLength": 10}}}))
            .unwrap();
        assert!(
            f.apply(&v, &json!({"id": "x", "client_secret": "y"}), &[])
                .is_none()
        );
    }
}
