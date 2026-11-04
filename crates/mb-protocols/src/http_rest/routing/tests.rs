//! The router's unit tests; the seeded comparison with `matchit` and a
//! brute-force oracle is in `compare`.

use super::*;

/// A table of `routes` (method, path) whose operation ids are the
/// paths, built through the same code as a contract's.
fn table(routes: &[(HttpMethod, &str)]) -> RouteTable {
    let mut contract = Trie::default();
    let mut skipped = Vec::new();
    for (method, path) in routes {
        if let Err(e) = contract.insert(*method, normalize(path), path, path) {
            skipped.push(e.for_operation(path, *method, path));
        }
    }
    RouteTable {
        contract,
        prefixed: Trie::default(),
        skipped,
    }
}

fn gets(paths: &[&str]) -> RouteTable {
    let routes: Vec<_> = paths.iter().map(|p| (HttpMethod::Get, *p)).collect();
    let t = table(&routes);
    assert!(t.skipped().is_empty(), "{:?}", t.skipped());
    t
}

/// The route (its path) and parameters `GET path` finds.
fn get(t: &RouteTable, path: &str) -> Option<(String, Vec<(String, String)>)> {
    t.find(HttpMethod::Get, path).map(|(op, params)| {
        let mut params: Vec<(String, String)> = params
            .into_iter()
            .map(|(k, v)| (k, v.as_str().unwrap_or_default().to_owned()))
            .collect();
        params.sort();
        (op, params)
    })
}

fn found(t: &RouteTable, path: &str) -> Option<String> {
    get(t, path).map(|(op, _)| op)
}

fn p(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    let mut v: Vec<_> = pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    v.sort();
    v
}

#[test]
fn normalizes_trailing_slash() {
    assert_eq!(normalize("/"), "/");
    assert_eq!(normalize("/orders/"), "/orders");
    assert_eq!(normalize("/orders"), "/orders");
}

#[test]
fn decodes_percent_escapes() {
    assert_eq!(percent_decode("o%201"), "o 1");
    assert_eq!(percent_decode("a%2Fb"), "a/b");
    assert_eq!(percent_decode("100%"), "100%");
    assert_eq!(percent_decode("%zz"), "%zz");
    assert_eq!(percent_decode("%+1"), "%+1");
    assert_eq!(percent_decode("caf%C3%A9"), "café");
}

#[test]
fn odata_keys_with_sibling_collections() {
    let t = gets(&[
        "/companies",
        "/companies({company_id})",
        "/companies({company_id})/customers",
        "/companies({company_id})/customers({customer_id})",
        "/companies({company_id})/salesInvoices",
        "/companies({company_id})/salesInvoices({sales_invoice_id})",
    ]);
    assert_eq!(
        get(&t, "/companies(c1)/customers(k%201)"),
        Some((
            "/companies({company_id})/customers({customer_id})".into(),
            p(&[("company_id", "c1"), ("customer_id", "k 1")])
        ))
    );
    assert_eq!(
        get(&t, "/companies(c1)/salesInvoices(s2)/"),
        Some((
            "/companies({company_id})/salesInvoices({sales_invoice_id})".into(),
            p(&[("company_id", "c1"), ("sales_invoice_id", "s2")])
        ))
    );
    assert_eq!(
        found(&t, "/companies(c1)/customers").as_deref(),
        Some("/companies({company_id})/customers")
    );
    assert_eq!(
        found(&t, "/companies(c1)").as_deref(),
        Some("/companies({company_id})")
    );
    assert_eq!(found(&t, "/companies()"), None, "a key is never empty");
    assert_eq!(found(&t, "/companies(c1"), None);
    assert_eq!(found(&t, "/companies(c1)x"), None);
    assert_eq!(found(&t, "/companies(c1)/vendors(v1)"), None);
}

#[test]
fn several_parameters_in_one_segment() {
    let t = gets(&[
        "/pairs(left={a},right={b})",
        "/files/{name}.{ext}",
        "/v/{a}-{b}",
    ]);
    assert_eq!(
        get(&t, "/pairs(left=1,right=2)"),
        Some((
            "/pairs(left={a},right={b})".into(),
            p(&[("a", "1"), ("b", "2")])
        ))
    );
    assert_eq!(found(&t, "/pairs(left=1,right=)"), None);
    assert_eq!(found(&t, "/pairs(left=,right=2)"), None);
    assert_eq!(
        get(&t, "/files/a.tar.gz").map(|(_, p)| p),
        Some(p(&[("name", "a"), ("ext", "tar.gz")]))
    );
    // The first separator that lets the rest match.
    assert_eq!(
        get(&t, "/v/1-2-3").map(|(_, p)| p),
        Some(p(&[("a", "1"), ("b", "2-3")]))
    );
    assert_eq!(
        get(&t, "/v/-1-2").map(|(_, p)| p),
        Some(p(&[("a", "-1"), ("b", "2")]))
    );
    assert_eq!(found(&t, "/v/12"), None);
}

