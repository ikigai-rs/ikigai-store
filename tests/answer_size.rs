//! The SIZE of a SPARQL answer is bounded, and past the bound it is REFUSED (ledger #970).
//!
//! The claim: unbudgeted, a 3-pattern any-BGP produced 1.3M rows in 0.57 s, and under a
//! 5 s time budget that is gigabytes of memory — the time budget bounds how long, not how
//! much. Reproduced against 0.2.8 by the first two tests here, which use only the API 0.2.8
//! already had: a two-pattern cross product over 400 quads (160,000 rows, a 64-byte query)
//! was ANSWERED there, in full, and is refused now.
//!
//! The rest pin the contract shared with `ikigai-sparql` (ledger #970): rows for SELECT,
//! triples for CONSTRUCT/DESCRIBE, serialized bytes for every form, ASK exempt; refused with
//! `InvalidArgument` on `query`, never truncated; a base for every caller, raised only by a
//! `urn:cap:store:answer:*` grant up to the ceiling (root gets the ceiling), and lowered only
//! by a request's inline `max_rows=` / `max_bytes=`. Every bound is proven exact with a
//! small injected one — at the bound answered, one past it refused — so no test here holds
//! more than a few tens of megabytes.

use futures::executor::block_on;
use ikigai_core::{ArgRef, Capability, ContentId, Error, Iri, Kernel, Request, Verb};
use ikigai_store::{space, DurableStore, CAP_READ};
use std::sync::Arc;

/// `n` quads in the default graph: `<urn:s{i}> <urn:p> "…{i}"`.
fn seed(kernel: &Kernel, n: usize, literal: usize) {
    let pad = "x".repeat(literal);
    let triples: String = (0..n)
        .map(|i| format!("<urn:s{i}> <urn:p> \"{pad}{i}\" . "))
        .collect();
    issue(
        kernel,
        &Capability::root(),
        Verb::Sink,
        "urn:iki:store:update",
        &[("content", inline(&format!("INSERT DATA {{ {triples} }}")))],
    )
    .expect("seeding");
}

fn kernel_over(store: DurableStore) -> Kernel {
    Kernel::with_meta_renderer(
        Arc::new(space(store)),
        Arc::new(ikigai_vocab::TurtleRenderer),
    )
}

fn inline(s: &str) -> ArgRef {
    ArgRef::Inline(s.as_bytes().to_vec())
}

fn issue(
    kernel: &Kernel,
    cap: &Capability,
    verb: Verb,
    iri: &str,
    args: &[(&str, ArgRef)],
) -> Result<Vec<u8>, Error> {
    let mut request = Request::new(verb, Iri::parse(iri).unwrap());
    for (name, value) in args {
        request = request.with_arg(*name, value.clone());
    }
    block_on(kernel.issue(request, cap)).map(|r| r.bytes.to_vec())
}

fn reader() -> Capability {
    Capability::scoped([CAP_READ])
}

/// Refused as `InvalidArgument` on `query`, and the refusal names `needle`.
fn refused(got: Result<Vec<u8>, Error>, needle: &str) {
    match got {
        Err(Error::InvalidArgument { name, detail }) if name == "query" => assert!(
            detail.contains(needle),
            "the refusal names `{needle}`: {detail}"
        ),
        Err(other) => {
            panic!("expected InvalidArgument on `query` naming `{needle}`, got {other:?}")
        }
        Ok(body) => panic!(
            "expected a refusal naming `{needle}`, got an ANSWER of {} bytes",
            body.len()
        ),
    }
}

/// The reproduction, rows: 160,000 one-column rows (~8 MB of JSON, under the byte bound) from
/// a 64-byte query, for a caller holding nothing but the read grant.
#[test]
fn a_cross_product_past_the_base_row_bound_is_refused_not_answered() {
    let kernel = kernel_over(DurableStore::in_memory().unwrap());
    seed(&kernel, 400, 0);
    let query = "SELECT ?a WHERE { ?a ?p ?o . ?b ?q ?c }";
    refused(
        issue(
            &kernel,
            &reader(),
            Verb::Source,
            "urn:iki:store:select",
            &[("query", inline(query))],
        ),
        "the answer exceeds 100000 rows; add LIMIT, narrow the query, or ask the host for more",
    );
    // …and the same rows under a LIMIT inside the bound are answered.
    let body = issue(
        &kernel,
        &reader(),
        Verb::Source,
        "urn:iki:store:select",
        &[("query", inline(&format!("{query} LIMIT 100000")))],
    )
    .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&body).matches("\"a\":").count(),
        100_000
    );
}

