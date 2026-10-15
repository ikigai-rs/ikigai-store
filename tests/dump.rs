//! `urn:iki:store:dump`: the whole dataset as N-Quads, under the broad read grant only
//! (ledger #374).
//!
//! What a backup needs and a hand-rolled `SELECT ?g ?s ?p ?o` got wrong by default: every
//! named graph AND the default graph, the engine's own escaping, a stable order, and a refusal
//! rather than a truncated file when the dataset is past the caller's bound.

use futures::executor::block_on;
use ikigai_core::{ArgRef, Capability, Error, Iri, Kernel, Request, Verb};
use ikigai_store::{cap_read_graph, space, DurableStore, CAP_READ};
use oxigraph::io::{RdfFormat, RdfParser};
use oxigraph::model::Quad;
use std::collections::BTreeSet;
use std::sync::Arc;

fn kernel() -> Kernel {
    Kernel::with_meta_renderer(
        Arc::new(space(DurableStore::in_memory().unwrap())),
        Arc::new(ikigai_vocab::TurtleRenderer),
    )
}

fn issue(
    kernel: &Kernel,
    cap: &Capability,
    verb: Verb,
    iri: &str,
    args: &[(&str, &str)],
) -> ikigai_core::Result<ikigai_core::Representation> {
    let request = args.iter().fold(
        Request::new(verb, Iri::parse(iri).unwrap()),
        |request, (name, value)| request.with_arg(*name, ArgRef::Inline(value.as_bytes().to_vec())),
    );
    block_on(kernel.issue(request, cap))
}

/// TriG: two named graphs, the default graph, and literals that need escaping, a language
/// tag and a datatype, so the serializer has something to get wrong.
const DATA: &str = r#"
<urn:x:a> <urn:x:p> "in the default graph" .
GRAPH <urn:x:g1> {
    <urn:x:a> <urn:x:p> "a \"quoted\" line\nand a second line"@en .
    <urn:x:b> <urn:x:n> "42"^^<http://www.w3.org/2001/XMLSchema#integer> .
}
GRAPH <urn:x:g2> {
    <urn:x:a> <urn:x:p> "in g2" .
}
"#;

fn seeded() -> Kernel {
    let k = kernel();
    issue(
        &k,
        &Capability::root(),
        Verb::Sink,
        "urn:iki:store:load",
        &[("content", DATA), ("format", "application/trig")],
    )
    .unwrap();
    k
}

fn quads(text: &[u8], format: RdfFormat) -> BTreeSet<String> {
    RdfParser::from_format(format)
        .for_slice(text)
        .map(|q| q.unwrap())
        .map(|q: Quad| q.to_string())
        .collect()
}

fn dump(k: &Kernel, cap: &Capability, args: &[(&str, &str)]) -> ikigai_core::Result<Vec<u8>> {
    issue(k, cap, Verb::Source, "urn:iki:store:dump", args).map(|rep| rep.bytes)
}

#[test]
fn the_dump_is_n_quads_with_every_graph_and_the_default_graph() {
    let k = seeded();
    let rep = issue(
        &k,
        &Capability::scoped([CAP_READ.to_string()]),
        Verb::Source,
        "urn:iki:store:dump",
        &[],
    )
    .unwrap();
    assert!(
        rep.repr_type.to_string().starts_with("application/n-quads"),
        "{}",
        rep.repr_type
    );
    let text = String::from_utf8(rep.bytes.clone()).unwrap();
    // Exactly the quads that went in, every graph and the default graph.
    assert_eq!(
        quads(&rep.bytes, RdfFormat::NQuads),
        quads(DATA.as_bytes(), RdfFormat::TriG)
    );
    assert_eq!(text.lines().count(), 4, "{text}");
    // The shape: a default-graph quad has no fourth term, a named one ends in its graph.
    assert!(
        text.contains("<urn:x:a> <urn:x:p> \"in the default graph\" .\n"),
        "{text}"
    );
    assert!(
        text.contains("<urn:x:a> <urn:x:p> \"in g2\" <urn:x:g2> .\n"),
        "{text}"
    );
    // The engine's escaping: the newline inside the literal stays inside one line.
    assert!(
        text.contains(r#""a \"quoted\" line\nand a second line"@en"#),
        "{text}"
    );
}

/// ★ Sorted lines, so two dumps of the same dataset are the same bytes whatever order the
/// quads went in, and a dump loaded into another store dumps identically.
#[test]
fn the_dump_is_sorted_and_round_trips_through_load() {
    let k = seeded();
    let root = Capability::root();
    let first = dump(&k, &root, &[]).unwrap();
    let text = String::from_utf8(first.clone()).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    let mut sorted = lines.clone();
    sorted.sort_unstable();
    assert_eq!(lines, sorted, "{text}");

    let other = kernel();
    issue(
        &other,
        &root,
        Verb::Sink,
        "urn:iki:store:load",
        &[("content", &text), ("format", "application/n-quads")],
    )
    .unwrap();
    assert_eq!(dump(&other, &root, &[]).unwrap(), first);
}

/// ⚠ The capability posture: the broad read grant, and nothing narrower. A tenant holding a
/// grant for every graph in the store is still refused, because the default graph has no IRI
/// and the dump reads it too.
#[test]
fn only_the_broad_read_grant_dumps() {
    let k = seeded();
    let every_graph = Capability::scoped([cap_read_graph("urn:x:g1"), cap_read_graph("urn:x:g2")]);
    assert!(
        matches!(dump(&k, &every_graph, &[]), Err(Error::Denied(_))),
        "per-graph grants dumped the whole dataset"
    );
    assert!(dump(&k, &Capability::scoped([CAP_READ.to_string()]), &[]).is_ok());
}

/// Past the bound it is refused, never truncated, naming the bound it crossed.
#[test]
fn a_dataset_past_the_bound_is_refused_not_truncated() {
    let k = seeded();
    let root = Capability::root();
    let err = dump(&k, &root, &[("max_rows", "3")]).unwrap_err();
    assert!(
        matches!(&err, Error::InvalidArgument { name, detail }
            if name == "max_rows" && detail.contains("larger than 3 quads")),
        "{err:?}"
    );
    let err = dump(&k, &root, &[("max_bytes", "64")]).unwrap_err();
    assert!(
        matches!(&err, Error::InvalidArgument { name, detail }
            if name == "max_bytes" && detail.contains("larger than 64 bytes")),
        "{err:?}"
    );
    // At the bound it is answered in full.
    assert_eq!(
        dump(&k, &root, &[("max_rows", "4")])
            .unwrap()
            .split(|b| *b == b'\n')
            .count(),
        5
    );
}
