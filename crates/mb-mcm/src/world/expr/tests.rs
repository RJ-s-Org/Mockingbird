use std::collections::HashMap;

use serde_json::{Value, json};

use super::*;

/// A fake session: held records by type, counters, and a mint that echoes.
#[derive(Default)]
struct Fake {
    records: HashMap<String, Vec<Value>>,
    seqs: HashMap<String, i64>,
}

impl Fake {
    fn new() -> Self {
        let mut f = Fake::default();
        f.records.insert(
            "Incident".into(),
            vec![
                json!({"id": "i1", "key": "ABC", "status": "open", "amount": 10}),
                json!({"id": "i2", "key": "DEF", "status": "resolved", "amount": 2.5}),
                json!({"id": "i3", "key": "abc", "status": "open", "amount": 7}),
            ],
        );
        f.records.insert(
            "PaymentIntent".into(),
            vec![json!({"id": "pi_1", "currency": "usd", "amount": 1000})],
        );
        f
    }
}

impl Host for Fake {
    fn held(&mut self, ty: &str, id: &Value) -> Option<Value> {
        self.records
            .get(ty)?
            .iter()
            .find(|r| r.get("id") == Some(id))
            .cloned()
    }
    fn all(&mut self, ty: &str) -> Vec<Value> {
        self.records.get(ty).cloned().unwrap_or_default()
    }
    fn seq(&mut self, scope: &str) -> i64 {
        let n = self.seqs.entry(scope.to_string()).or_insert(0);
        *n += 1;
        *n
    }
    fn mint(&mut self, pattern: &str) -> Option<String> {
        (pattern != "bad").then(|| format!("minted:{pattern}"))
    }
}

fn request() -> Value {
    json!({
        "body": {"amount": 250, "frac": 2.5, "key": "abc", "name": "Ada Lovelace",
                 "nested": {"a": {"b": 1}}, "payment_intent": "pi_1", "tags": ["x", "y"],
                 "lines": [{"price": 3, "qty": 2}, {"price": 1.5, "qty": 4}], "nil": null},
        "path": {"repo": "r1"},
        "query": {"sort": "-price"},
        "headers": {"x-id": "h"}
    })
}

fn record() -> Value {
    json!({"amount": 1000, "amount_refunded": 500, "y": "x"})
}

fn run_with(src: &str, record: &Value, it: Option<&Value>) -> Result<Value, String> {
    let p = Program::compile(src).map_err(|e| format!("compile: {e}"))?;
    let req = request();
    let b = Bindings {
        request: &req,
        record,
        now: &json!("2026-11-10T00:00:00Z"),
        it,
    };
    p.eval(&b, &mut Fake::new())
        .map_err(|e| format!("eval: {e}"))
}

fn run(src: &str) -> Result<Value, String> {
    run_with(src, &record(), None)
}

fn ok(src: &str) -> Value {
    match run(src) {
        Ok(v) => v,
        Err(e) => panic!("{src}: {e}"),
    }
}

fn compile_err(src: &str) -> CompileError {
    match Program::compile(src) {
        Ok(_) => panic!("{src}: compiled"),
        Err(e) => e,
    }
}

fn eval_err(src: &str) -> String {
    match run(src) {
        Ok(v) => panic!("{src}: evaluated to {v}"),
        Err(e) => {
            assert!(e.starts_with("eval: "), "{src}: {e}");
            e
        }
    }
}

// ---- grammar and precedence

