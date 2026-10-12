//! A triple term an UPDATE grows, a request at a time, is bounded in depth before the update
//! commits (ledger #1333).
//!
//! The claim, from `ikigai-linkeddata` PR 41: `DELETE { … ?t } INSERT { … ?n } WHERE { … ?t
//! BIND(TRIPLE(<s>, <p>, …56 levels… ?t) AS ?n) }` wraps whatever triple term the store already
//! holds in 56 more levels, each request inside every text bound, and oxigraph encodes, decodes
//! and serializes a triple term RECURSIVELY. So the term grows until a request that touches it
//! overflows its thread, and a stack overflow aborts the whole process. `src/depth.rs` bounds a
//! LOADED document (ledger #992) and `src/limits.rs` bounds the update TEXT, but neither bounds
//! a term the update BUILDS from a stored one; both update doors had the hole.
//!
//! ★ Only in a build with RDF 1.2 on (`oxigraph/rdf-12`, which `ikigai-cli` gets from rudof by
//! feature unification, and which CI reaches through this crate's `rdf-12` gate). Without it
//! `TRIPLE` does not parse, nothing can build a triple term, and there is nothing to grow, so
//! the growth tests are cfg'd on the gate: the sound direction (the gate on implies the
//! variant exists).
//!
//! Every growth runs in a CHILD PROCESS (this test binary re-executed with one probe named in
//! its environment) on a 2 MiB thread, a tokio worker's size, so an abort kills the child and
//! reads here as a failure with the signal named.

use futures::executor::block_on;
use ikigai_core::{ArgRef, Capability, Iri, Kernel, Request, Verb};
use ikigai_store::{space, DurableStore};
use std::process::Command;
use std::sync::Arc;

const PROBE: &str = "IKIGAI_STORE_TRIPLE_GROWTH_PROBE";
const G: &str = "urn:example:g";
/// The bound, restated so this file reads on its own: `depth::MAX_RDF_NESTING`.
#[cfg_attr(not(feature = "rdf-12"), allow(dead_code))]
const BOUND: usize = 64;
/// Levels one request adds: inside the text nesting bound (`BIND(` and the braces around it
/// take the rest of `MAX_SPARQL_NESTING`), and the number linkeddata measured with.
const PER_REQUEST: usize = 56;
/// Levels one `<<( … )>>` template request adds: the most the text scan admits, since each
/// level keeps one more reading of `<` open (`MAX_SCAN_BRANCHES` refuses 7).
const TEMPLATE_PER_REQUEST: usize = 6;

/// `TRIPLE(<urn:s>, <urn:p>, … ?t …)`, `n` deep.
fn wrap(n: usize) -> String {
    format!(
        "{}?t{}",
        "TRIPLE(<urn:s>, <urn:p>, ".repeat(n),
        ")".repeat(n)
    )
}

/// One request that replaces the term at `<urn:a> <urn:p>` with itself wrapped `n` deeper.
fn grow(door: &str, n: usize) -> String {
    match door {
        "update" => format!(
            "DELETE {{ <urn:a> <urn:p> ?t }} INSERT {{ <urn:a> <urn:p> ?n }} \
             WHERE {{ <urn:a> <urn:p> ?t BIND({} AS ?n) }}",
            wrap(n)
        ),
        "graph-update" => format!(
            "DELETE {{ GRAPH <{G}> {{ <urn:a> <urn:p> ?t }} }} \
             INSERT {{ GRAPH <{G}> {{ <urn:a> <urn:p> ?n }} }} \
             WHERE {{ GRAPH <{G}> {{ <urn:a> <urn:p> ?t }} BIND({} AS ?n) }}",
            wrap(n)
        ),
        other => panic!("no door named {other}"),
    }
}

