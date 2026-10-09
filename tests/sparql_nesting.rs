//! A SPARQL text that would overflow the stack is refused, or run on a stack big enough for
//! it — at EVERY door that parses caller text (ledger #915).
//!
//! The claim: `SELECT * WHERE { FILTER(((…3000 parentheses…1…))) }` aborted a host
//! (`tokio-rt-worker has overflowed its stack`), because oxigraph's parser recurses once per
//! level and a stack overflow aborts the whole process, on any thread. Every reproduction
//! here therefore runs in a CHILD PROCESS — this test binary re-executed with one probe
//! named in its environment — on a 2 MiB thread, the size of a tokio worker's. The parent
//! asserts on the child's exit: an abort kills the child, never this binary, and reads as a
//! failure with the signal named.
//!
//! These tests use only the API that predates the fix, so they compile — and fail, with
//! the child aborted — against 0.2.6.

use futures::executor::block_on;
use ikigai_core::{ArgRef, Capability, Iri, Kernel, Request, Verb};
use ikigai_store::{space, DurableStore};
use std::process::Command;
use std::sync::Arc;

const PROBE: &str = "IKIGAI_STORE_NESTING_PROBE";
const G: &str = "urn:example:g";
/// The bound, restated so these tests compile against 0.2.6, which has no constant to
/// name. A unit test in `src/limits.rs` pins `MAX_SPARQL_NESTING` to the same number.
const BOUND: usize = 64;

fn parens(n: usize) -> String {
    format!(
        "SELECT * WHERE {{ FILTER({}1{}) }}",
        "(".repeat(n),
        ")".repeat(n)
    )
}

/// Writes into `G`, so the scoped door accepts it too; `INSERT {…}` closes before the
/// `WHERE`, so this nests exactly as deep as [`parens`].
fn update_parens(n: usize) -> String {
    format!(
        "INSERT {{ GRAPH <{G}> {{ <urn:s> <urn:p> <urn:o> }} }} WHERE {{ FILTER({}1{}) }}",
        "(".repeat(n),
        ")".repeat(n)
    )
}