#[test]
fn suffixes_and_prefixes_within_a_segment() {
    let t = gets(&[
        "/files/{name}.json",
        "/files/{name}.tar.gz",
        "/files/{name}.gz",
        "/files/{name}",
        "/files/index.json",
        "/items/{id}:archive",
        "/items/{id}:restore",
        "/items/{id}",
        "/items/{id}/notes",
        "/img/img-{id}.png",
        "/a/pre{x}",
        "/a/p{x}",
        "/a/{y}",
    ]);
    let cases = [
        ("/files/a.json", "/files/{name}.json", "a"),
        ("/files/a.b.json", "/files/{name}.json", "a.b"),
        ("/files/a.tar.gz", "/files/{name}.tar.gz", "a"),
        ("/files/a.gz", "/files/{name}.gz", "a"),
        ("/files/.json", "/files/{name}", ".json"),
        ("/files/a", "/files/{name}", "a"),
        ("/items/1:archive", "/items/{id}:archive", "1"),
        ("/items/1:restore", "/items/{id}:restore", "1"),
        ("/items/1", "/items/{id}", "1"),
        ("/items/1:other", "/items/{id}", "1:other"),
        ("/img/img-7.png", "/img/img-{id}.png", "7"),
    ];
    for (path, route, value) in cases {
        let (op, params) = get(&t, path).unwrap();
        assert_eq!(op, route, "{path}");
        assert_eq!(params[0].1, value, "{path}");
    }
    assert_eq!(found(&t, "/img/img-7.jpg"), None);
    assert_eq!(found(&t, "/img/img-.png"), None);
    assert_eq!(
        found(&t, "/files/index.json").as_deref(),
        Some("/files/index.json")
    );
    assert_eq!(
        found(&t, "/items/1/notes").as_deref(),
        Some("/items/{id}/notes")
    );
    assert_eq!(found(&t, "/a/prez").as_deref(), Some("/a/pre{x}"));
    assert_eq!(found(&t, "/a/pre").as_deref(), Some("/a/p{x}"));
    assert_eq!(found(&t, "/a/q").as_deref(), Some("/a/{y}"));
}

#[test]
fn precedence_static_then_template_then_param_then_catch_all() {
    let t = gets(&[
        "/{*rest}",
        "/a/{x}",
        "/a/b",
        "/a/{x}.json",
        "/s/{*path}",
        "/s/{x}/meta",
    ]);
    assert_eq!(found(&t, "/a/b").as_deref(), Some("/a/b"));
    assert_eq!(found(&t, "/a/b.json").as_deref(), Some("/a/{x}.json"));
    assert_eq!(found(&t, "/a/c").as_deref(), Some("/a/{x}"));
    assert_eq!(
        get(&t, "/a/c/d"),
        Some(("/{*rest}".into(), p(&[("rest", "a/c/d")])))
    );
    assert_eq!(
        found(&t, "/").as_deref(),
        None,
        "a catch-all is never empty"
    );
    assert_eq!(found(&t, "/s/1/meta").as_deref(), Some("/s/{x}/meta"));
    assert_eq!(
        get(&t, "/s/1/raw"),
        Some(("/s/{*path}".into(), p(&[("path", "1/raw")])))
    );
}

#[test]
fn the_first_differing_segment_decides_and_matching_backtracks() {
    let t = gets(&["/a/b", "/a/{x}", "/a/b/c", "/a/{x}/d", "/a/b/{y}/e"]);
    assert_eq!(found(&t, "/a/b/c").as_deref(), Some("/a/b/c"));
    assert_eq!(found(&t, "/a/b/d").as_deref(), Some("/a/{x}/d"));
    assert_eq!(found(&t, "/a/z/d").as_deref(), Some("/a/{x}/d"));
    assert_eq!(found(&t, "/a/b/d/e").as_deref(), Some("/a/b/{y}/e"));
    let t = gets(&["/u/{id}", "/u/{user_id}/posts", "/u/{id}/{tab}"]);
    assert_eq!(
        get(&t, "/u/1/posts"),
        Some(("/u/{user_id}/posts".into(), p(&[("user_id", "1")])))
    );
    assert_eq!(found(&t, "/u/1/likes").as_deref(), Some("/u/{id}/{tab}"));
}

