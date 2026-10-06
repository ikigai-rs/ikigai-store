//! Each query IRI refuses the other result FAMILY, and answers both forms of its own
//! (ledger #751, probe p10).
//!
//! The audit found the summaries promising that "a query of another form is refused" while
//! `select` answered ASK, `ask` answered SELECT, and likewise for CONSTRUCT and DESCRIBE.
//! The code is intentional — an IRI's promise is its declared `outputs`, which the family
//! fixes — so the DOCS were fixed, and this pins the behavior they now describe, on both
//! the broad and the scoped doors.

use futures::executor::block_on;
use ikigai_core::{ArgRef, Capability, Error, Iri, Kernel, Request, Verb};
use ikigai_store::{space, DurableStore};
use std::sync::Arc;

const G: &str = "urn:example:g";

fn kernel() -> Kernel {
    let k = Kernel::new(Arc::new(space(DurableStore::in_memory().unwrap())));
    let seed = format!(
        "INSERT DATA {{ <urn:p:alice> <urn:name> \"alice\" . GRAPH <{G}> {{ <urn:p:alice> <urn:name> \"alice\" }} }}"
    );
    run(
        &k,
        "urn:iki:store:update",
        Verb::Sink,
        &[("content", &seed)],
    )
    .expect("seed");
    k
}

fn run(k: &Kernel, iri: &str, verb: Verb, args: &[(&str, &str)]) -> Result<String, Error> {
    let mut request = Request::new(verb, Iri::parse(iri).unwrap());
    for (name, value) in args {
        request = request.with_arg(*name, ArgRef::Inline(value.as_bytes().to_vec()));
    }
    block_on(k.issue(request, &Capability::root()))
        .map(|r| String::from_utf8_lossy(&r.bytes).into_owned())
}

#[test]
fn a_query_iri_answers_its_family_and_refuses_the_other() {
    let k = kernel();
    let result_set = ["SELECT * { ?s ?p ?o }", "ASK { ?s ?p ?o }"];
    let graph = [
        "CONSTRUCT { ?s ?p ?o } WHERE { ?s ?p ?o }",
        "DESCRIBE <urn:p:alice>",
    ];
    for (form, answers, refuses) in [
        ("select", result_set, graph),
        ("ask", result_set, graph),
        ("construct", graph, result_set),
        ("describe", graph, result_set),
    ] {
        for (prefix, extra) in [("", None), ("graph-", Some(("graph", G)))] {
            let iri = format!("urn:iki:store:{prefix}{form}");
            let args = |q: &'static str| {
                let mut a = vec![("query", q)];
                a.extend(extra);
                a
            };
            for q in answers {
                run(&k, &iri, Verb::Source, &args(q))
                    .unwrap_or_else(|e| panic!("{iri} must answer `{q}` (same family): {e}"));
            }
            for q in refuses {
                let got = run(&k, &iri, Verb::Source, &args(q));
                assert!(
                    matches!(&got, Err(Error::InvalidArgument { name, .. }) if name == "query"),
                    "{iri} must refuse `{q}` (other family): {got:?}"
                );
            }
        }
    }
}