#[test]
fn literals() {
    assert_eq!(ok("1"), json!(1));
    assert_eq!(ok("0x1F"), json!(31));
    assert_eq!(ok("-9223372036854775808"), json!(i64::MIN));
    assert_eq!(ok("2.5"), json!(2.5));
    assert_eq!(ok("1e3"), json!(1000.0));
    assert_eq!(ok("1.5E-1"), json!(0.15));
    assert_eq!(ok("true"), json!(true));
    assert_eq!(ok("false"), json!(false));
    assert_eq!(ok("null"), Value::Null);
    assert_eq!(ok("'a\\'b'"), json!("a'b"));
    assert_eq!(ok(r#""a\"b\n\t\\""#), json!("a\"b\n\t\\"));
    assert_eq!(ok(r"'\x41é\U0001F600\101'"), json!("Aé😀A"));
    assert_eq!(ok(r"r'a\nb'"), json!("a\\nb"));
    assert_eq!(ok("[1, 'a', [true], ]"), json!([1, "a", [true]]));
    assert_eq!(ok("[]"), json!([]));
    assert_eq!(
        ok("{'a': 1, 'b': {'c': null},}"),
        json!({"a": 1, "b": {"c": null}})
    );
    assert_eq!(ok("{}"), json!({}));
    assert_eq!(ok("1 // a comment\n + 1"), json!(2));
}

#[test]
fn precedence_and_associativity() {
    assert_eq!(ok("1 + 2 * 3"), json!(7));
    assert_eq!(ok("(1 + 2) * 3"), json!(9));
    assert_eq!(ok("10 - 3 - 2"), json!(5));
    assert_eq!(ok("12 / 3 / 2"), json!(2));
    assert_eq!(ok("2 * 3 % 4"), json!(2));
    assert_eq!(ok("-2 * 3"), json!(-6));
    assert_eq!(ok("- -2"), json!(2));
    assert_eq!(ok("!true || true"), json!(true));
    assert_eq!(ok("!(true || true)"), json!(false));
    assert_eq!(ok("true || false && false"), json!(true));
    assert_eq!(ok("1 + 1 == 2 && 2 < 3"), json!(true));
    assert_eq!(ok("1 < 2 == true"), json!(true));
    assert_eq!(ok("true ? 1 : 2"), json!(1));
    assert_eq!(ok("false ? 1 : true ? 2 : 3"), json!(2));
    assert_eq!(ok("1 + 1 == 2 ? 'a' + 'b' : 'c'"), json!("ab"));
    assert_eq!(ok("2 in [1, 2] && !(3 in [1, 2])"), json!(true));
    assert_eq!(ok("1 + 1 in [2]"), json!(true));
    assert_eq!(ok("-request.body.amount"), json!(-250));
    assert_eq!(ok("!has(request.body.zzz)"), json!(true));
}

#[test]
fn members_indexes_and_calls() {
    assert_eq!(ok("request.body.nested.a.b"), json!(1));
    assert_eq!(ok("request.headers['x-id']"), json!("h"));
    assert_eq!(ok("request.body.tags[1]"), json!("y"));
    assert_eq!(ok("request.body.lines[0].price"), json!(3));
    assert_eq!(ok("[[1, 2], [3]][1][0]"), json!(3));
    assert_eq!(ok("{'a': {'b': 2}}.a.b"), json!(2));
    assert_eq!(ok("{'a': 1}['a']"), json!(1));
    assert_eq!(ok("'k_' + request.path.repo"), json!("k_r1"));
    assert_eq!(ok("'abc'.startsWith('ab')"), json!(true));
    assert_eq!(ok("startsWith('abc', 'bc')"), json!(false));
    assert_eq!(ok("request.body.name.lower().size()"), json!(12));
    assert_eq!(ok("record.y"), json!("x"));
    assert_eq!(ok("now"), json!("2026-11-10T00:00:00Z"));
    assert_eq!(ok("'a' in {'a': 1}"), json!(true));
    assert_eq!(ok("'key' in request.body"), json!(true));
}

// ---- null and missing

#[test]
fn null_and_missing() {
    assert_eq!(ok("request.body.missing"), Value::Null);
    assert_eq!(ok("request.body.missing.deeper.still"), Value::Null);
    assert_eq!(ok("request.body.nil.x"), Value::Null);
    assert_eq!(ok("request.body.tags[5]"), Value::Null);
    assert_eq!(ok("request.body.tags[-1]"), Value::Null);
    assert_eq!(ok("request.headers['nope']"), Value::Null);
    assert_eq!(ok("request.body.missing == null"), json!(true));
    assert_eq!(ok("null == null"), json!(true));
    assert_eq!(ok("null != 0"), json!(true));
    assert_eq!(ok("has(request.body.key)"), json!(true));
    assert_eq!(
        ok("has(request.body.nil)"),
        json!(true),
        "present, even if null"
    );
    assert_eq!(ok("has(request.body.missing)"), json!(false));
    assert_eq!(ok("has(request.body.missing.x)"), json!(false));
    assert_eq!(ok("has(request.body.key.x)"), json!(false));
    assert_eq!(ok("1 in request.body.missing"), json!(false));
    // a null record
    let null = Value::Null;
    assert_eq!(run_with("record == null", &null, None), Ok(json!(true)));
    assert_eq!(run_with("record.y", &null, None), Ok(Value::Null));
    assert_eq!(
        run_with("has(record.y) ? record.y : 'none'", &null, None),
        Ok(json!("none"))
    );
    // a member of a non-map is an error, not null
    assert!(eval_err("request.body.key.x").contains("no member 'x' on a string"));
    assert!(eval_err("request.body.tags['a']").contains("list index must be an int"));
    assert!(eval_err("request.body.nested[1]").contains("map index must be a string"));
}

#[test]
fn it_binding() {
    let it = json!({"price": 4});
    assert_eq!(run_with("it.price * 2", &record(), Some(&it)), Ok(json!(8)));
    let e = run("it.price").unwrap_err();
    assert!(e.contains("'it' is not bound here"), "{e}");
}

// ---- numbers

#[test]
fn number_policy() {
    assert_eq!(ok("1 + 2.5"), json!(3.5));
    assert_eq!(ok("1 + 2.5 == 3.5"), json!(true));
    assert_eq!(ok("7 / 2"), json!(3.5));
    let six_halves = ok("6 / 2");
    assert_eq!(six_halves, json!(3));
    assert!(six_halves.is_i64(), "6 / 2 stays an int");
    assert!(ok("1 + 2").is_i64());
    // An integral result is an int, however its operands were spelled.
    assert!(ok("1.0 + 2").is_i64());
    assert!(ok("1.5 + 1.5").is_i64());
    assert_eq!(ok("(1.5 + 1.5) % 2"), json!(1));
    let two = json!({"n": 2.0});
    assert_eq!(run_with("record.n % 2", &two, None), Ok(json!(0)));
    assert_eq!(
        run_with("record.n", &two, None),
        Ok(json!(2.0)),
        "passed through as spelled"
    );
    assert!(ok("0.5 + 0.25").is_f64());
    assert_eq!(ok("string(1.5 * 2)"), json!("3"));
    // Ints and doubles compare exactly, beyond 2^53 too.
    assert_eq!(ok("9007199254740993 == 9007199254740992.0"), json!(false));
    assert_eq!(ok("9007199254740993 > 9007199254740992.0"), json!(true));
    assert_eq!(ok("9223372036854775807 < 1e19"), json!(true));
    assert_eq!(ok("-7 / 2"), json!(-3.5));
    assert_eq!(ok("7 % 3"), json!(1));
    assert_eq!(ok("-7 % 3"), json!(-1));
    assert_eq!(
        ok("record.amount_refunded + request.body.amount"),
        json!(750)
    );
    assert_eq!(
        ok("record.amount_refunded + request.body.frac"),
        json!(502.5)
    );
    assert_eq!(ok("request.body.frac * 2"), json!(5));
    assert_eq!(
        ok("record.amount == 1000.0 && request.body.frac > 2"),
        json!(true)
    );
    assert_eq!(ok("2 < 2.5 && 3.0 >= 3 && 1 != 1.5"), json!(true));
    assert_eq!(ok("[1, 2.0] == [1.0, 2]"), json!(true));
    assert_eq!(ok("{'a': 1} == {'a': 1.0}"), json!(true));
    assert_eq!(
        ok(
            "record.amount_refunded + request.body.amount == record.amount ? 'refunded' : 'partially_refunded'"
        ),
        json!("partially_refunded")
    );
    for overflow in [
        "9223372036854775807 + 1",
        "-9223372036854775808 - 1",
        "4611686018427387904 * 2",
        "-(-9223372036854775808)",
        "-9223372036854775808 / -1",
        "-9223372036854775808 % -1",
    ] {
        assert!(
            eval_err(overflow).contains("integer overflow"),
            "{overflow}"
        );
    }
    assert!(eval_err("1 / 0").contains("division by zero"));
    assert!(eval_err("1 % 0").contains("division by zero"));
    assert!(eval_err("1.0 / 0").contains("division by zero"));
    assert!(eval_err("1.5 / 0").contains("not a finite number"));
    assert!(eval_err("1.5 % 1").contains("'%' needs two ints"));
    assert!(eval_err("1e308 * 10").contains("not a finite number"));
    // a u64 beyond i64 is a double
    let big = json!({"n": u64::MAX});
    assert_eq!(run_with("record.n > 1", &big, None), Ok(json!(true)));
    assert!(
        compile_err("9223372036854775808")
            .message
            .contains("out of range")
    );
    assert!(compile_err("1e999").message.contains("out of range"));
}

#[test]
fn strings_lists_and_type_errors() {
    assert_eq!(ok("'a' + 'b'"), json!("ab"));
    assert_eq!(ok("[1] + [2, 3]"), json!([1, 2, 3]));
    assert_eq!(ok("'a' < 'b' && 'b' <= 'b' && false < true"), json!(true));
    assert_eq!(ok("'a' == 1"), json!(false));
    assert!(eval_err("'a' + 1").contains("cannot combine string and int"));
    assert!(eval_err("'a' < 1").contains("cannot compare string with int"));
    assert!(eval_err("null < 1").contains("cannot compare"));
    assert!(eval_err("!1").contains("needs a bool"));
    assert!(eval_err("-'a'").contains("needs a number"));
    assert!(eval_err("1 && true").contains("expected a bool"));
    assert!(eval_err("request.body.missing ? 1 : 2").contains("expected a bool"));
    assert!(eval_err("1 in 'abc'").contains("'in' needs a list or a map"));
    assert!(eval_err("{1: 2}").contains("map keys must be strings"));
    assert!(eval_err("{'a': 1, 'a': 2}").contains("duplicate map key"));
    // short-circuit: the right side is not evaluated
    assert_eq!(ok("false && 1"), json!(false));
    assert_eq!(ok("true || 1"), json!(true));
}

// ---- functions

#[test]
fn string_functions() {
    assert_eq!(ok("size('héllo')"), json!(5));
    assert_eq!(ok("size([1, 2])"), json!(2));
    assert_eq!(ok("size(request.body)"), json!(9));
    assert_eq!(ok("'hello'.endsWith('lo')"), json!(true));
    assert_eq!(ok("'hello'.contains('ell')"), json!(true));
    assert_eq!(ok("lower('ABC')"), json!("abc"));
    assert_eq!(ok("upper('abc')"), json!("ABC"));
    assert_eq!(
        ok("slug('  Hello, World! 2026 ')"),
        json!("hello-world-2026")
    );
    assert_eq!(ok("slug('Ünïcode -- x')"), json!("n-code-x"));
    assert_eq!(ok("string(42)"), json!("42"));
    assert_eq!(ok("string(2.5)"), json!("2.5"));
    assert_eq!(ok("string(true)"), json!("true"));
    assert_eq!(ok("'#' + string(seq('pr'))"), json!("#1"));
    assert_eq!(ok("int('42')"), json!(42));
    assert_eq!(ok("int(2.9)"), json!(2));
    assert_eq!(ok("int(-2.9)"), json!(-2));
    assert_eq!(ok("double(2)"), json!(2), "an integral result is an int");
    assert_eq!(ok("double('2.5')"), json!(2.5));
    assert!(eval_err("int('x')").contains("cannot parse"));
    assert!(eval_err("int(1e19)").contains("out of range"));
    assert!(eval_err("double('inf')").contains("cannot parse"));
    assert!(eval_err("string(null)").contains("cannot convert"));
    assert!(eval_err("lower(1)").contains("lower() needs a string, got int"));
    assert!(eval_err("size(1)").contains("size() needs"));
}

#[test]
fn hashes() {
    assert_eq!(ok("md5('')"), json!("d41d8cd98f00b204e9800998ecf8427e"));
    assert_eq!(ok("md5('abc')"), json!("900150983cd24fb0d6963f7d28e17f72"));
    assert_eq!(
        ok("sha1('abc')"),
        json!("a9993e364706816aba3e25717850c26c9cd0d89d")
    );
    assert_eq!(
        ok("sha1('')"),
        json!("da39a3ee5e6b4b0d3255bfef95601890afd80709")
    );
    assert_eq!(
        ok("sha256('abc')"),
        json!("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
    );
    assert_eq!(
        ok("sha256('')"),
        json!("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
    );
    // the mailchimp member id shape
    assert_eq!(
        ok("md5(lower('Ada@Example.com')) == md5('ada@example.com')"),
        json!(true)
    );
    assert_eq!(ok("'abc'.md5()"), ok("md5('abc')"), "a method too");
}

#[test]
fn matches() {
    assert_eq!(ok("'abc-123'.matches('^[a-z]+-[0-9]+$')"), json!(true));
    assert_eq!(
        ok("matches('abc', 'b')"),
        json!(true),
        "a search, not a full match"
    );
    assert_eq!(ok("'abc'.matches('^b')"), json!(false));
    assert_eq!(
        ok("request.body.key.matches('^' + 'a')"),
        json!(true),
        "a computed pattern"
    );
    let e = compile_err("'a'.matches('(')");
    assert!(e.message.contains("invalid regular expression"), "{e}");
    assert_eq!(e.pos.column, 13, "points at the pattern");
    assert!(eval_err("'a'.matches('(' + '')").contains("invalid regular expression"));
    // over the size limit
    let e = compile_err("'a'.matches('(\\\\w{1000}){1000}')");
    assert!(e.message.contains("invalid regular expression"), "{e}");
}

#[test]
fn session_functions() {
    assert_eq!(
        ok("held('PaymentIntent', request.body.payment_intent).currency"),
        json!("usd")
    );
    assert_eq!(ok("held('PaymentIntent', 'nope')"), Value::Null);
    assert_eq!(ok("held('PaymentIntent', 'nope').currency"), Value::Null);
    assert_eq!(
        ok("held('PaymentIntent', request.body.missing)"),
        Value::Null
    );
    assert_eq!(ok("[seq('a'), seq('a'), seq('b')]"), json!([1, 2, 1]));
    assert_eq!(ok("seq('pr:' + request.path.repo)"), json!(1));
    assert_eq!(ok("mint('[0-9]{12}')"), json!("minted:[0-9]{12}"));
    assert!(eval_err("mint('bad')").contains("mint() cannot generate"));
    assert!(eval_err("seq(1)").contains("seq() needs a string"));
}

#[test]
fn seq_state_lives_in_the_host() {
    let p = Program::compile("seq('incident')").unwrap();
    let mut host = Fake::new();
    let (req, rec, now) = (request(), Value::Null, Value::Null);
    let b = Bindings {
        request: &req,
        record: &rec,
        now: &now,
        it: None,
    };
    for want in 1..=3 {
        assert_eq!(p.eval(&b, &mut host), Ok(json!(want)));
    }
}

// ---- macros

#[test]
fn list_macros() {
    assert_eq!(ok("[1, 2, 3].all(x, x > 0)"), json!(true));
    assert_eq!(ok("[1, 2, 3].all(x, x > 1)"), json!(false));
    assert_eq!(ok("[1, 2].exists(x, x == 2)"), json!(true));
    assert_eq!(ok("[].exists(x, x == 2)"), json!(false));
    assert_eq!(ok("[1, 2, 3].map(x, x * 2)"), json!([2, 4, 6]));
    assert_eq!(ok("[1, 2, 3].filter(x, x % 2 == 1)"), json!([1, 3]));
    assert_eq!(
        ok("request.body.lines.map(l, l.price * l.qty)"),
        json!([6, 6])
    );
    assert_eq!(
        ok("{'a': 1, 'b': 2}.map(k, k)"),
        json!(["a", "b"]),
        "maps range over keys"
    );
    assert_eq!(
        ok("[[1], [2, 3]].map(x, x.map(y, y + 1))"),
        json!([[2], [3, 4]])
    );
    // the outer bindings stay visible inside
    assert_eq!(ok("[10].map(x, x + record.amount)"), json!([1010]));
    assert!(eval_err("request.body.missing.all(x, true)").contains("expected a list, got null"));
    assert!(eval_err("[1].all(x, x)").contains("expected a bool"));
}

#[test]
fn global_macros_with_it() {
    assert_eq!(ok("count('Incident', it.status == 'open')"), json!(2));
    assert_eq!(ok("count('Nope', true)"), json!(0));
    assert_eq!(
        ok(
            "exists('Incident', lower(it.key) == lower(request.body.key) && it.status != 'resolved')"
        ),
        json!(true)
    );
    assert_eq!(ok("exists('Incident', it.key == 'zzz')"), json!(false));
    assert_eq!(ok("exists('Nope', true)"), json!(false));
    assert_eq!(
        ok("count('Incident', it.amount < record.amount_refunded)"),
        json!(3)
    );
    assert_eq!(ok("sum([1, 2, 3], it * 2)"), json!(12));
    assert!(ok("sum([1, 2, 3], it)").is_i64());
    assert_eq!(ok("sum([{'a': 1}, {'a': 2.5}], it.a)"), json!(3.5));
    assert_eq!(ok("sum([], it)"), json!(0));
    assert_eq!(ok("sum(request.body.lines, it.price * it.qty)"), json!(12));
    // an inner `it` shadows an outer one
    assert_eq!(
        ok("sum([1, 2], count('Incident', it.amount > 5))"),
        json!(4)
    );
    assert_eq!(
        ok("count('Incident', sum([it.amount], it) > 5)"),
        json!(2),
        "sum's it shadows count's"
    );
    let it = json!({"key": "ABC"});
    assert_eq!(
        run_with("exists('Incident', it.key == 'DEF')", &record(), Some(&it)),
        Ok(json!(true))
    );
    assert!(eval_err("sum(['a'], it)").contains("sum() needs numbers, got string"));
    assert!(eval_err("sum([9223372036854775807, 1], it)").contains("integer overflow"));
    assert!(eval_err("count('Incident', it.amount)").contains("expected a bool"));
}

// ---- compile errors

#[test]
fn unknown_names_and_arity() {
    assert!(
        compile_err("foo(1)")
            .message
            .contains("unknown function foo()")
    );
    assert!(
        compile_err("'a'.foo()")
            .message
            .contains("unknown function foo()")
    );
    assert!(
        compile_err("size(1, 2)")
            .message
            .contains("size() takes 1 argument, got 2")
    );
    assert!(
        compile_err("'a'.startsWith()")
            .message
            .contains("takes 1 argument, got 0")
    );
    assert!(
        compile_err("count('Incident')")
            .message
            .contains("count() takes 2 arguments, got 1")
    );
    assert!(
        compile_err("held('T')")
            .message
            .contains("takes 2 arguments")
    );
    assert!(compile_err("has(a)").message.contains("member access"));
    assert!(
        compile_err("has(record['a'])")
            .message
            .contains("member access")
    );
    assert!(
        compile_err("count(request.body.t, true)")
            .message
            .contains("type name in quotes")
    );
    assert!(
        compile_err("held('', 1)")
            .message
            .contains("type name in quotes")
    );
    assert!(
        compile_err("[1].all(1, true)")
            .message
            .contains("must be a variable name")
    );
    assert!(
        compile_err("[1].map(x)")
            .message
            .contains("takes 2 arguments")
    );
    assert!(compile_err("all([1], x)").message.contains("list macro"));
    assert!(compile_err("'a'.seq()").message.contains("global function"));
    assert!(compile_err("x + 1").message.contains("unknown name 'x'"));
    assert!(
        compile_err("[1].map(x, y)")
            .message
            .contains("unknown name 'y'")
    );
    assert!(
        compile_err("request.method")
            .message
            .contains("request has no 'method'")
    );
    for src in ["request['body'].secret", "request['nope']", "request[0]"] {
        assert!(
            compile_err(src)
                .message
                .contains("request cannot be indexed"),
            "{src}"
        );
    }
    assert!(
        compile_err("has(request.method)")
            .message
            .contains("request has no 'method'")
    );
    assert_eq!(ok("has(request.body)"), json!(true));
    assert!(
        compile_err("[1].map(record, 1)")
            .message
            .contains("cannot be a macro variable")
    );
    assert!(
        compile_err("[1].map(request, 1)")
            .message
            .contains("cannot be a macro variable")
    );
    // `x` is in scope only inside its macro
    assert!(
        compile_err("[1].map(x, x) + [x]")
            .message
            .contains("unknown name 'x'")
    );
}

#[test]
fn syntax_errors_and_positions() {
    let e = compile_err("request.body.x +");
    assert_eq!((e.pos.line, e.pos.column, e.pos.offset), (1, 17, 16), "{e}");
    assert!(e.message.contains("end of expression"), "{e}");
    let e = compile_err("a ==\n  (b");
    assert_eq!(e.pos.line, 2, "{e}");
    let e = compile_err("1 +\n  foo(2)");
    assert_eq!((e.pos.line, e.pos.column), (2, 3), "{e}");
    assert_eq!(e.to_string(), "line 2, column 3: unknown function foo()");
    let e = compile_err("'é' + bar");
    assert_eq!(
        (e.pos.column, e.pos.offset),
        (7, 7),
        "columns count characters"
    );
    let e = compile_err("a = 1");
    assert!(e.message.contains("use '=='"), "{e}");
    assert!(compile_err("'abc").message.contains("unterminated"));
    assert!(compile_err("'a\\qb'").message.contains("invalid escape"));
    assert!(compile_err("1u").message.contains("malformed number"));
    assert!(compile_err("true ? 1").message.contains("':'"));
    assert!(compile_err("[1, 2").message.contains("']'"));
    assert!(
        compile_err("{'a' 1}")
            .message
            .contains("':' after a map key")
    );
    assert!(compile_err("1 2").message.contains("unexpected number 2"));
    assert!(compile_err("a.1").message.contains("member name"));
    assert!(compile_err("#").message.contains("unexpected character"));
    assert!(compile_err("").message.contains("end of expression"));
    // eval errors carry positions too
    let p = Program::compile("1 +\n  'a'.size() + 'b'").unwrap();
    let (r, n) = (request(), Value::Null);
    let b = Bindings {
        request: &r,
        record: &n,
        now: &n,
        it: None,
    };
    let e = p.eval(&b, &mut Fake::new()).unwrap_err();
    assert_eq!((e.pos.line, e.pos.column), (2, 14), "{e}");
}

// ---- caps

#[test]
fn depth_cap() {
    let parens = |n: usize| format!("{}1{}", "(".repeat(n), ")".repeat(n));
    assert!(Program::compile(&parens(20)).is_ok());
    assert!(
        compile_err(&parens(1000))
            .message
            .contains("nested too deeply")
    );
    let lists = |n: usize| format!("{}1{}", "[".repeat(n), "]".repeat(n));
    assert!(Program::compile(&lists(30)).is_ok());
    assert!(
        compile_err(&lists(40))
            .message
            .contains("nested too deeply")
    );
    let chain = |n: usize| vec!["1"; n].join(" + ");
    assert_eq!(ok(&chain(30)), json!(30));
    assert!(
        compile_err(&chain(100))
            .message
            .contains("nested too deeply")
    );
    assert!(
        compile_err(&format!("{}true", "!".repeat(100_000)))
            .message
            .contains("nested too deeply")
    );
    assert!(
        compile_err(&format!("{}1", "-".repeat(40)))
            .message
            .contains("nested too deeply")
    );
    let members = format!("record{}", ".a".repeat(40));
    assert!(compile_err(&members).message.contains("nested too deeply"));
    let ternary = format!("{}1", "true ? 1 : ".repeat(40));
    assert!(compile_err(&ternary).message.contains("nested too deeply"));
    let calls = format!("{}'a'{}", "lower(".repeat(40), ")".repeat(40));
    assert!(compile_err(&calls).message.contains("nested too deeply"));
}

#[test]
fn node_cap() {
    let wide = |n: usize| format!("[{}]", vec!["1"; n].join(", "));
    assert!(Program::compile(&wide(500)).is_ok());
    assert!(
        compile_err(&wide(600))
            .message
            .contains("more than 512 nodes")
    );
    let map = format!(
        "{{{}}}",
        (0..300)
            .map(|i| format!("'k{i}': {i}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    assert!(compile_err(&map).message.contains("more than 512 nodes"));
}

#[test]
fn step_cap() {
    let mut f = Fake::new();
    f.records.insert(
        "Big".into(),
        (0..3000)
            .map(|i| json!({"id": format!("b{i}"), "n": i}))
            .collect(),
    );
    let (r, n) = (request(), Value::Null);
    let b = Bindings {
        request: &r,
        record: &n,
        now: &n,
        it: None,
    };
    let p = Program::compile("count('Big', it.n > 3)").unwrap();
    let e = p.eval(&b, &mut f).unwrap_err();
    assert!(e.message.contains("budget of 10000 steps"), "{e}");
    assert_eq!(p.eval_with_budget(&b, &mut f, 100_000), Ok(json!(2996)));
    // nested comprehensions multiply
    let p = Program::compile(
        "[1,2,3,4,5,6,7,8,9,10].all(a, [1,2,3,4,5,6,7,8,9,10].all(b, [1,2,3,4,5,6,7,8,9,10].all(c, [1,2,3,4,5,6,7,8,9,10].all(d, true))))",
    )
    .unwrap();
    let e = p.eval(&b, &mut f).unwrap_err();
    assert!(e.message.contains("budget"), "{e}");
    // count and exists pay for every record the host returns, even when
    // exists stops at the first
    let p = Program::compile("exists('Big', true)").unwrap();
    assert!(p.eval_with_budget(&b, &mut f, 2000).is_err());
    assert_eq!(p.eval_with_budget(&b, &mut f, 4000), Ok(json!(true)));
    // a computed pattern costs a compile on every call
    let p = Program::compile("[1,2,3,4,5,6,7,8,9,10].all(x, 'a'.matches('a' + 'b?'))").unwrap();
    assert!(p.eval_with_budget(&b, &mut f, 400).is_err());
    assert_eq!(p.eval_with_budget(&b, &mut f, 1000), Ok(json!(true)));
    let p = Program::compile("[1,2,3,4,5,6,7,8,9,10].all(x, 'a'.matches('ab?'))").unwrap();
    assert_eq!(p.eval_with_budget(&b, &mut f, 400), Ok(json!(true)));
    // a small budget trips on plain nodes
    let p = Program::compile("1 + 2 + 3").unwrap();
    assert!(p.eval_with_budget(&b, &mut f, 3).is_err());
    assert_eq!(p.eval_with_budget(&b, &mut f, 5), Ok(json!(6)));
}

/// Evaluates `src` against a request whose body is `body`.
fn eval_body(src: &str, body: Value) -> Result<Value, EvalError> {
    let req = json!({ "body": body });
    let n = Value::Null;
    let b = Bindings {
        request: &req,
        record: &n,
        now: &n,
        it: None,
    };
    Program::compile(src).unwrap().eval(&b, &mut Fake::new())
}

#[test]
fn size_cap() {
    // Each level doubles a string while the tree grows by one level only.
    let mut src = String::from("[request.body.s]");
    let levels = 14;
    for i in 0..levels {
        src.push_str(&format!(".map(v{i}, [v{i} + v{i}]"));
    }
    src.push_str(".map(w, size(w))");
    src.push_str(&")".repeat(levels));
    let e = eval_body(&src, json!({"s": "x".repeat(1024)})).unwrap_err();
    assert!(e.message.contains("budget"), "{e}");
    // A big value read once per item is charged each time.
    let e = eval_body(
        "request.body.items.map(x, request.body.big)",
        json!({"items": vec![0; 200], "big": "y".repeat(64 * 1024)}),
    )
    .unwrap_err();
    assert!(e.message.contains("budget"), "{e}");
    // Lists, case changes and filters are charged too.
    for src in [
        "request.body.items.map(x, request.body.list + request.body.list)",
        "request.body.items.map(x, upper(request.body.big))",
        "request.body.items.filter(x, true).map(x, request.body.items.filter(y, true))",
    ] {
        let e = eval_body(
            src,
            json!({"items": vec![0; 200], "big": "y".repeat(64 * 1024), "list": vec!["z"; 4096]}),
        )
        .unwrap_err();
        assert!(e.message.contains("budget"), "{src}: {e}");
    }
    // An ordinary amount of data is well within the budget.
    assert_eq!(
        eval_body(
            "size(request.body.big + request.body.big)",
            json!({"big": "y".repeat(256 * 1024)})
        ),
        Ok(json!(512 * 1024))
    );
}

// ---- static check

#[test]
fn check_members_types_functions() {
    let p = Program::compile(
        "record.amount_refunded + request.body.amount == record.amount \
         && exists('Incident', it.key == request.body.key) \
         && held('PaymentIntent', request.body.payment_intent).currency == 'usd' \
         && has(request.body.opt) && request.headers['x-id'] != null \
         && request.body.lines.all(l, l.price > 0) && sum(record.items, it.qty) > 0 \
         && request.query.list[request.path.i].name == ''",
    )
    .unwrap();
    let refs = p.check();
    let members: Vec<String> = refs.members.iter().map(ToString::to_string).collect();
    for want in [
        "record.amount_refunded",
        "request.body.amount",
        "record.amount",
        "Incident.key",
        "request.body.key",
        "PaymentIntent.currency",
        "request.body.payment_intent",
        "request.body.opt",
        "request.headers.x-id",
        "request.body.lines",
        "request.body.lines[].price",
        "record.items",
        "record.items[].qty",
        "request.query.list[].name",
        "request.path.i",
    ] {
        assert!(
            members.contains(&want.to_string()),
            "missing {want} in {members:?}"
        );
    }
    // no partial chains
    assert!(
        !members.contains(&"request.body".to_string()),
        "{members:?}"
    );
    let types: Vec<(&str, &str)> = refs
        .types
        .iter()
        .map(|t| (t.name.as_str(), t.function))
        .collect();
    assert_eq!(types, [("Incident", "exists"), ("PaymentIntent", "held")]);
    let functions: Vec<&str> = refs.functions.iter().map(|f| f.name).collect();
    assert_eq!(functions, ["exists", "held", "has", "all", "sum"]);
    // positions: the member's name, the type's literal
    let src = p.source();
    let amount = refs
        .members
        .iter()
        .find(|m| m.to_string() == "request.body.amount")
        .unwrap();
    assert_eq!(&src[amount.pos.offset..amount.pos.offset + 6], "amount");
    assert_eq!(
        &src[refs.types[0].pos.offset..refs.types[0].pos.offset + 10],
        "'Incident'"
    );
}

#[test]
fn check_it_and_shadowing() {
    let refs = Program::compile("it.a + [1].map(it, it)[0] + count('T', it.b)")
        .unwrap()
        .check();
    let members: Vec<String> = refs.members.iter().map(ToString::to_string).collect();
    assert_eq!(members, ["it.a", "T.b"]);
}

// ---- Program

#[test]
fn program_is_send_sync_and_reusable() {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<Program>();
    let p = std::sync::Arc::new(Program::compile("request.body.amount * 2").unwrap());
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let p = std::sync::Arc::clone(&p);
            std::thread::spawn(move || {
                let (r, n) = (request(), Value::Null);
                let b = Bindings {
                    request: &r,
                    record: &n,
                    now: &n,
                    it: None,
                };
                p.eval(&b, &mut Fake::new())
            })
        })
        .collect();
    for h in handles {
        assert_eq!(h.join().unwrap(), Ok(json!(500)));
    }
}

// ---- no input panics

#[test]
fn malformed_inputs_are_errors() {
    let bad = [
        "",
        " ",
        "(",
        ")",
        "[",
        "]",
        "{",
        "}",
        ",",
        ".",
        ":",
        "?",
        "!",
        "-",
        "+",
        "*",
        "/",
        "%",
        "&&",
        "||",
        "==",
        "!=",
        "<",
        "a.",
        ".a",
        "a..b",
        "a[",
        "a[]",
        "a[1",
        "a(",
        "f(,)",
        "(1,2)",
        "1 +",
        "+ 1",
        "1 + + ",
        "true ?",
        "true ? 1 :",
        "? 1 : 2",
        "{'a'}",
        "{'a':}",
        "{:1}",
        "[,]",
        "[1,,2]",
        "'",
        "\"",
        "'\\",
        "'\\x'",
        "'\\u12'",
        "'\\uD800'",
        "'\\777'",
        "r'",
        "0x",
        "0xZ",
        "1e",
        "1.",
        "1.e5",
        "1..2",
        "99999999999999999999",
        "1e400",
        "has()",
        "has(1)",
        "has(a.b, c)",
        "count()",
        "count('T')",
        "count(1, 2)",
        "sum()",
        "sum(1)",
        "held()",
        "[].all()",
        "[].all(1, 2)",
        "[].map(x)",
        "x.y.z(",
        "a b",
        "request",
        "request.",
        "request..body",
        "record[",
        "it[0",
        "null.",
        "true.false",
        "in",
        "1 in",
        "in 1",
        "'a' 'b'",
        "\u{0}",
        "\u{feff}1",
        "é",
        "a\nb",
        "1 // c\n +",
        "'\n'",
        "=",
        "&",
        "|",
        "a = b",
        "a & b",
        "a | b",
        "!=1",
        "()",
        "[](",
        "{}.",
        "{}[",
        "lower()",
        "lower(1, 2)",
        "'a'.matches('[')",
        "'a'.matches('(')",
        "mint()",
        "seq()",
    ];
    for src in bad {
        let r = std::panic::catch_unwind(|| Program::compile(src));
        match r {
            Ok(Ok(p)) => {
                // a few of these compile; evaluating them must not panic either
                let (rq, n) = (request(), Value::Null);
                let b = Bindings {
                    request: &rq,
                    record: &n,
                    now: &n,
                    it: None,
                };
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    p.eval(&b, &mut Fake::new())
                }));
                assert!(r.is_ok(), "{src:?} panicked on eval");
            }
            Ok(Err(e)) => assert!(!e.message.is_empty(), "{src:?}"),
            Err(_) => panic!("{src:?} panicked"),
        }
    }
}

#[test]
fn fuzz_like_mutations_never_panic() {
    // Every prefix, and every single-character deletion, of valid expressions.
    let seeds = [
        "count('Incident', it.status == 'open' && lower(it.key) == request.body.key) > 0 ? 'a' : 'b'",
        "sum(request.body.lines, it.price * it.qty) + -1.5e2 / 3 % 2",
        "request.body.lines.filter(l, l.qty > 1).map(l, {'p': l.price, 'k': [1, 2][0]})",
        "has(record.a.b) || 'x\\u00e9\\n'.matches('^[a-z]+$') && md5(string(seq('s'))) in ['a']",
        "held('PaymentIntent', request.body['payment_intent']).currency + mint('[0-9]{4}')",
    ];
    let (rq, n) = (request(), Value::Null);
    let b = Bindings {
        request: &rq,
        record: &n,
        now: &n,
        it: None,
    };
    let mut compiled = 0;
    for seed in seeds {
        let idx: Vec<usize> = seed
            .char_indices()
            .map(|(i, _)| i)
            .chain([seed.len()])
            .collect();
        let mut variants: Vec<String> = idx.iter().map(|&i| seed[..i].to_string()).collect();
        for w in idx.windows(2) {
            variants.push(format!("{}{}", &seed[..w[0]], &seed[w[1]..]));
        }
        for v in variants {
            let r = std::panic::catch_unwind(|| Program::compile(&v));
            let Ok(r) = r else { panic!("{v:?} panicked") };
            if let Ok(p) = r {
                compiled += 1;
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    p.eval(&b, &mut Fake::new())
                }));
                assert!(r.is_ok(), "{v:?} panicked on eval");
            }
        }
    }
    assert!(compiled > 0);
}