#[test]
fn escaped_braces_are_literal() {
    let t = gets(&["/x/{{lit}}", "/x/{p}"]);
    assert_eq!(found(&t, "/x/{lit}").as_deref(), Some("/x/{{lit}}"));
    assert_eq!(found(&t, "/x/%7Blit%7D").as_deref(), Some("/x/{p}"));
}

#[test]
fn root_and_trailing_slashes() {
    let t = gets(&["/", "/{x}"]);
    assert_eq!(found(&t, "/").as_deref(), Some("/"));
    assert_eq!(found(&t, "/q/").as_deref(), Some("/{x}"));
    assert_eq!(found(&t, "/q//"), None, "an empty segment is no parameter");
}

#[test]
fn identical_shapes_conflict_and_the_rest_still_route() {
    let t = table(&[
        (HttpMethod::Get, "/a/{x}"),
        (HttpMethod::Get, "/a/{y}"),
        (HttpMethod::Get, "/b"),
        (HttpMethod::Get, "/b/"),
        (HttpMethod::Get, "/c({k})/d"),
        (HttpMethod::Get, "/c({key})/d"),
        (HttpMethod::Delete, "/a/{y}"),
        (HttpMethod::Get, "/ok"),
    ]);
    let skipped: Vec<&str> = t.skipped().iter().map(RouteError::operation).collect();
    assert_eq!(skipped, ["/a/{y}", "/b/", "/c({key})/d"]);
    assert_eq!(
        t.skipped()[0].to_string(),
        "operation /a/{y}: cannot route GET /a/{y}: it matches the same requests as \
         operation /a/{x} (GET /a/{x}), which takes them"
    );
    assert_eq!(found(&t, "/a/1").as_deref(), Some("/a/{x}"));
    assert_eq!(found(&t, "/ok").as_deref(), Some("/ok"));
    assert_eq!(
        t.find(HttpMethod::Delete, "/a/1")
            .map(|(op, _)| op)
            .as_deref(),
        Some("/a/{y}")
    );
}

#[test]
fn invalid_patterns_are_skipped_with_a_reason() {
    let cases = [
        ("/a/{x", "unclosed `{` in `{x`"),
        ("/a/x}", "unmatched `}` in `x}`"),
        ("/a/{}", "empty parameter name in `{}`"),
        ("/a/{*}", "empty parameter name in `{*}`"),
        (
            "/a/{x}{y}",
            "two parameters in `{x}{y}` must be separated by literal text",
        ),
        (
            "/a/{*rest}/b",
            "the catch-all parameter in `{*rest}` must be the last segment",
        ),
        (
            "/a/x{*rest}",
            "the catch-all parameter in `x{*rest}` must be the whole segment",
        ),
    ];
    for (path, reason) in cases {
        let t = table(&[(HttpMethod::Get, path), (HttpMethod::Get, "/ok")]);
        assert_eq!(
            t.skipped(),
            [RouteError::Invalid {
                op: path.into(),
                method: HttpMethod::Get,
                path: path.into(),
                reason: reason.into(),
            }],
            "{path}"
        );
        assert!(found(&t, "/ok").is_some());
    }
}

/// `matchit` refused this set (a prefix-and-suffix template beside a
/// suffix one); here the longer literal text wins.
#[test]
fn prefix_and_suffix_templates_side_by_side() {
    let t = gets(&["/a/x{p}y", "/a/{p}.json", "/a/{q}.json/meta"]);
    assert_eq!(
        get(&t, "/a/x1y"),
        Some(("/a/x{p}y".into(), p(&[("p", "1")])))
    );
    assert_eq!(
        get(&t, "/a/x1y.json"),
        Some(("/a/{p}.json".into(), p(&[("p", "x1y")])))
    );
    assert_eq!(
        found(&t, "/a/x.json/meta").as_deref(),
        Some("/a/{q}.json/meta")
    );
}

#[test]
fn the_doc_example_first_differing_segment() {
    let t = gets(&["/a/{y}/c", "/a/b/{x}"]);
    assert_eq!(
        get(&t, "/a/b/c"),
        Some(("/a/b/{x}".into(), p(&[("x", "c")])))
    );
    assert_eq!(
        get(&t, "/a/z/c"),
        Some(("/a/{y}/c".into(), p(&[("y", "z")])))
    );
}

