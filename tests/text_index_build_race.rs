//! A text index's first build and the scoped writes that overlap it never count one literal
//! twice, and an index that already counts one twice heals (ledger #4448).
//!
//! What this reproduces on 0.6.3, two defects:
//!
//! - **One literal counted twice.** A first build cleared the index with tantivy's
//!   `delete_all_documents`, which clears only committed segments: a document a scoped write had
//!   added but not yet committed survived, and the build added the same literal again. The graph
//!   settled at `documents: 9` for 8 literals, with `deleted documents awaiting compaction: 1`,
//!   and stayed there. `many_threads_filling_new_graphs_never_count_a_literal_twice` hits it.
//! - **A count read too early.** A timer commit zeroed `pending` before its documents were
//!   searchable, so a status read in between said `state: ready`, `pending: 0` beside the count
//!   from before the commit, here 2 to 6 documents for 8 literals. A caller that waits for
//!   `pending: 0`, as the ledger's `complete` check does, took that count as settled.
//!
//! An index that already counts one literal twice, as 0.6.3 could leave on disk, heals: its
//! count is checked on open and after every build, and format 3 rebuilds every older index once.
#![cfg(feature = "text-index")]

use futures::executor::block_on;
use ikigai_core::{ArgRef, Capability, Error, Iri, Kernel, Request, Verb};
use ikigai_store::{
    cap_read_graph, cap_write_graph, graph_thread, space, DurableStore, TextEnrollment,
};
use std::sync::Arc;
use std::time::{Duration, Instant};

const TITLE: &str = "urn:example:title";
const BODY: &str = "urn:example:body";

fn enrollment() -> TextEnrollment {
    TextEnrollment::new()
        .graphs_starting_with("urn:example:ledger:")
        .predicate_weighted(TITLE, 2.0)
        .predicate(BODY)
}

fn issue(
    kernel: &Kernel,
    verb: Verb,
    iri: &str,
    args: &[(&str, &str)],
    cap: &Capability,
) -> Result<String, Error> {
    let mut request = Request::new(verb, Iri::parse(iri).unwrap());
    for (name, value) in args {
        request = request.with_arg(*name, ArgRef::Inline(value.as_bytes().to_vec()));
    }
    block_on(kernel.issue(request, cap)).map(|r| String::from_utf8_lossy(&r.bytes).into_owned())
}

fn status_iri(graph: &str) -> String {
    let encoded = graph_thread(graph);
    let encoded = encoded.rsplit_once("graph-written:").unwrap().1;
    format!("urn:iki:store:text-index:graph:{encoded}")
}

fn status(kernel: &Kernel, graph: &str) -> String {
    issue(
        kernel,
        Verb::Source,
        &status_iri(graph),
        &[],
        &Capability::root(),
    )
    .expect("status")
}

fn field(status: &str, name: &str) -> u64 {
    status
        .lines()
        .find_map(|l| l.strip_prefix(&format!("{name}: ")))
        .unwrap_or_else(|| panic!("no `{name}` in {status}"))
        .parse()
        .unwrap()
}

