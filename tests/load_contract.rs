//! `urn:iki:store:load` does what its declaration says (ledger #751, probes p8 and p8b).
//!
//! `format` declares five syntaxes in its `one_of` and used to accept anything oxigraph
//! recognizes — N3 included, whose formulas land in blank-node graphs that no grant, no
//! scoped door and `urn:iki:store:graphs` can ever name. And `graph`'s summary said quad
//! syntaxes ignore it, when in fact it replaces the DOCUMENT's default graph. The first was
//! a code defect; the second was a doc defect, and this file pins what the code does.

use futures::executor::block_on;
use ikigai_core::{ArgRef, Capability, Error, Iri, Kernel, Request, Verb};
use ikigai_store::{space, DurableStore};
use std::sync::Arc;

fn kernel() -> Kernel {
    Kernel::new(Arc::new(space(DurableStore::in_memory().unwrap())))
}

fn run(kernel: &Kernel, verb: Verb, iri: &str, args: &[(&str, &str)]) -> Result<String, Error> {
    let mut request = Request::new(verb, Iri::parse(iri).unwrap());
    for (name, value) in args {
        request = request.with_arg(*name, ArgRef::Inline(value.as_bytes().to_vec()));
    }
    block_on(kernel.issue(request, &Capability::root()))
        .map(|r| String::from_utf8_lossy(&r.bytes).into_owned())
}

fn ask(kernel: &Kernel, query: &str) -> bool {
    run(
        kernel,
        Verb::Source,
        "urn:iki:store:ask",
        &[("query", query)],
    )
    .unwrap()
    .contains("true")
}

#[test]
fn load_refuses_a_format_outside_its_declared_one_of() {
    // ⚠ Not `text/plain`: oxigraph maps it to N-Triples, one of the five, so it IS a
    // declared syntax under another name — the same leniency `as` has on the query doors.
    for media in ["text/n3", "application/ld+json", "nonsense"] {
        let k = kernel();
        let got = run(
            &k,
            Verb::Sink,
            "urn:iki:store:load",
            &[
                (
                    "content",
                    "{ <urn:s> <urn:p> <urn:o> } => { <urn:s> <urn:q> <urn:o> } .",
                ),
                ("format", media),
            ],
        );
        assert!(
            matches!(&got, Err(Error::InvalidArgument { name, .. }) if name == "format"),
            "`{media}` was not refused by name: {got:?}"
        );
        assert!(
            !ask(&k, "ASK { { ?s ?p ?o } UNION { GRAPH ?g { ?s ?p ?o } } }"),
            "{media}"
        );
    }
    // Every declared syntax still loads, and a parameter on one of them is that syntax.
    for (media, doc) in [
        ("text/turtle", "<urn:s> <urn:p> <urn:o> ."),
        ("text/turtle; charset=utf-8", "<urn:s> <urn:p> <urn:o> ."),
        ("application/n-triples", "<urn:s> <urn:p> <urn:o> .\n"),
        ("application/n-quads", "<urn:s> <urn:p> <urn:o> <urn:g> .\n"),
        ("application/trig", "<urn:g> { <urn:s> <urn:p> <urn:o> }"),
        (
            "application/rdf+xml",
            r#"<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="urn:s"><p xmlns="urn:" rdf:resource="urn:o"/></rdf:Description></rdf:RDF>"#,
        ),
    ] {
        run(
            &kernel(),
            Verb::Sink,
            "urn:iki:store:load",
            &[("content", doc), ("format", media)],
        )
        .unwrap_or_else(|e| panic!("`{media}` is declared and must load: {e}"));
    }
}

/// ★ What `graph=` does with a QUAD syntax, pinned because the summary used to say it was
/// ignored: it replaces the document's default graph, and a statement that names its own
/// graph keeps it.
#[test]
fn load_graph_replaces_a_quad_documents_default_graph_and_nothing_else() {
    for (media, doc) in [
        (
            "application/n-quads",
            "<urn:s> <urn:p> <urn:bare> .\n<urn:s> <urn:p> <urn:named> <urn:g:own> .\n",
        ),
        (
            "application/trig",
            "<urn:s> <urn:p> <urn:bare> . <urn:g:own> { <urn:s> <urn:p> <urn:named> }",
        ),
    ] {
        let k = kernel();
        run(
            &k,
            Verb::Sink,
            "urn:iki:store:load",
            &[("content", doc), ("format", media), ("graph", "urn:g:x")],
        )
        .unwrap();
        assert!(
            ask(&k, "ASK { GRAPH <urn:g:x> { <urn:s> <urn:p> <urn:bare> } }"),
            "{media}"
        );
        assert!(
            !ask(&k, "ASK { <urn:s> <urn:p> <urn:bare> }"),
            "{media}: stayed in default"
        );
        assert!(
            ask(
                &k,
                "ASK { GRAPH <urn:g:own> { <urn:s> <urn:p> <urn:named> } }"
            ),
            "{media}"
        );
        assert!(
            !ask(
                &k,
                "ASK { GRAPH <urn:g:x> { <urn:s> <urn:p> <urn:named> } }"
            ),
            "{media}"
        );
    }
}
