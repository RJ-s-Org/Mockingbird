//! Per-language variants of a record a read generated.
//!
//! A read whose operation declares a query parameter that selects the answer's language
//! ([`selector`], by what the contract says of it: [`super::query_roles`]) asks for the
//! same record in that
//! language. The record a read generated (or took from the pool) is stored once, in the
//! language of the read that made it, which [`Variants`] remembers. A later read of
//! that record, still as that read left it, in another language is generated again,
//! knowing the stored record (so its facts stay), and does not replace it: its record
//! is kept as that language's variant, and later reads in that language are answered
//! from it. Reads in the stored record's own language, and of a record written since,
//! go on as before. Only a hint, as [`OwnReads`] is: on another node, after a restart
//! or once emptied, a held record answers every language.

use super::*;

/// Records [`Variants`] remembers at most; it is emptied when full.
const MAX_VARIANT_RECORDS: usize = 4096;

/// Variants one record keeps at most; further languages are generated each time.
const MAX_VARIANTS: usize = 16;

/// What [`Variants`] holds of one record.
#[derive(Debug)]
pub(crate) struct Represented {
    /// The selecting parameter, as the operation declares it.
    param: String,
    /// The value the read that stored the record sent (`None`: it sent none).
    base: Option<String>,
    /// A hash of the record as that read stored it (see [`OwnReads::digest`]).
    digest: u64,
    /// The record each other value was answered with.
    kept: HashMap<Option<String>, Value>,
}

/// The language each record a read stored was answered in, and its variants.
#[derive(Debug, Default)]
pub(crate) struct Variants(std::sync::Mutex<HashMap<OwnRead, Represented>>);

/// The query parameter `op` declares that selects its answer's language, and the
/// value `call` sends for it (`None` when it sends none, or not as text).
fn selector(op: &Operation, call: &OperationCall) -> Option<(String, Option<String>)> {
    let param = super::query_roles::language_param(op)?;
    let value = call
        .query
        .get(param)
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .map(str::to_string);
    Some((param.clone(), value))
}

/// The representation a read of `op` asks for, as the prompt names it
/// (`language=fr-FR`; the API's default when the read sends none).
pub(super) fn representation(op: &Operation, call: &OperationCall) -> Option<String> {
    let (param, value) = selector(op, call)?;
    Some(match value {
        Some(value) => format!("{param}={value}"),
        None => format!("no {param}: the API's default"),
    })
}

/// What a read in another language than its record's makes of the held record.
pub(crate) enum Variant {
    /// The record as this language's earlier read answered it.
    Kept(StoredResource),
    /// None yet: the read is generated, knowing the held record.
    New,
}

impl Variants {
    /// Remembers that a read of `op` stored `r`, in the language `call` asks for.
    pub(super) fn remember(
        &self,
        session: &SessionKey,
        op: &Operation,
        call: &OperationCall,
        r: &StoredResource,
    ) {
        let Some((param, base)) = selector(op, call) else {
            return;
        };
        let Ok(mut held) = self.0.lock() else {
            return;
        };
        if held.len() >= MAX_VARIANT_RECORDS {
            held.clear();
        }
        held.insert(
            OwnReads::key(session, r),
            Represented {
                param,
                base,
                digest: OwnReads::digest(&r.data),
                kept: HashMap::new(),
            },
        );
    }

    /// The variant a read of `op` answers for `held`: `None` when it asks for no other
    /// language than the one `held` was stored in, or `held` is not as a read stored it.
    pub(super) fn of(
        &self,
        session: &SessionKey,
        op: &Operation,
        call: &OperationCall,
        held: &StoredResource,
    ) -> Option<Variant> {
        if !matches!(op.kind, OperationKind::Query) {
            return None;
        }
        let (param, value) = selector(op, call)?;
        let known = self.0.lock().ok()?;
        let represented = known.get(&OwnReads::key(session, held))?;
        if represented.param != param
            || represented.base == value
            || represented.digest != OwnReads::digest(&held.data)
        {
            return None;
        }
        Some(match represented.kept.get(&value) {
            Some(data) => Variant::Kept(StoredResource {
                data: data.clone(),
                ..held.clone()
            }),
            None => Variant::New,
        })
    }

    /// Keeps `record`, what a read [`Variants::of`] made a [`Variant::New`] of generated,
    /// as `held`'s variant for the language `call` asks for. `held` stays as it is.
    pub(super) fn keep(
        &self,
        session: &SessionKey,
        (op, call): (&Operation, &OperationCall),
        held: &StoredResource,
        record: &Value,
    ) {
        if self.of(session, op, call, held).is_none() {
            return;
        }
        let Some((_, value)) = selector(op, call) else {
            return;
        };
        let Ok(mut known) = self.0.lock() else {
            return;
        };
        if let Some(represented) = known.get_mut(&OwnReads::key(session, held))
            && (represented.kept.len() < MAX_VARIANTS || represented.kept.contains_key(&value))
        {
            represented.kept.insert(value, record.clone());
        }
    }

    /// The record a read stored as `before` now stands as `after`, which only adds what a
    /// later read embedded (see [`super::query_roles`]): its language and kept variants
    /// stay, each answered with the members it lacks taken from `after`.
    pub(super) fn grew(&self, session: &SessionKey, before: &StoredResource, after: &Value) {
        let Ok(mut known) = self.0.lock() else {
            return;
        };
        if let Some(represented) = known.get_mut(&OwnReads::key(session, before))
            && represented.digest == OwnReads::digest(&before.data)
        {
            represented.digest = OwnReads::digest(after);
        }
    }
}
