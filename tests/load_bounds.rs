//! `urn:iki:store:load` bounds a document's bytes, refuses XML entity declarations, and bounds
//! what a document's statements expand to (ledger #1428).
//!
//! The claim, reproduced here before it was fixed: the load door's only pre-parse guard was
//! triple-term DEPTH. It bounded no bytes, and oxrdfxml resolves `<!ENTITY>` values against
//! earlier entities AT DECLARATION TIME, storing each fully expanded, so a "billion laughs"
//! chain of eleven entities, four references each, made a 525-byte document expand one
//! reference to ~3 MB, and ~16 levels (~680 bytes) to ~3 GB. Nothing refused it, and it was
//! parsed on the store's writer thread under the write lock.
//!
//! The same review of the other syntaxes this door loads found the same SHAPE without
//! entities: a Turtle or TriG `@prefix` (or `@base`), an RDF/XML namespace or `xml:base`, and
//! the door's own `graph` argument are each written ONCE and copied into every statement that
//! uses them, so a few hundred kilobytes of text can spell out gigabytes of terms. N-Triples
//! and N-Quads spell every term out in full and do not amplify.
//!
//! So, before the writer is asked for anything:
//!
//! - a document over the store's load bound (`MAX_LOAD_BYTES` unless the host sets another)
//!   is refused, naming the bound;
//! - an RDF/XML document that declares an entity is refused;
//! - the document is parsed once without storing anything, counting the bytes of the terms its
//!   statements produce, and refused past the same bound. Only a document that passes is
//!   parsed again on the writer and stored.

use futures::executor::block_on;
use ikigai_core::{ArgRef, Capability, Error, Iri, Kernel, Request, Verb};
use ikigai_store::depth::check_rdf_nesting;
use ikigai_store::limits::MAX_LOAD_BYTES;
use ikigai_store::{space, DurableStore, StorageError, CAP_READ, CAP_WRITE};
use oxigraph::io::RdfFormat;
use std::sync::Arc;
use std::time::Duration;

/// `levels` entities, each referencing the previous one `fanout` times; the body references
/// the last. One reference expands to about `3 * fanout^(levels - 1)` bytes. (The auditor's
/// document.)
fn laughs(levels: usize, fanout: usize) -> String {
    let mut entities = String::from("<!ENTITY e0 \"lol\">");
    for n in 1..levels {
        let prev = format!("&e{};", n - 1).repeat(fanout);
        entities.push_str(&format!("<!ENTITY e{n} \"{prev}\">"));
    }
    format!(
        "<?xml version=\"1.0\"?>\n<!DOCTYPE rdf [{entities}]>\n\
         <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\
         <rdf:Description rdf:about=\"urn:s\"><rdf:value>&e{};</rdf:value></rdf:Description>\
         </rdf:RDF>",
        levels - 1
    )
}

fn kernel(store: &DurableStore) -> Kernel {
    Kernel::new(Arc::new(space(store.clone())))
}

fn load(
    kernel: &Kernel,
    document: &str,
    format: &str,
    graph: Option<&str>,
) -> ikigai_core::Result<String> {
    let mut request = Request::new(Verb::Sink, Iri::parse("urn:iki:store:load").unwrap())
        .with_arg("content", ArgRef::Inline(document.as_bytes().to_vec()))
        .with_arg("format", ArgRef::Inline(format.as_bytes().to_vec()));
    if let Some(graph) = graph {
        request = request.with_arg("graph", ArgRef::Inline(graph.as_bytes().to_vec()));
    }
    block_on(kernel.issue(request, &Capability::scoped([CAP_WRITE, CAP_READ])))
        .map(|rep| String::from_utf8_lossy(&rep.bytes).into_owned())
}

fn quads(store: &DurableStore) -> usize {
    let kernel = kernel(store);
    let answer = block_on(kernel.issue(
        Request::new(Verb::Source, Iri::parse("urn:iki:store:select").unwrap()).with_arg(
            "query",
            ArgRef::Inline(
                b"SELECT (COUNT(*) AS ?n) WHERE { { GRAPH ?g { ?s ?p ?o } } UNION { ?s ?p ?o } }"
                    .to_vec(),
            ),
        ),
        &Capability::root(),
    ))
    .unwrap();
    let text = String::from_utf8_lossy(&answer.bytes).into_owned();
    // `"value":"<n>"` in SPARQL JSON results.
    let value = text.split("\"value\":\"").nth(1).unwrap();
    value[..value.find('"').unwrap()].parse().unwrap()
}

fn refused_on_content(result: ikigai_core::Result<String>, needle: &str) {
    match result {
        Err(Error::InvalidArgument { name, detail }) => {
            assert_eq!(name, "content");
            assert!(
                detail.contains(needle),
                "the refusal names `{needle}`: {detail}"
            );
            assert!(detail.contains("Nothing was loaded"), "{detail}");
        }
        other => panic!("expected the document refused on `content`, got {other:?}"),
    }
}

/// ★ The auditor's document: tiny, no triple terms (so the depth gate passes it), and refused
/// for declaring entities at all. Before the fix every level here loaded.
#[test]
fn an_rdf_xml_entity_declaration_is_refused() {
    let store = DurableStore::in_memory().unwrap();
    let k = kernel(&store);
    for levels in [1usize, 4, 11] {
        let document = laughs(levels, 4);
        assert!(document.len() < 600, "{} bytes", document.len());
        check_rdf_nesting(document.as_bytes(), RdfFormat::RdfXml, "content").unwrap();
        refused_on_content(load(&k, &document, "application/rdf+xml", None), "ENTITY");
    }
    // In any case: oxrdfxml reads `<!ENTITY`, and nothing reads a lowercase one, but a scan
    // that is stricter than the parser costs only documents no real data looks like.
    let lower = laughs(2, 2).replace("<!ENTITY", "<!entity");
    refused_on_content(load(&k, &lower, "application/rdf+xml", None), "ENTITY");
    assert_eq!(quads(&store), 0, "nothing was loaded");
}