/// `matchit` answered these with a 404: it did not backtrack out of a
/// template once the template matched.
#[test]
fn backtracks_out_of_a_template_where_matchit_gave_up() {
    let t = gets(&["/files/{id}/{rev}", "/files/{name}.json/meta"]);
    assert_eq!(
        get(&t, "/files/f.json/7"),
        Some((
            "/files/{id}/{rev}".into(),
            p(&[("id", "f.json"), ("rev", "7")])
        ))
    );
    assert_eq!(
        found(&t, "/files/f.json/meta").as_deref(),
        Some("/files/{name}.json/meta")
    );
    let t = gets(&["/a/{x}/b{y}", "/a/{x}.json/c", "/{*rest}"]);
    assert_eq!(
        get(&t, "/a/f.json/bz"),
        Some(("/a/{x}/b{y}".into(), p(&[("x", "f.json"), ("y", "z")])))
    );
}

#[test]
fn equal_literal_lengths_order_by_pieces_not_by_contract_order() {
    // One byte of literal text each: `a{x}` (literal first) sorts before
    // `{x}a` whichever is declared first.
    for routes in [["/r/a{x}", "/r/{x}a"], ["/r/{x}a", "/r/a{x}"]] {
        let t = gets(&routes);
        assert_eq!(found(&t, "/r/aza").as_deref(), Some("/r/a{x}"));
    }
    // More literal text wins over declaration order.
    for routes in [["/r/a{x}", "/r/{x}ba"], ["/r/{x}ba", "/r/a{x}"]] {
        let t = gets(&routes);
        assert_eq!(found(&t, "/r/aba").as_deref(), Some("/r/{x}ba"));
    }
}

#[test]
fn allowed_lists_every_method_in_one_walk() {
    let t = table(&[
        (HttpMethod::Get, "/a/{x}"),
        (HttpMethod::Delete, "/a/b"),
        (HttpMethod::Post, "/a/{x}.json"),
        (HttpMethod::Put, "/{*rest}"),
        (HttpMethod::Patch, "/c({k})/d"),
    ]);
    assert_eq!(
        t.allowed("/a/b"),
        [HttpMethod::Get, HttpMethod::Put, HttpMethod::Delete]
    );
    assert_eq!(
        t.allowed("/a/b.json"),
        [HttpMethod::Get, HttpMethod::Post, HttpMethod::Put]
    );
    assert_eq!(t.allowed("/c(1)/d"), [HttpMethod::Put, HttpMethod::Patch]);
    assert!(t.allowed("/").is_empty());
}

/// Fails when `f` takes longer than `limit` (generous: unoptimized CI).
fn within(limit: std::time::Duration, what: &str, f: impl FnOnce()) {
    let start = std::time::Instant::now();
    f();
    let took = start.elapsed();
    assert!(took < limit, "{what} took {took:?}");
}

#[test]
fn adversarial_segments_match_in_linear_time() {
    let limit = std::time::Duration::from_millis(500);
    // Many separators, then a mismatch at the end: backtracking over every
    // separator choice was exponential in the number of parameters.
    let t = gets(&["/e(k0={p0},k1={p1},k2={p2},k3={p3})"]);
    let mut req = String::from("/e(k0=x");
    for i in 1..4 {
        for _ in 0..1500 {
            req.push_str(&format!(",k{i}=x"));
        }
    }
    req.push('X');
    assert!(req.len() > 20_000);
    within(limit, "a 22 KB OData key", || {
        assert_eq!(found(&t, &req), None);
    });
    let closed = format!("{})", &req[..req.len() - 1]);
    within(limit, "a 22 KB OData key that matches", || {
        assert!(found(&t, &closed).is_some());
    });

    let t = gets(&["/v/{a}-{b}-{c}-{d}-{e}.x"]);
    for n in [400, 6400, 100_000] {
        let dashes = format!("/v/{}", "-".repeat(n));
        within(limit, "dashes without the suffix", || {
            assert_eq!(found(&t, &dashes), None);
        });
        let ok = format!("/v/{}.x", "a-".repeat(n));
        within(limit, "dashes with the suffix", || {
            assert!(found(&t, &ok).is_some());
        });
    }

    let routes: Vec<String> = (0..500).map(|i| format!("/{{a}}/b{i}/{{c}}")).collect();
    let routes: Vec<&str> = routes.iter().map(String::as_str).collect();
    let t = gets(&routes);
    let long = format!("/{}/zz/{}", "a".repeat(30_000), "c".repeat(30_000));
    within(limit, "500 routes and a long path", || {
        assert_eq!(found(&t, &long), None);
        assert!(t.allowed(&long).is_empty());
    });
}

mod compare;