/// The same growth with the `<<( … )>>` template instead of `TRIPLE(…)`: the update's INSERT
/// builds the term, not its WHERE. Prefixed names inside the parentheses, so that only the
/// `<<(`s keep the text scan's readings of `<` open.
fn grow_by_template(door: &str, n: usize) -> String {
    let term = format!("{}?t{}", "<<( ex:s ex:p ".repeat(n), " )>>".repeat(n));
    match door {
        "update" => format!(
            "PREFIX ex: <urn:ex:> DELETE {{ <urn:a> <urn:p> ?t }} \
             INSERT {{ <urn:a> <urn:p> {term} }} WHERE {{ <urn:a> <urn:p> ?t }}"
        ),
        "graph-update" => format!(
            "PREFIX ex: <urn:ex:> DELETE {{ GRAPH <{G}> {{ <urn:a> <urn:p> ?t }} }} \
             INSERT {{ GRAPH <{G}> {{ <urn:a> <urn:p> {term} }} }} \
             WHERE {{ GRAPH <{G}> {{ <urn:a> <urn:p> ?t }} }}"
        ),
        other => panic!("no door named {other}"),
    }
}

fn seed(door: &str) -> String {
    match door {
        "update" => "INSERT DATA { <urn:a> <urn:p> <urn:o> }".to_string(),
        _ => format!("INSERT DATA {{ GRAPH <{G}> {{ <urn:a> <urn:p> <urn:o> }} }}"),
    }
}

fn kernel() -> Kernel {
    Kernel::new(Arc::new(space(DurableStore::in_memory().unwrap())))
}

fn update(kernel: &Kernel, door: &str, text: &str) -> ikigai_core::Result<String> {
    let mut req = Request::new(
        Verb::Sink,
        Iri::parse(format!("urn:iki:store:{door}")).unwrap(),
    )
    .with_arg("content", ArgRef::Inline(text.as_bytes().to_vec()));
    if door == "graph-update" {
        req = req.with_arg("graph", ArgRef::Inline(G.as_bytes().to_vec()));
    }
    block_on(kernel.issue(req, &Capability::root()))
        .map(|rep| String::from_utf8_lossy(&rep.bytes).into_owned())
}

fn query(kernel: &Kernel, form: &str, text: &str) -> ikigai_core::Result<String> {
    let req = Request::new(
        Verb::Source,
        Iri::parse(format!("urn:iki:store:{form}")).unwrap(),
    )
    .with_arg("query", ArgRef::Inline(text.as_bytes().to_vec()));
    block_on(kernel.issue(req, &Capability::root()))
        .map(|rep| String::from_utf8_lossy(&rep.bytes).into_owned())
}

/// How deep the term at `<urn:a> <urn:p>` nests, read back through CONSTRUCT (N-Triples
/// writes one `<<(` per level).
fn depth(kernel: &Kernel) -> usize {
    let text = query(
        kernel,
        "construct",
        "CONSTRUCT { <urn:a> <urn:p> ?t } WHERE { { <urn:a> <urn:p> ?t } UNION { GRAPH ?g { <urn:a> <urn:p> ?t } } }",
    )
    .unwrap();
    text.matches("<<(").count()
}

/// The child's half: inert unless a parent named a probe (`door:style:rounds`). Seeds the
/// store, grows the term `rounds` times, then reads it through all four query forms, on a
/// 2 MiB thread; prints what happened and exits before the harness can.
#[test]
fn probe_child() {
    let Ok(spec) = std::env::var(PROBE) else {
        return;
    };
    let mut parts = spec.split(':');
    let door = parts.next().unwrap().to_string();
    let style = parts.next().unwrap().to_string();
    let rounds: usize = parts.next().unwrap().parse().unwrap();
    let outcome = std::thread::Builder::new()
        .stack_size(2 << 20)
        .spawn(move || {
            let k = kernel();
            update(&k, &door, &seed(&door)).unwrap();
            let (mut applied, mut refused, mut first) = (0, 0, String::new());
            for _ in 0..rounds {
                let text = match style.as_str() {
                    "bind" => grow(&door, PER_REQUEST),
                    _ => grow_by_template(&door, TEMPLATE_PER_REQUEST),
                };
                match update(&k, &door, &text) {
                    Ok(_) => applied += 1,
                    Err(e) => {
                        if first.is_empty() {
                            first = e.to_string().replace('\n', " ");
                        }
                        refused += 1;
                    }
                }
            }
            let mut forms = Vec::new();
            for (form, text) in [
                ("select", "SELECT ?t WHERE { { <urn:a> <urn:p> ?t } UNION { GRAPH ?g { <urn:a> <urn:p> ?t } } }"),
                ("ask", "ASK { { <urn:a> <urn:p> ?t } UNION { GRAPH ?g { <urn:a> <urn:p> ?t } } }"),
                ("construct", "CONSTRUCT { <urn:a> <urn:p> ?t } WHERE { { <urn:a> <urn:p> ?t } UNION { GRAPH ?g { <urn:a> <urn:p> ?t } } }"),
                ("describe", "DESCRIBE <urn:a>"),
            ] {
                forms.push(format!("{form}={}", query(&k, form, text).is_ok()));
            }
            format!(
                "applied={applied} refused={refused} depth={} reads={} first={first}",
                depth(&k),
                forms.join(",")
            )
        })
        .unwrap()
        .join()
        .unwrap();
    println!("\nOUTCOME {outcome}");
    std::process::exit(0);
}

