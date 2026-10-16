//! `urn:iki:store:dump`: the whole dataset as N-Quads, under the broad read grant only
//! (ledger #374).
//!
//! What a backup needs and a hand-rolled `SELECT ?g ?s ?p ?o` got wrong by default: every
//! named graph AND the default graph, the engine's own escaping, a stable order, and a refusal
//! rather than a truncated file when the dataset is past the caller's bound.
//!
//! The order is canonical PER BACKING (ledger #1732): sorted lines in memory, RocksDB's key
//! order on a durable store, where the answer is written as read and held once. The durable
//! half is at the bottom, behind `persistent`.

use futures::executor::block_on;
use ikigai_core::{ArgRef, Capability, Error, Expiry, Iri, Kernel, Request, Verb};
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

/// ★ A dump is never cacheable (ledger #1732), even on an owned store whose every other
/// whole-dataset read is: the kernel copies a cacheable answer into its cache before deciding
/// whether to keep it, so a cacheable dump held the dataset twice. The pair: a broad SELECT
/// on the same store IS cacheable, so the bare dump is the dump's own choice.
#[test]
fn a_dump_is_never_cacheable_though_the_stores_other_reads_are() {
    let k = seeded();
    let root = Capability::root();
    let rep = issue(&k, &root, Verb::Source, "urn:iki:store:dump", &[]).unwrap();
    assert!(matches!(rep.expiry, Expiry::Always), "{:?}", rep.expiry);
    let select = issue(
        &k,
        &root,
        Verb::Source,
        "urn:iki:store:select",
        &[("query", "SELECT * WHERE { ?s ?p ?o }")],
    )
    .unwrap();
    assert!(
        matches!(select.expiry, Expiry::Never),
        "{:?}",
        select.expiry
    );
}

/// The durable half: RocksDB's key order, written as read.
#[cfg(feature = "persistent")]
mod durable {
    use super::*;
    use oxigraph::io::RdfSerializer;
    use std::path::PathBuf;

    fn scratch(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "ikigai-store-dump-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        path
    }

    /// Enough quads, over enough IRIs, that the store's key order (IRIs are keyed by a hash)
    /// is not the text order by accident.
    fn many() -> Vec<String> {
        (0..60)
            .map(|n| {
                let graph = match n % 3 {
                    0 => String::new(),
                    1 => " <urn:x:g1>".to_string(),
                    _ => format!(" <urn:x:g{}>", n % 7),
                };
                format!("<urn:x:s{n}> <urn:x:p{}> \"value {n}\"{graph} .\n", n % 4)
            })
            .collect()
    }

    fn durable_kernel(path: &PathBuf) -> (Kernel, ikigai_store::SnapshotReader) {
        let store = DurableStore::open(path).unwrap();
        let reader = store.reader();
        let kernel = Kernel::with_meta_renderer(
            Arc::new(space(store)),
            Arc::new(ikigai_vocab::TurtleRenderer),
        );
        (kernel, reader)
    }

    fn load(k: &Kernel, lines: &[String]) {
        issue(
            k,
            &Capability::root(),
            Verb::Sink,
            "urn:iki:store:load",
            &[
                ("content", &lines.concat()),
                ("format", "application/n-quads"),
            ],
        )
        .unwrap();
    }

    /// ★ Written as read: the dump IS the store's snapshot serialized in iteration order, which
    /// on this data is not text order. (Before ledger #1732 the dump sorted, and this failed.)
    #[test]
    fn a_durable_dump_is_the_stores_key_order_written_as_read() {
        let path = scratch("key-order");
        let (k, reader) = durable_kernel(&path);
        load(&k, &many());
        let dumped = dump(&k, &Capability::root(), &[]).unwrap();

        let mut read = Vec::new();
        let mut serializer = RdfSerializer::from_format(RdfFormat::NQuads).for_writer(&mut read);
        for quad in reader.snapshot_iter() {
            serializer.serialize_quad(&quad.unwrap()).unwrap();
        }
        serializer.finish().unwrap();
        assert_eq!(
            String::from_utf8_lossy(&dumped),
            String::from_utf8_lossy(&read)
        );

        let text = String::from_utf8(dumped).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        let mut sorted = lines.clone();
        sorted.sort_unstable();
        assert_ne!(
            lines, sorted,
            "the data no longer tells key order from text order"
        );
        // The default graph first: its quads have no fourth term.
        let defaults = lines
            .iter()
            .take_while(|l| l.matches('<').count() == 2)
            .count();
        assert_eq!(defaults, 20, "{text}");
        drop(k);
        let _ = std::fs::remove_dir_all(&path);
    }

    /// ★ The order is a function of each quad alone, whatever the history: the same quads loaded
    /// in the opposite order, beside a quad written and deleted again, dump to the same bytes;
    /// the dump loaded into a third store dumps to the same bytes again; and the lines are the
    /// in-memory backing's lines, which are sorted.
    #[test]
    fn a_durable_dump_does_not_depend_on_history_and_round_trips() {
        let quads = many();
        let (a_path, b_path, c_path) = (scratch("hist-a"), scratch("hist-b"), scratch("hist-c"));
        let (a, _) = durable_kernel(&a_path);
        load(&a, &quads);
        let (b, _) = durable_kernel(&b_path);
        let mut reversed = quads.clone();
        reversed.reverse();
        load(&b, &reversed[..30]);
        load(
            &b,
            &["<urn:x:gone> <urn:x:p> \"briefly\" <urn:x:g1> .\n".to_string()],
        );
        issue(
            &b,
            &Capability::root(),
            Verb::Sink,
            "urn:iki:store:update",
            &[(
                "content",
                "DELETE DATA { GRAPH <urn:x:g1> { <urn:x:gone> <urn:x:p> \"briefly\" } }",
            )],
        )
        .unwrap();
        load(&b, &reversed[30..]);
        let root = Capability::root();
        let first = dump(&a, &root, &[]).unwrap();
        assert_eq!(dump(&b, &root, &[]).unwrap(), first);

        let (c, _) = durable_kernel(&c_path);
        issue(
            &c,
            &root,
            Verb::Sink,
            "urn:iki:store:load",
            &[
                ("content", std::str::from_utf8(&first).unwrap()),
                ("format", "application/n-quads"),
            ],
        )
        .unwrap();
        assert_eq!(dump(&c, &root, &[]).unwrap(), first);

        let memory = kernel();
        load(&memory, &quads);
        let in_memory = String::from_utf8(dump(&memory, &root, &[]).unwrap()).unwrap();
        let text = String::from_utf8(first).unwrap();
        let mut lines: Vec<&str> = text.lines().collect();
        lines.sort_unstable();
        assert_eq!(lines, in_memory.lines().collect::<Vec<_>>());

        drop((a, b, c));
        for path in [a_path, b_path, c_path] {
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}
