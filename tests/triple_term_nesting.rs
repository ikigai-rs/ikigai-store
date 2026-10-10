//! An RDF document that nests RDF 1.2 triple terms deeply is refused before it is parsed, at
//! every door that parses one (ledger #992).
//!
//! The claim: `<<( … )>>` nested ~3000 deep aborted a host through `ikigai-shacl`, because
//! oxrdf copies (and drops) a nested triple term recursively, once per level, inside the
//! parser, and a stack overflow aborts the whole process on any thread. `urn:iki:store:load`
//! parses caller documents with the same parsers, inline on the caller's thread, under the
//! store's write lock — so the same document aborted every tenant of the host with it.
//!
//! ★ Only in a build with RDF 1.2 on. That is `oxigraph/rdf-12`, which `ikigai-cli` gets by
//! feature UNIFICATION (rudof, through `ikigai-shacl`, enables it), never by asking this
//! crate. Here it is this crate's own `rdf-12` feature (`--features rdf-12`, or CI's
//! `features: "*"`), which enables exactly `oxigraph/rdf-12` — the same unified feature set.
//! Only the test that needs the variant (a document AT the bound loads) is cfg'd on it, the
//! sound direction: this feature on implies the variant exists. The REFUSAL is not cfg'd on
//! anything, because a consumer reaching RDF 1.2 through rudof does not enable this crate's
//! feature — so the refusal tests run in every build, and are the reproduction in one with it.
//!
//! Every reproduction runs in a CHILD PROCESS — this test binary re-executed with one probe
//! named in its environment — on a 2 MiB thread, a tokio worker's size, so an abort kills
//! the child and reads here as a failure with the signal named.

use futures::executor::block_on;
use ikigai_core::{ArgRef, Capability, Iri, Kernel, Request, Verb};
use ikigai_store::{space, DurableStore};
use std::process::Command;
use std::sync::Arc;

const PROBE: &str = "IKIGAI_STORE_TRIPLE_TERM_PROBE";
/// The bound, restated so this file reads on its own. A unit test in `src/limits.rs` pins
/// `MAX_RDF_NESTING` to the same number.
const BOUND: usize = 64;

/// `<<( <urn:s> <urn:p> <<( … <urn:o> … )>> )>>`, `n` deep: one triple-term object per level.
fn terms(n: usize) -> String {
    format!(
        "{}<urn:o>{}",
        "<<( <urn:s> <urn:p> ".repeat(n),
        " )>>".repeat(n)
    )
}

/// The same nesting in RDF/XML: a `rdf:parseType="Triple"` property element holds one node
/// element, whose property is the next level down.
fn rdfxml(n: usize) -> String {
    format!(
        "<?xml version=\"1.0\"?>\n\
         <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\" \
                  xmlns:ex=\"urn:ex:\" rdf:version=\"1.2\">\
         <rdf:Description rdf:about=\"urn:s\">{}<ex:p rdf:resource=\"urn:o\"/>{}</rdf:Description>\
         </rdf:RDF>",
        "<ex:p rdf:parseType=\"Triple\"><rdf:Description rdf:about=\"urn:s\">".repeat(n),
        "</rdf:Description></ex:p>".repeat(n),
    )
}

/// One `urn:iki:store:load` of a document `n` deep, in the syntax `case` names.
fn document(case: &str, n: usize) -> (String, &'static str) {
    match case {
        "turtle" => (format!("<urn:s> <urn:p> {} .", terms(n)), "text/turtle"),
        "trig" => (
            format!("GRAPH <urn:g> {{ <urn:s> <urn:p> {} }}", terms(n)),
            "application/trig",
        ),
        "ntriples" => (
            format!("<urn:s> <urn:p> {} .\n", terms(n)),
            "application/n-triples",
        ),
        "nquads" => (
            format!("<urn:s> <urn:p> {} <urn:g> .\n", terms(n)),
            "application/n-quads",
        ),
        "rdfxml" => (rdfxml(n), "application/rdf+xml"),
        other => panic!("no probe named {other}"),
    }
}

const FORMATS: [&str; 5] = ["turtle", "trig", "ntriples", "nquads", "rdfxml"];

fn load(kernel: &Kernel, case: &str, n: usize) -> ikigai_core::Result<String> {
    let (content, format) = document(case, n);
    let req = Request::new(Verb::Sink, Iri::parse("urn:iki:store:load").unwrap())
        .with_arg("content", ArgRef::Inline(content.into_bytes()))
        .with_arg("format", ArgRef::Inline(format.as_bytes().to_vec()));
    block_on(kernel.issue(req, &Capability::root()))
        .map(|rep| String::from_utf8_lossy(&rep.bytes).into_owned())
}