/// The parent's half: run one probe in a child and return what it said, or fail naming how
/// it died: an abort is the defect.
#[cfg_attr(not(feature = "rdf-12"), allow(dead_code))]
fn probe(door: &str, style: &str, rounds: usize) -> String {
    let out = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "probe_child", "--nocapture", "--test-threads=1"])
        .env(PROBE, format!("{door}:{style}:{rounds}"))
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let overflow = stderr.lines().find(|l| l.contains("overflowed its stack"));
    assert!(
        out.status.success(),
        "growing through `{door}` ({style}) for {rounds} rounds aborted the child ({}): {}",
        out.status,
        overflow.unwrap_or(&stderr)
    );
    let at = stdout
        .find("\nOUTCOME ")
        .unwrap_or_else(|| panic!("the `{door}` probe reported nothing: {stdout}"));
    stdout[at + "\nOUTCOME ".len()..]
        .lines()
        .next()
        .unwrap_or("")
        .to_string()
}

// ------------------------------------------------------------------ the reproduction

/// The claim, at both update doors, both ways of building the term, 300 requests each (16,800
/// levels by `TRIPLE`, 1,800 by template, if nothing stops them). Against 0.2.18 (debug) the
/// child ABORTED on the store's writer thread at about 11,800 levels, by either constructor
/// (`TRIPLE` on the 211th request; the template, six levels a request, on about the 1,968th,
/// in a longer run). Now each
/// request commits while the term stays inside the bound, every later one is refused before it
/// commits, and the term the store keeps reads back through all four forms.
#[cfg(feature = "rdf-12")]
#[test]
fn a_triple_term_grown_across_updates_is_refused_past_the_bound_and_aborts_nothing() {
    let rounds = 300;
    for door in ["update", "graph-update"] {
        for (style, per) in [("bind", PER_REQUEST), ("template", TEMPLATE_PER_REQUEST)] {
            let applied = BOUND / per;
            let outcome = probe(door, style, rounds);
            assert!(
                outcome.starts_with(&format!(
                    "applied={applied} refused={} depth={} \
                     reads=select=true,ask=true,construct=true,describe=true \
                     first=invalid argument `content`",
                    rounds - applied,
                    applied * per
                )) && outcome.contains("MAX_RDF_NESTING"),
                "{door} ({style}): {outcome}"
            );
        }
    }
}

// ------------------------------------------------------------------ at and over the bound

/// An update that leaves the term exactly at the bound commits; one level more is refused,
/// and nothing of it is applied, its `DELETE` half included.
#[cfg(feature = "rdf-12")]
#[test]
fn at_the_bound_an_update_commits_and_one_level_past_applies_nothing() {
    for door in ["update", "graph-update"] {
        at_the_bound(&kernel(), door);
    }
}

#[cfg(feature = "rdf-12")]
fn at_the_bound(k: &Kernel, door: &str) {
    update(k, door, &seed(door)).unwrap();
    update(k, door, &grow(door, BOUND - PER_REQUEST)).unwrap();
    update(k, door, &grow(door, PER_REQUEST)).unwrap_or_else(|e| panic!("{door}: {e}"));
    assert_eq!(depth(k), BOUND, "{door}");
    let err = update(k, door, &grow(door, 1)).unwrap_err().to_string();
    assert!(
        err.starts_with("invalid argument `content`") && err.contains("MAX_RDF_NESTING"),
        "{door}: {err}"
    );
    assert_eq!(
        depth(k),
        BOUND,
        "{door}: the refused update changed the store"
    );
}