/// One request, as `(verb, iri, args)`, named by a probe so a child can rebuild it.
fn request(case: &str, n: usize) -> (Verb, String, Vec<(&'static str, String)>) {
    let scoped = |form: &str| {
        (
            Verb::Source,
            format!("urn:iki:store:graph-{form}"),
            vec![("query", parens(n)), ("graph", G.to_string())],
        )
    };
    let broad = |form: &str| {
        (
            Verb::Source,
            format!("urn:iki:store:{form}"),
            vec![("query", parens(n))],
        )
    };
    match case {
        "select" | "ask" | "construct" | "describe" => broad(case),
        "graph-select" | "graph-ask" | "graph-construct" | "graph-describe" => {
            scoped(case.trim_start_matches("graph-"))
        }
        "update" => (
            Verb::Sink,
            "urn:iki:store:update".to_string(),
            vec![("content", update_parens(n))],
        ),
        "graph-update" => (
            Verb::Sink,
            "urn:iki:store:graph-update".to_string(),
            vec![("content", update_parens(n)), ("graph", G.to_string())],
        ),
        // Not nesting: a FLAT `||` chain, which the evaluator still recurses over once per
        // term. 600 terms overflow a 2 MiB thread in a debug build (~300 do), and are fine
        // on the stack the query is run on.
        "or-chain" => (
            Verb::Source,
            "urn:iki:store:select".to_string(),
            vec![(
                "query",
                format!(
                    "SELECT * WHERE {{ ?s ?p ?o FILTER(false{}) }}",
                    "||false".repeat(n)
                ),
            )],
        ),
        // The same chain inside an update's WHERE.
        "update-or-chain" => (
            Verb::Sink,
            "urn:iki:store:update".to_string(),
            vec![(
                "content",
                format!(
                    "INSERT {{ <urn:s> <urn:p> <urn:o> }} WHERE {{ ?s ?p ?o FILTER(false{}) }}",
                    "||false".repeat(n)
                ),
            )],
        ),
        // The cheapest recursion per byte that brackets do not cover: one `!` is one level
        // (~400 bytes of stack each in a release build), and a `+1` term another.
        "bang-chain" => (
            Verb::Source,
            "urn:iki:store:select".to_string(),
            vec![(
                "query",
                format!("SELECT * WHERE {{ FILTER({}true) }}", "!".repeat(n)),
            )],
        ),
        "or1-chain" => (
            Verb::Source,
            "urn:iki:store:select".to_string(),
            vec![(
                "query",
                format!("SELECT * WHERE {{ FILTER(1{}) }}", "||1".repeat(n)),
            )],
        ),
        "plus-chain" => (
            Verb::Source,
            "urn:iki:store:select".to_string(),
            vec![(
                "query",
                format!("SELECT * WHERE {{ FILTER(1{} > 0) }}", "+1".repeat(n)),
            )],
        ),
        // Not SPARQL at all, and here as evidence: oxttl's Turtle parser keeps its own stack,
        // so a deeply nested document does not recurse.
        "load-bnodes" => (
            Verb::Sink,
            "urn:iki:store:load".to_string(),
            vec![(
                "content",
                format!(
                    "<urn:s> <urn:p> {}<urn:o>{} .",
                    "[ <urn:p> ".repeat(n),
                    " ]".repeat(n)
                ),
            )],
        ),
        "load-collections" => (
            Verb::Sink,
            "urn:iki:store:load".to_string(),
            vec![(
                "content",
                format!("<urn:s> <urn:p> {}{} .", "(".repeat(n), ")".repeat(n)),
            )],
        ),
        // `bindings=` is JSON, and serde_json refuses past 128 levels on its own.
        "bindings" => (
            Verb::Source,
            "urn:iki:store:select".to_string(),
            vec![
                ("query", "SELECT ?s WHERE { ?s ?p ?o }".to_string()),
                ("bindings", format!("{}{}", "[".repeat(n), "]".repeat(n))),
            ],
        ),
        other => panic!("no probe named {other}"),
    }
}

fn issue(kernel: &Kernel, case: &str, n: usize) -> ikigai_core::Result<String> {
    let (verb, iri, args) = request(case, n);
    let mut req = Request::new(verb, Iri::parse(&iri).unwrap());
    for (name, value) in args {
        req = req.with_arg(name, ArgRef::Inline(value.into_bytes()));
    }
    block_on(kernel.issue(req, &Capability::root()))
        .map(|rep| String::from_utf8_lossy(&rep.bytes).into_owned())
}

fn kernel() -> Kernel {
    let kernel = Kernel::new(Arc::new(space(DurableStore::in_memory().unwrap())));
    // One quad in `G` and one in the default graph, so a query that runs has rows to see.
    let seed = format!(
        "INSERT DATA {{ <urn:s> <urn:p> <urn:o> . GRAPH <{G}> {{ <urn:s> <urn:p> <urn:o> }} }}"
    );
    block_on(
        kernel.issue(
            Request::new(Verb::Sink, Iri::parse("urn:iki:store:update").unwrap())
                .with_arg("content", ArgRef::Inline(seed.into_bytes())),
            &Capability::root(),
        ),
    )
    .expect("seeding");
    kernel
}

/// The child's half: inert unless a parent named a probe. Runs it on a 2 MiB thread —
/// a tokio worker's stack — prints the outcome, and exits before the harness can.
#[test]
fn probe_child() {
    let Ok(spec) = std::env::var(PROBE) else {
        return;
    };
    let (case, n) = spec.split_once(':').unwrap();
    let (case, n) = (case.to_string(), n.parse::<usize>().unwrap());
    let outcome = std::thread::Builder::new()
        .stack_size(2 << 20)
        .spawn(move || match issue(&kernel(), &case, n) {
            Ok(text) => format!("ok {}", text.replace('\n', " ")),
            Err(e) => format!("err {}", e.to_string().replace('\n', " ")),
        })
        .unwrap()
        .join()
        .unwrap();
    println!("\nOUTCOME {outcome}");
    std::process::exit(0);
}

/// The parent's half: run one probe in a child and return what it said, or fail naming how
/// it died — an abort is the defect.
fn probe(case: &str, n: usize) -> String {
    let out = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "probe_child", "--nocapture", "--test-threads=1"])
        .env(PROBE, format!("{case}:{n}"))
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "the `{case}` probe at {n} did not survive a 2 MiB thread: {} — {}",
        out.status,
        stderr
            .lines()
            .find(|l| l.contains("overflow"))
            .unwrap_or(&stderr)
    );
    let at = stdout
        .find("\nOUTCOME ")
        .unwrap_or_else(|| panic!("the `{case}` probe reported nothing: {stdout}"));
    stdout[at + "\nOUTCOME ".len()..]
        .lines()
        .next()
        .unwrap_or("")
        .to_string()
}