fn update(kernel: &Kernel, iri: &str, content: &str) -> ikigai_core::Result<String> {
    let mut req = Request::new(Verb::Sink, Iri::parse(iri).unwrap())
        .with_arg("content", ArgRef::Inline(content.as_bytes().to_vec()));
    if iri.ends_with("graph-update") {
        req = req.with_arg("graph", ArgRef::Inline(b"urn:g".to_vec()));
    }
    block_on(kernel.issue(req, &Capability::root()))
        .map(|rep| String::from_utf8_lossy(&rep.bytes).into_owned())
}

fn kernel() -> Kernel {
    Kernel::new(Arc::new(space(DurableStore::in_memory().unwrap())))
}

/// The child's half: inert unless a parent named a probe. Runs it on a 2 MiB thread, prints
/// the outcome, and exits before the harness can.
#[test]
fn probe_child() {
    let Ok(spec) = std::env::var(PROBE) else {
        return;
    };
    let (case, n) = spec.split_once(':').unwrap();
    let (case, n) = (case.to_string(), n.parse::<usize>().unwrap());
    let outcome = std::thread::Builder::new()
        .stack_size(2 << 20)
        .spawn(move || match load(&kernel(), &case, n) {
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
    let overflow = stderr.lines().find(|l| l.contains("overflowed its stack"));
    assert!(
        out.status.success(),
        "the `{case}` probe at {n} aborted the child ({}): {}",
        out.status,
        overflow.unwrap_or(&stderr)
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

// ------------------------------------------------------------------ the reproduction

/// The claim, in every syntax the door loads: 3000 levels aborted the host with RDF 1.2 on. The
/// refusal holds in every build, so this runs in every build; it is the reproduction only in one
/// with the feature (the child aborted against 0.2.9 with `--features rdf-12`).
#[test]
fn three_thousand_nested_triple_terms_are_refused_in_every_syntax_and_abort_nothing() {
    for case in FORMATS {
        let outcome = probe(case, 3000);
        assert!(
            outcome.starts_with("err invalid argument `content`")
                && outcome.contains("MAX_RDF_NESTING"),
            "{case}: {outcome}"
        );
    }
}

/// Far past what any stack holds, and still a small document (about 1 MB): refused by the
/// scan, which keeps no stack of its own.
#[test]
fn fifty_thousand_nested_triple_terms_are_refused_too() {
    for case in ["turtle", "ntriples", "rdfxml"] {
        let outcome = probe(case, 50_000);
        assert!(
            outcome.starts_with("err invalid argument `content`"),
            "{case}: {outcome}"
        );
    }
}

// ------------------------------------------------------------------ at and over the bound

/// At the bound every syntax loads, and the triple term it built is really there.
#[cfg(feature = "rdf-12")]
#[test]
fn a_document_at_the_bound_loads_in_every_syntax() {
    for case in FORMATS {
        let k = kernel();
        let loaded = load(&k, case, BOUND).unwrap_or_else(|e| panic!("{case}: {e}"));
        assert!(loaded.contains("0 -> 1 quads"), "{case}: {loaded}");
    }
}

/// One past the bound is refused, by name, in every syntax, and with or without RDF 1.2 in
/// the build: the scan does not depend on the feature.
#[test]
fn one_past_the_bound_is_refused_in_every_syntax() {
    let k = kernel();
    for case in FORMATS {
        let err = load(&k, case, BOUND + 1).unwrap_err().to_string();
        assert!(
            err.contains("invalid argument `content`") && err.contains("MAX_RDF_NESTING"),
            "{case}: {err}"
        );
    }
}

// ------------------------------------------------------------------ SPARQL LOAD

/// `LOAD <url>` fetches a document and parses it inside oxigraph, with nothing between the
/// fetch and the parse for a scan to stand in — and, in a host whose graph enables
/// `oxigraph/http-client` (`ikigai-cli` does, through rudof), it is an outbound request no
/// `urn:cap:net:*` gates. Both update doors refuse it before evaluation, as a typed
/// `InvalidArgument` on `content`, in every build.
#[test]
fn sparql_load_is_refused_at_both_update_doors() {
    let k = kernel();
    for (iri, text) in [
        ("urn:iki:store:update", "LOAD <http://127.0.0.1:9/deep.ttl>"),
        (
            "urn:iki:store:update",
            "INSERT DATA { <urn:s> <urn:p> <urn:o> } ; LOAD SILENT <http://127.0.0.1:9/x> INTO GRAPH <urn:h>",
        ),
        (
            "urn:iki:store:graph-update",
            "LOAD <http://127.0.0.1:9/deep.ttl> INTO GRAPH <urn:g>",
        ),
    ] {
        let err = update(&k, iri, text).unwrap_err().to_string();
        assert!(
            err.contains("invalid argument `content`") && err.contains("LOAD"),
            "{iri}: {text}: {err}"
        );
    }
    // Nothing of the refused request was applied, its `INSERT DATA` included.
    let count = update(
        &k,
        "urn:iki:store:update",
        "INSERT DATA { <urn:a> <urn:b> <urn:c> }",
    )
    .unwrap();
    assert!(count.contains("0 -> 1 quads"), "{count}");
}