/// Wait until the index is ready with nothing pending, as the ledger's `complete` check does,
/// and answer its status.
fn settled(kernel: &Kernel, graph: &str) -> String {
    let start = Instant::now();
    loop {
        let s = status(kernel, graph);
        if s.contains("state: ready\n") && field(&s, "pending") == 0 {
            return s;
        }
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "never settled: {s}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn write(kernel: &Kernel, graph: &str, subject: usize) {
    let cap = Capability::scoped([cap_write_graph(graph), cap_read_graph(graph)]);
    issue(
        kernel,
        Verb::Sink,
        "urn:iki:store:graph-update",
        &[
            ("graph", graph),
            (
                "content",
                &format!(
                    "INSERT DATA {{ GRAPH <{graph}> {{ \
                     <urn:item:{subject}> <{TITLE}> \"Title {subject}\" ; \
                     <{BODY}> \"Body of item {subject}, about staff\" ; \
                     <urn:example:other> \"not enrolled\" }} }}"
                ),
            ),
        ],
        &cap,
    )
    .expect("scoped write");
}

/// Four scoped writes to a graph no index has seen: the first opens the index and starts its
/// build, the rest land while it may still run. Eight enrolled literals, eight documents.
fn fill_new_graph(kernel: &Kernel, graph: &str) {
    for subject in 1..=4 {
        write(kernel, graph, subject);
    }
}

#[test]
fn a_first_build_overlapping_scoped_writes_counts_each_literal_once() {
    // Several graphs, so the race is met many times in one run rather than hoped for once.
    let kernel = Kernel::new(Arc::new(space(
        DurableStore::in_memory()
            .unwrap()
            .with_text_index(enrollment())
            .unwrap(),
    )));
    for n in 0..8 {
        let graph = format!("urn:example:ledger:race{n}");
        fill_new_graph(&kernel, &graph);
        let s = settled(&kernel, &graph);
        assert_eq!(
            field(&s, "documents"),
            8,
            "the graph holds 8 enrolled literals: {s}"
        );
    }
}

/// Many threads, each writing ten graphs no index has seen, as gonk's tests do in parallel:
/// on 0.6.3 a few graphs of a few hundred settled at `documents: 9` with `deleted documents
/// awaiting compaction: 1`, the state the ledger reported. A first build's
/// `delete_all_documents` cleared only committed segments, so a document a scoped write had
/// added but not yet committed survived it, and the build added the same literal again.
#[test]
fn many_threads_filling_new_graphs_never_count_a_literal_twice() {
    const THREADS: usize = 16;
    const GRAPHS: usize = 10;
    let kernel = Arc::new(Kernel::new(Arc::new(space(
        DurableStore::in_memory()
            .unwrap()
            .with_text_index(enrollment())
            .unwrap(),
    ))));
    let threads: Vec<_> = (0..THREADS)
        .map(|t| {
            let kernel = Arc::clone(&kernel);
            std::thread::spawn(move || {
                let mut wrong = Vec::new();
                for n in 0..GRAPHS {
                    let graph = format!("urn:example:ledger:stress{t}x{n}");
                    fill_new_graph(&kernel, &graph);
                    let s = settled(&kernel, &graph);
                    if field(&s, "documents") != 8 {
                        wrong.push(s);
                    }
                }
                wrong
            })
        })
        .collect();
    let wrong: Vec<String> = threads
        .into_iter()
        .flat_map(|t| t.join().unwrap())
        .collect();
    assert!(
        wrong.is_empty(),
        "{} of {} graphs, each holding 8 enrolled literals, settled at another count:\n{}",
        wrong.len(),
        THREADS * GRAPHS,
        wrong.join("\n")
    );
}

#[cfg(feature = "persistent")]
mod durable {
    use super::*;

    fn temp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ikigai-store-text-race-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn open(dir: &std::path::Path) -> Kernel {
        Kernel::new(Arc::new(space(
            DurableStore::open(dir)
                .unwrap()
                .with_text_index(enrollment())
                .unwrap(),
        )))
    }

    fn index_dirs(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        std::fs::read_dir(dir.join("text-index"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect()
    }

    /// Add a second copy of an indexed document straight into a closed index, as the race
    /// left one on disk, keeping its meta `clean: true` and setting its `format` if given.
    fn double_one_document(index_dir: &std::path::Path, format: Option<u32>) {
        let index = tantivy::Index::open_in_dir(index_dir).unwrap();
        let schema = index.schema();
        let key = schema.get_field("key").unwrap();
        let subject = schema.get_field("subject").unwrap();
        let predicate = schema.get_field("predicate").unwrap();
        let word = schema.get_field("word").unwrap();
        let mut writer: tantivy::IndexWriter<tantivy::TantivyDocument> =
            index.writer_with_num_threads(1, 15_000_000).unwrap();
        let mut doc = tantivy::TantivyDocument::default();
        // Any key the index already holds would do; the count is what the ledger checks.
        doc.add_text(key, "doubled");
        doc.add_text(subject, "urn:item:1");
        doc.add_text(predicate, TITLE);
        doc.add_text(word, "Title 1");
        writer.add_document(doc).unwrap();
        writer.commit().unwrap();
        drop(writer);
        let meta = index_dir.join("ikigai-text-index.json");
        let mut m: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&meta).unwrap()).unwrap();
        assert_eq!(m["clean"], true, "closed cleanly: {m}");
        if let Some(format) = format {
            m["format"] = format.into();
        }
        std::fs::write(&meta, m.to_string()).unwrap();
    }

    fn heals_from(name: &str, format: Option<u32>) {
        let dir = temp(name);
        let graph = "urn:example:ledger:upgrade";
        {
            let kernel = open(&dir);
            fill_new_graph(&kernel, graph);
            assert_eq!(field(&settled(&kernel, graph), "documents"), 8);
        }
        let dirs = index_dirs(&dir);
        assert_eq!(dirs.len(), 1);
        double_one_document(&dirs[0], format);
        let kernel = open(&dir);
        let s = settled(&kernel, graph);
        assert_eq!(
            field(&s, "documents"),
            8,
            "an index closed with a doubled document heals on open: {s}"
        );
        drop(kernel);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An index 0.6.3 wrote (format 2) holding one doubled document: reopened, it is rebuilt.
    #[test]
    fn a_format_2_index_with_a_doubled_document_is_rebuilt_on_open() {
        heals_from("format2", Some(2));
    }

    /// The current format, closed cleanly yet counting one document more than the store
    /// holds literals: the count is checked on open, and the index rebuilt.
    #[test]
    fn a_current_index_whose_count_drifted_is_rebuilt_on_open() {
        heals_from("current", None);
    }
}