/// The reproduction, bytes: 90,000 rows of two ~200-byte literals (~36 MB of JSON), under the
/// row bound and over the 16 MiB byte bound.
#[test]
fn an_answer_past_the_base_byte_bound_is_refused_not_answered() {
    let kernel = kernel_over(DurableStore::in_memory().unwrap());
    seed(&kernel, 300, 200);
    refused(
        issue(
            &kernel,
            &reader(),
            Verb::Source,
            "urn:iki:store:select",
            &[(
                "query",
                inline("SELECT ?o ?c WHERE { ?a ?p ?o . ?b ?q ?c }"),
            )],
        ),
        "the answer exceeds 16777216 bytes; add LIMIT, narrow the query, or ask the host for more",
    );
}

/// The claim about `budget=` (ledger #964's CMS arc found `ikigai-sparql` IGNORES one passed
/// by reference and falls back to the ceiling, which bypasses a host that stamps "add if
/// missing"). Here it is refused by name in every unreadable form; this test pins that.
#[test]
fn a_budget_that_is_not_inline_is_refused_not_ignored() {
    let kernel = kernel_over(DurableStore::in_memory().unwrap());
    seed(&kernel, 3, 0);
    for (label, arg) in [
        (
            "by reference",
            ArgRef::Reference(Iri::parse("urn:example:elsewhere").unwrap()),
        ),
        ("by content id", ArgRef::Content(ContentId::of(b"1"))),
        ("not UTF-8", ArgRef::Inline(vec![0xff, 0xfe])),
    ] {
        match issue(
            &kernel,
            &reader(),
            Verb::Source,
            "urn:iki:store:select",
            &[
                ("query", inline("SELECT * WHERE { ?s ?p ?o }")),
                ("budget", arg),
            ],
        ) {
            Err(Error::InvalidArgument { name, .. }) if name == "budget" => {}
            other => panic!("budget {label}: expected a refusal naming `budget`, got {other:?}"),
        }
    }
}

// ---------------------------------------------------------------- the contract, exactly

use ikigai_store::budget::{
    cap_answer, cap_answer_bytes, AnswerBound, AnswerBudget, CEILING_MAX_ROWS,
};
use ikigai_store::{cap_read_graph, CAP_WRITE};

const G: &str = "urn:example:g";

/// A store whose base is `rows` / `bytes`, ceiling 1,000 rows / 1 MiB, seeded with 20 quads
/// in the default graph and the same 20 in `G`.
fn small(rows: u64, bytes: u64) -> Kernel {
    let store = DurableStore::in_memory().unwrap().with_answer_budget(
        AnswerBudget::new(AnswerBound::new(rows, bytes).unwrap())
            .with_ceiling(AnswerBound::new(1_000, 1 << 20).unwrap()),
    );
    let kernel = kernel_over(store);
    let triples: String = (0..20)
        .map(|i| format!("<urn:s{i}> <urn:p> \"v{i}\" . "))
        .collect();
    issue(
        &kernel,
        &Capability::root(),
        Verb::Sink,
        "urn:iki:store:update",
        &[(
            "content",
            inline(&format!(
                "INSERT DATA {{ {triples} GRAPH <{G}> {{ {triples} }} }}"
            )),
        )],
    )
    .expect("seeding");
    kernel
}

fn source(
    kernel: &Kernel,
    cap: &Capability,
    iri: &str,
    query: &str,
    extra: &[(&str, &str)],
) -> Result<Vec<u8>, Error> {
    let mut args = vec![("query", inline(query))];
    args.extend(extra.iter().map(|(k, v)| (*k, inline(v))));
    issue(kernel, cap, Verb::Source, iri, &args)
}

/// A door, the capability to read through it, a query, the serializations to try, how many
/// rows or triples the answer has, and what they are called.
type Case<'a> = (
    &'a str,
    &'a Capability,
    &'a str,
    &'a [&'a str],
    u64,
    &'a str,
);