const QUERY_DOORS: [&str; 8] = [
    "select",
    "ask",
    "construct",
    "describe",
    "graph-select",
    "graph-ask",
    "graph-construct",
    "graph-describe",
];

// ------------------------------------------------------------------ the reproduction

#[test]
fn three_thousand_parentheses_are_refused_at_every_query_door_and_abort_nothing() {
    for door in QUERY_DOORS {
        let outcome = probe(door, 3000);
        assert!(
            outcome.starts_with("err invalid argument `query`")
                && outcome.contains("deeper than 64"),
            "{door}: {outcome}"
        );
    }
}

#[test]
fn three_thousand_parentheses_are_refused_at_both_update_doors_and_abort_nothing() {
    for door in ["update", "graph-update"] {
        let outcome = probe(door, 3000);
        assert!(
            outcome.starts_with("err invalid argument `content`")
                && outcome.contains("deeper than 64"),
            "{door}: {outcome}"
        );
    }
}

#[test]
fn a_long_flat_chain_runs_on_the_stack_the_query_is_given() {
    // Inline on a 2 MiB thread this aborts a debug build; it is not nesting, so nothing
    // refuses it, and it is a shape a generated query may really have. It runs.
    let outcome = probe("or-chain", 600);
    assert!(outcome.starts_with("ok "), "{outcome}");
    let outcome = probe("update-or-chain", 600);
    assert!(outcome.starts_with("ok "), "{outcome}");
}

#[test]
fn a_run_of_not_is_nesting_and_is_refused_at_any_length() {
    // `!!!…` nests one level per byte, the cheapest recursion there is; it is counted with
    // the brackets, so even a run as long as the byte bound allows is refused, not parsed.
    for n in [3000, (1 << 20) - 64] {
        let outcome = probe("bang-chain", n);
        assert!(
            outcome.starts_with("err invalid argument `query`")
                && outcome.contains("deeper than 64"),
            "{n}: {outcome}"
        );
    }
}

#[test]
fn the_load_and_bindings_parsers_do_not_recurse_on_nesting() {
    for case in ["load-bnodes", "load-collections"] {
        let outcome = probe(case, 100_000);
        assert!(outcome.starts_with("ok "), "{case}: {outcome}");
    }
    let outcome = probe("bindings", 100_000);
    assert!(
        outcome.starts_with("err invalid argument `bindings`"),
        "{outcome}"
    );
}

// ------------------------------------------------------------------ at and under the bound

#[test]
fn a_query_at_the_bound_parses_and_runs_at_every_door() {
    // `SELECT * WHERE { FILTER(` is two levels; the rest brings it to exactly the bound.
    let k = kernel();
    for door in QUERY_DOORS {
        let result = issue(&k, door, BOUND - 2);
        match door {
            // A SELECT under a graph-shaped IRI is refused for its FORM, after parsing —
            // which is what this asserts: it got past the bound and through the parser.
            "construct" | "describe" | "graph-construct" | "graph-describe" => {
                let err = result.unwrap_err().to_string();
                assert!(err.contains("answers with"), "{door}: {err}");
            }
            _ => assert!(result.is_ok(), "{door}: {:?}", result.err()),
        }
        let over = issue(&k, door, BOUND - 1).unwrap_err().to_string();
        assert!(over.contains("deeper than 64"), "{door}: {over}");
    }
    for door in ["update", "graph-update"] {
        issue(&k, door, BOUND - 2).unwrap_or_else(|e| panic!("{door}: {e}"));
        let over = issue(&k, door, BOUND - 1).unwrap_err().to_string();
        assert!(over.contains("deeper than 64"), "{door}: {over}");
    }
}