/// The check reads only where the update's templates reach, and passes over a quad the store
/// held before the update: a deep term written before the check (here through the host's own
/// handle, which the doors cannot see) does not make every later triple-building update fail.
/// An update that adds a NEW deep quad is refused all the same, by copying the old one.
#[cfg(feature = "rdf-12")]
#[test]
fn a_deep_term_already_stored_is_not_this_updates_doing() {
    let (store, handle) = DurableStore::in_memory_shared().unwrap();
    deep_already_stored(store, handle);
}

#[cfg(feature = "rdf-12")]
fn deep_already_stored(store: DurableStore, handle: Arc<ikigai_store::Store>) {
    let k = Kernel::new(Arc::new(space(store)));
    let deep = format!(
        "INSERT DATA {{ <urn:old> <urn:p> {}<urn:o>{} . GRAPH <{G}> {{ <urn:old> <urn:p> {}<urn:o>{} }} }}",
        "<<( <urn:s> <urn:p> ".repeat(BOUND + 10),
        " )>>".repeat(BOUND + 10),
        "<<( <urn:s> <urn:p> ".repeat(BOUND + 10),
        " )>>".repeat(BOUND + 10),
    );
    // On a thread of its own: the host's own parse of this text is not this test's subject.
    std::thread::Builder::new()
        .stack_size(64 << 20)
        .spawn(move || handle.update(&deep).unwrap())
        .unwrap()
        .join()
        .unwrap();
    for door in ["update", "graph-update"] {
        update(&k, door, &seed(door)).unwrap();
        // A template over every subject reaches the old quad too, and passes it over.
        let any_subject = match door {
            "update" => "DELETE { ?a <urn:p> ?t } INSERT { ?a <urn:p> ?n } \
                         WHERE { VALUES ?a { <urn:a> } ?a <urn:p> ?t \
                         BIND(TRIPLE(<urn:s>, <urn:p>, ?t) AS ?n) }"
                .to_string(),
            _ => format!(
                "DELETE {{ GRAPH <{G}> {{ ?a <urn:p> ?t }} }} \
                 INSERT {{ GRAPH <{G}> {{ ?a <urn:p> ?n }} }} \
                 WHERE {{ VALUES ?a {{ <urn:a> }} GRAPH <{G}> {{ ?a <urn:p> ?t }} \
                 BIND(TRIPLE(<urn:s>, <urn:p>, ?t) AS ?n) }}"
            ),
        };
        update(&k, door, &any_subject).unwrap_or_else(|e| panic!("{door}: {e}"));
        // Copying the old term to a new quad adds a deep quad: refused.
        let copy = match door {
            "update" => "INSERT { <urn:new> <urn:p> ?t } WHERE { <urn:old> <urn:p> ?t } # triple"
                .to_string(),
            _ => format!(
                "INSERT {{ GRAPH <{G}> {{ <urn:new> <urn:p> ?t }} }} \
                 WHERE {{ GRAPH <{G}> {{ <urn:old> <urn:p> ?t }} }}"
            ),
        };
        let err = update(&k, door, &copy).unwrap_err().to_string();
        assert!(err.contains("MAX_RDF_NESTING"), "{door}: {err}");
    }
}

// ------------------------------------------------------------------ on RocksDB

/// The same two, on the durable backend: the broad door's check reads the transaction and the
/// committed store side by side, which is a different mechanism on RocksDB than in memory.
#[cfg(all(feature = "rdf-12", feature = "persistent"))]
#[test]
fn on_rocksdb_the_bound_holds_and_an_old_deep_term_is_passed_over() {
    let path = std::env::temp_dir().join(format!(
        "ikigai-store-triple-growth-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&path);
    {
        let (store, handle) = DurableStore::open_shared(&path).unwrap();
        deep_already_stored(store, handle);
    }
    let _ = std::fs::remove_dir_all(&path);
    for door in ["update", "graph-update"] {
        {
            let k = Kernel::new(Arc::new(space(DurableStore::open(&path).unwrap())));
            at_the_bound(&k, door);
        }
        let _ = std::fs::remove_dir_all(&path);
    }
}