/// Every door and every serialization, with `n` rows (or triples) in the answer: at a row
/// bound of `n` answered, at `n - 1` refused naming the bound.
#[test]
fn the_row_bound_is_exact_on_every_door_and_every_serialization() {
    let scoped = Capability::scoped([cap_read_graph(G)]);
    let cases: [Case; 6] = [
        (
            "urn:iki:store:select",
            &reader(),
            "SELECT * WHERE { ?s ?p ?o }",
            &[
                "application/sparql-results+json",
                "application/sparql-results+xml",
                "text/csv",
                "text/tab-separated-values",
            ],
            20,
            "rows",
        ),
        (
            "urn:iki:store:construct",
            &reader(),
            "CONSTRUCT WHERE { ?s ?p ?o }",
            &["application/n-triples", "text/turtle"],
            20,
            "triples",
        ),
        (
            "urn:iki:store:describe",
            &reader(),
            "DESCRIBE ?s WHERE { ?s ?p ?o }",
            &["application/n-triples", "text/turtle"],
            20,
            "triples",
        ),
        (
            "urn:iki:store:graph-select",
            &scoped,
            "SELECT * WHERE { ?s ?p ?o }",
            &["application/sparql-results+json"],
            20,
            "rows",
        ),
        (
            "urn:iki:store:graph-construct",
            &scoped,
            "CONSTRUCT WHERE { ?s ?p ?o }",
            &["application/n-triples"],
            20,
            "triples",
        ),
        (
            "urn:iki:store:graph-describe",
            &scoped,
            "DESCRIBE ?s WHERE { ?s ?p ?o }",
            &["text/turtle"],
            20,
            "triples",
        ),
    ];
    for (iri, cap, query, formats, n, unit) in cases {
        for format in formats {
            let graph = [("graph", G)];
            let scope: &[(&str, &str)] = if iri.contains("graph-") { &graph } else { &[] };
            // Through the base, injected small.
            let at = small(n, 1 << 20);
            let mut args = scope.to_vec();
            args.push(("as", format));
            source(&at, cap, iri, query, &args)
                .unwrap_or_else(|e| panic!("{iri} as {format} at the bound: {e}"));
            let under = small(n - 1, 1 << 20);
            refused(
                source(&under, cap, iri, query, &args),
                &format!("the answer exceeds {} {unit};", n - 1),
            );
            // Through `max_rows=`, which lowers the same way.
            let mut lowered = args.clone();
            lowered.push(("max_rows", "19"));
            let n_str = n.to_string();
            let mut exact = args.clone();
            exact.push(("max_rows", &n_str));
            source(&at, cap, iri, query, &exact).unwrap();
            refused(
                source(&at, cap, iri, query, &lowered),
                &format!("the answer exceeds 19 {unit};"),
            );
        }
    }
}

/// The byte bound is exact too: an answer of `b` bytes is answered at a bound of `b` and
/// refused at `b - 1`, on both families, through the base and through `max_bytes=`.
#[test]
fn the_byte_bound_is_exact() {
    for (iri, query) in [
        ("urn:iki:store:select", "SELECT * WHERE { ?s ?p ?o }"),
        ("urn:iki:store:construct", "CONSTRUCT WHERE { ?s ?p ?o }"),
    ] {
        let size = source(&small(1_000, 1 << 20), &reader(), iri, query, &[])
            .unwrap()
            .len() as u64;
        source(&small(1_000, size), &reader(), iri, query, &[]).unwrap();
        refused(
            source(&small(1_000, size - 1), &reader(), iri, query, &[]),
            &format!("the answer exceeds {} bytes;", size - 1),
        );
        let k = small(1_000, 1 << 20);
        let exact = size.to_string();
        let under = (size - 1).to_string();
        source(&k, &reader(), iri, query, &[("max_bytes", &exact)]).unwrap();
        refused(
            source(&k, &reader(), iri, query, &[("max_bytes", &under)]),
            &format!("the answer exceeds {} bytes;", size - 1),
        );
    }
}

/// ASK is exempt: its answer is one boolean, under either IRI that answers it.
#[test]
fn ask_is_exempt() {
    let k = small(1, 1);
    for iri in ["urn:iki:store:ask", "urn:iki:store:select"] {
        let body = source(&k, &reader(), iri, "ASK { ?s ?p ?o }", &[]).unwrap();
        assert!(String::from_utf8_lossy(&body).contains("true"), "{iri}");
    }
}