#[test]
fn the_costliest_bracket_at_the_bound_runs() {
    // A function call costs the parser the most stack per level (~60 KiB, unoptimized).
    let k = kernel();
    let calls = format!(
        "SELECT * WHERE {{ ?s ?p ?o FILTER({}?o{}) }}",
        "STR(".repeat(BOUND - 2),
        ")".repeat(BOUND - 2)
    );
    let mut req = Request::new(Verb::Source, Iri::parse("urn:iki:store:select").unwrap());
    req = req.with_arg("query", ArgRef::Inline(calls.into_bytes()));
    block_on(k.issue(req, &Capability::root())).unwrap();
}

#[test]
fn brackets_in_strings_iris_and_comments_are_not_nesting() {
    let k = kernel();
    let deep = "(".repeat(500);
    let query = format!(
        "SELECT * WHERE {{ ?s ?p ?o # {deep}\n\
         FILTER(?o != \"{deep}\" && ?o != '''{deep}''' && ?o != <urn:x:{deep}>) }}"
    );
    let mut req = Request::new(Verb::Source, Iri::parse("urn:iki:store:select").unwrap());
    req = req.with_arg("query", ArgRef::Inline(query.into_bytes()));
    let rows = block_on(k.issue(req, &Capability::root())).unwrap();
    assert!(String::from_utf8_lossy(&rows.bytes).contains("urn:o"));
}

#[test]
fn a_less_than_that_hides_a_string_from_an_iri_reading_is_still_counted() {
    // `( ?a<'> ) ' && (…`: skip `<'>` as an IRI and the string the PARSER opens at that `'`
    // is invisible, so a naive scan sees one level where the parser sees one per repetition.
    let k = kernel();
    let query = format!(
        "SELECT * WHERE {{ ?a ?p ?o FILTER({}true) }}",
        "( ?a<'> ) ' && ".repeat(200)
    );
    let mut req = Request::new(Verb::Source, Iri::parse("urn:iki:store:select").unwrap());
    req = req.with_arg("query", ArgRef::Inline(query.into_bytes()));
    let err = block_on(k.issue(req, &Capability::root())).unwrap_err();
    assert!(err.to_string().contains("deeper than 64"), "{err}");
}

#[test]
fn a_query_past_the_byte_bound_is_refused_by_name() {
    let k = kernel();
    let values = "<urn:x> ".repeat((1 << 20) / 8 + 1);
    let query = format!("SELECT * WHERE {{ VALUES ?x {{ {values} }} }}");
    let mut req = Request::new(Verb::Source, Iri::parse("urn:iki:store:select").unwrap());
    req = req.with_arg("query", ArgRef::Inline(query.into_bytes()));
    let err = block_on(k.issue(req, &Capability::root()))
        .unwrap_err()
        .to_string();
    assert!(err.contains("`query`") && err.contains("1048576"), "{err}");
}

/// RELEASE BUILDS ONLY, and slow (~30 s): a chain the bracket bound does not cover, far
/// past what a 2 MiB thread holds (~2,000 terms), runs on the stack `on_sparql_stack` sizes
/// for it. Not run at the byte bound itself only because the evaluator is QUADRATIC in a
/// chain's length — 40,000 terms take ~30 s, 80,000 more than two minutes — which is a CPU
/// cost this crate does not bound (see the README). A debug build spends ~20-50x the stack
/// per level, which `src/limits.rs` states as the limit of the guarantee.
///
///     cargo test --release --test sparql_nesting -- --ignored --nocapture
#[test]
#[ignore]
fn a_chain_twenty_times_what_a_worker_stack_holds_runs() {
    let outcome = probe("or1-chain", 40_000);
    assert!(outcome.starts_with("ok "), "{outcome}");
}
