//! A graph IRI containing a non-ASCII space is one graph on every door (ledger #751,
//! probe p4).
//!
//! RFC 3987's `ucschar` admits U+00A0, U+3000 and the other non-ASCII spaces, and oxiri
//! accepts them. The write door took such an IRI as one graph and `urn:iki:store:graphs`
//! listed it as one, but the scoped read door split `graph=` on Unicode whitespace and
//! read it as two — demanding two grants nobody holds, so the graph was writable and
//! listable and never readable. The separator is ASCII whitespace now.

use futures::executor::block_on;
use ikigai_core::{ArgRef, Capability, Iri, Kernel, Request, Verb};
use ikigai_store::{cap_read_graph, cap_write_graph, space, DurableStore};
use std::sync::Arc;

fn run(k: &Kernel, verb: Verb, iri: &str, args: &[(&str, &str)], cap: &Capability) -> String {
    let mut request = Request::new(verb, Iri::parse(iri).unwrap());
    for (name, value) in args {
        request = request.with_arg(*name, ArgRef::Inline(value.as_bytes().to_vec()));
    }
    String::from_utf8(
        block_on(k.issue(request, cap))
            .unwrap_or_else(|e| panic!("{iri}: {e}"))
            .bytes,
    )
    .unwrap()
}

#[test]
fn a_graph_iri_containing_a_unicode_space_is_read_as_itself() {
    for g in ["urn:a\u{3000}urn:b", "urn:a\u{a0}urn:b"] {
        assert!(oxigraph_accepts(g));
        let k = Kernel::new(Arc::new(space(DurableStore::in_memory().unwrap())));
        let tenant = Capability::scoped([cap_write_graph(g), cap_read_graph(g)]);
        run(
            &k,
            Verb::Sink,
            "urn:iki:store:graph-update",
            &[
                ("graph", g),
                (
                    "content",
                    &format!("INSERT DATA {{ GRAPH <{g}> {{ <urn:s> <urn:p> <urn:o> }} }}"),
                ),
            ],
            &tenant,
        );
        assert_eq!(
            run(&k, Verb::Source, "urn:iki:store:graphs", &[], &tenant),
            format!("{g}\n")
        );
        let read = run(
            &k,
            Verb::Source,
            "urn:iki:store:graph-ask",
            &[("graph", g), ("query", "ASK { <urn:s> <urn:p> <urn:o> }")],
            &tenant,
        );
        assert!(read.contains("true"), "{g:?}: {read}");
    }
}

/// The IRI is valid by the store's own parser, which is what makes the split a defect and
/// not a refusal of bad input.
fn oxigraph_accepts(g: &str) -> bool {
    ikigai_store::sparql::iri(g, "graph").is_ok()
}