/// A DOCTYPE that declares no entity is harmless and still loads.
#[test]
fn a_doctype_without_entities_still_loads() {
    let store = DurableStore::in_memory().unwrap();
    let document = "<?xml version=\"1.0\"?>\n<!DOCTYPE rdf:RDF>\n\
         <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\
         <rdf:Description rdf:about=\"urn:s\"><rdf:value>&amp;lol</rdf:value></rdf:Description>\
         </rdf:RDF>";
    load(&kernel(&store), document, "application/rdf+xml", None).unwrap();
    assert_eq!(quads(&store), 1);
}

/// A document over the store's load bound is refused, naming it.
#[test]
fn a_document_over_the_load_bound_is_refused() {
    assert_eq!(MAX_LOAD_BYTES, 256 << 20, "the default bound");
    assert_eq!(
        DurableStore::in_memory().unwrap().max_load_bytes(),
        MAX_LOAD_BYTES
    );
    let store = DurableStore::in_memory().unwrap().with_max_load_bytes(4096);
    let line = "<urn:s> <urn:p> \"a literal of some length\" .\n";
    let fits = line.repeat(4096 / line.len());
    load(&kernel(&store), &fits, "application/n-triples", None).unwrap();
    let over = line.repeat(4096 / line.len() + 1);
    refused_on_content(
        load(&kernel(&store), &over, "application/n-triples", None),
        "4096",
    );
    assert_eq!(quads(&store), 1, "the over-bound document added nothing");
}

/// ★ The amplification the review found beside the entity one: a long `@prefix` IRI written
/// once and used on every statement. ~70 KB of Turtle spells out ~20 MB of terms here; scaled
/// up, a few hundred KB spells out gigabytes. Refused on what it PRODUCES.
#[test]
fn a_prefix_that_expands_past_the_bound_is_refused() {
    let store = DurableStore::in_memory()
        .unwrap()
        .with_max_load_bytes(1 << 20);
    let k = kernel(&store);
    let long = format!("urn:{}", "x".repeat(64 << 10));
    let uses = "p:a p:a p:a .\n".repeat(100);
    let turtle = format!("@prefix p: <{long}#> .\n{uses}");
    assert!(turtle.len() < 1 << 20, "the document itself fits the bound");
    refused_on_content(load(&k, &turtle, "text/turtle", None), "expand");
    let trig = format!("@prefix p: <{long}#> .\n<urn:g> {{ {uses} }}");
    refused_on_content(load(&k, &trig, "application/trig", None), "expand");
    // `@base` is the same shape.
    let based = format!("@base <{long}/> .\n{}", "<a> <a> <a> .\n".repeat(100));
    refused_on_content(load(&k, &based, "text/turtle", None), "expand");
    // So is an RDF/XML namespace.
    let props: String = (0..100).map(|i| format!("<p:a>{i}</p:a>")).collect();
    let xml = format!(
        "<rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\" \
         xmlns:p=\"{long}#\"><rdf:Description rdf:about=\"urn:s\">{props}\
         </rdf:Description></rdf:RDF>"
    );
    refused_on_content(load(&k, &xml, "application/rdf+xml", None), "expand");
    // And the door's own `graph`, copied into every statement.
    let flat = "<urn:s> <urn:p> <urn:o> .\n".repeat(100);
    refused_on_content(
        load(&k, &flat, "application/n-triples", Some(&long)),
        "expand",
    );
    assert_eq!(quads(&store), 0, "nothing was loaded");
    // The same statements with a short prefix load.
    load(
        &k,
        &format!("@prefix p: <urn:p#> .\n{uses}"),
        "text/turtle",
        None,
    )
    .unwrap();
    assert_eq!(quads(&store), 1);
}

/// ★ Refused before the writer is asked: a refused document is answered while another write
/// holds the store's writer thread and write lock, rather than waiting behind it.
#[test]
fn a_refused_document_does_not_wait_for_the_writer() {
    let (store, _handle) = DurableStore::in_memory_shared().unwrap();
    let writer = store.shared_writer().unwrap();
    let (release, held) = std::sync::mpsc::channel::<()>();
    let (started, running) = std::sync::mpsc::channel::<()>();
    let holder = std::thread::spawn(move || {
        writer.write(move |_txn| {
            started.send(()).unwrap();
            // Hold the writer thread (and the write lock) until released, bounded so a
            // failure cannot hang the test binary.
            let _ = held.recv_timeout(Duration::from_secs(30));
            Ok::<_, StorageError>(())
        })
    });
    running.recv().unwrap();
    let k = kernel(&store);
    let (answer, answered) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = answer.send(load(&k, &laughs(11, 4), "application/rdf+xml", None));
    });
    let result = answered.recv_timeout(Duration::from_secs(5));
    release.send(()).unwrap();
    holder.join().unwrap().unwrap();
    match result {
        Ok(result) => refused_on_content(result, "ENTITY"),
        Err(_) => panic!("the refusal waited for the writer"),
    }
}