/// A grant raises each bound up to the ceiling and nothing else; root gets the ceiling; a
/// request cannot raise what its capability gets.
#[test]
fn grants_raise_the_bound_and_a_request_only_lowers_it() {
    let k = small(5, 1 << 20);
    let all = "SELECT * WHERE { ?s ?p ?o }"; // 20 rows
    refused(
        source(&k, &reader(), "urn:iki:store:select", all, &[]),
        "exceeds 5 rows",
    );
    // `max_rows=` above the base does not raise it.
    refused(
        source(
            &k,
            &reader(),
            "urn:iki:store:select",
            all,
            &[("max_rows", "1000")],
        ),
        "exceeds 5 rows",
    );
    let granted = Capability::scoped([CAP_READ.to_string(), cap_answer(20)]);
    source(&k, &granted, "urn:iki:store:select", all, &[]).unwrap();
    let not_enough = Capability::scoped([CAP_READ.to_string(), cap_answer(19)]);
    refused(
        source(&k, &not_enough, "urn:iki:store:select", all, &[]),
        "exceeds 19 rows",
    );
    // A grant past the ceiling is clamped to it, and root gets it: 1,000 here.
    let greedy = Capability::scoped([CAP_READ.to_string(), cap_answer(CEILING_MAX_ROWS)]);
    let thousand = "SELECT * WHERE { ?a ?p ?o . ?b ?q ?c }"; // 400 rows
    source(&k, &greedy, "urn:iki:store:select", thousand, &[]).unwrap();
    source(
        &k,
        &Capability::root(),
        "urn:iki:store:select",
        thousand,
        &[],
    )
    .unwrap();
    let cross3 = "SELECT * WHERE { ?a ?p ?o . ?b ?q ?c . ?d ?r ?e }"; // 8,000 rows
    refused(
        source(&k, &greedy, "urn:iki:store:select", cross3, &[]),
        "exceeds 1000 rows",
    );
    refused(
        source(&k, &Capability::root(), "urn:iki:store:select", cross3, &[]),
        "exceeds 1000 rows",
    );
    // A row grant raises rows and not bytes; a byte grant raises bytes.
    let tiny = small(1_000, 100);
    let rows_only = Capability::scoped([CAP_READ.to_string(), cap_answer(1_000)]);
    refused(
        source(&tiny, &rows_only, "urn:iki:store:select", all, &[]),
        "exceeds 100 bytes",
    );
    let bytes_too = Capability::scoped([CAP_READ.to_string(), cap_answer_bytes(1 << 20)]);
    source(&tiny, &bytes_too, "urn:iki:store:select", all, &[]).unwrap();
}

/// `max_rows` / `max_bytes` present but not a positive whole number, or not inline, are
/// refused by name — never ignored, which would answer under the ceiling instead.
#[test]
fn a_max_rows_or_max_bytes_that_is_not_a_positive_inline_number_is_refused() {
    let k = small(1_000, 1 << 20);
    let all = "SELECT * WHERE { ?s ?p ?o }";
    for arg in ["max_rows", "max_bytes"] {
        for bad in ["0", "-1", "ten", "1.5", ""] {
            match source(&k, &reader(), "urn:iki:store:select", all, &[(arg, bad)]) {
                Err(Error::InvalidArgument { name, .. }) if name == arg => {}
                other => panic!("{arg}={bad:?}: expected a refusal naming it, got {other:?}"),
            }
        }
        for (label, value) in [
            (
                "by reference",
                ArgRef::Reference(Iri::parse("urn:example:elsewhere").unwrap()),
            ),
            ("by content id", ArgRef::Content(ContentId::of(b"5"))),
            ("not UTF-8", ArgRef::Inline(vec![0xff, 0xfe])),
        ] {
            match issue(
                &k,
                &reader(),
                Verb::Source,
                "urn:iki:store:select",
                &[("query", inline(all)), (arg, value)],
            ) {
                Err(Error::InvalidArgument { name, .. }) if name == arg => {}
                other => panic!("{arg} {label}: expected a refusal naming it, got {other:?}"),
            }
        }
    }
}

/// The one ledger #970 measured: a three-pattern cross product (1.7 million rows over 120
/// quads, in a 55-byte query) is refused by its SIZE, long before its time budget — so the
/// rows never pile up in memory waiting for the deadline.
#[test]
fn the_cross_product_is_refused_by_size_before_time() {
    let kernel = kernel_over(DurableStore::in_memory().unwrap());
    seed(&kernel, 120, 0);
    let started = std::time::Instant::now();
    refused(
        issue(
            &kernel,
            &Capability::scoped([CAP_READ, CAP_WRITE]),
            Verb::Source,
            "urn:iki:store:select",
            &[(
                "query",
                inline("SELECT * WHERE { ?a ?b ?c . ?d ?e ?f . ?g ?h ?i }"),
            )],
        ),
        // Nine columns a row, so the byte bound is reached first (at ~60,000 rows).
        "the answer exceeds 16777216 bytes;",
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "refused at {:?}, which is the time budget's job, not the size bound's",
        started.elapsed()
    );
}
