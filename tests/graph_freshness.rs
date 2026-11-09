//! A read confined to named graphs depends on those graphs' own freshness (ledger #3322).
//!
//! Two halves, and the second is the one that matters more. A write confined to one graph
//! must leave a cached read of ANOTHER graph alone, or one append re-queries every graph in
//! the store. And every write that CAN change a read must still reach it, because a stale
//! answer is worse than a cache miss: a broad update, a load, and a write outside the
//! store's own path each have a test here that fails if the read goes stale.
//!
//! Each read goes through a composite that counts its own invocations and declares no
//! thread, so "served from the cache" and "recomputed" are observed rather than inferred
//! from a fresh answer that a never-cached read would also give.

use async_trait::async_trait;
use futures::executor::block_on;
use ikigai_core::{
    ArgRef, Capability, Description, Endpoint, EndpointSpace, Error, Exact, Invocation, Iri,
    Kernel, ReprType, Representation, Request, Resolution, Result, Scope, Space, Verb,
};
use ikigai_store::{
    cap_write_graph, graph_thread, space, DurableStore, SharerWrites, StorageError,
};
use oxigraph::model::{GraphNameRef, NamedNodeRef, QuadRef};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

const ALPHA: &str = "urn:example:alpha";
const BETA: &str = "urn:example:beta";
const GAMMA: &str = "urn:example:gamma";

const COUNT: &str = "SELECT (COUNT(*) AS ?n) WHERE { ?s ?p ?o }";
const COUNT_EVERYTHING: &str =
    "SELECT (COUNT(*) AS ?n) WHERE { { GRAPH ?g { ?s ?p ?o } } UNION { ?s ?p ?o } }";

/// Issues one fixed read through the store and answers it `.cacheable()`, naming no thread.
struct Counted {
    runs: Arc<AtomicUsize>,
    target: String,
    args: Vec<(String, String)>,
}

#[async_trait]
impl Endpoint for Counted {
    async fn invoke(&self, inv: &Invocation<'_>) -> Result<Representation> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        let mut request = Request::new(Verb::Source, Iri::parse(self.target.as_str()).unwrap());
        for (name, value) in &self.args {
            request = request.with_arg(name.as_str(), ArgRef::Inline(value.as_bytes().to_vec()));
        }
        let answer = inv.issue(request).await?;
        Ok(Representation::new(ReprType::new("text/plain"), answer.bytes).cacheable())
    }

    fn name(&self) -> &str {
        "counted"
    }

    fn describe(&self) -> Description {
        Description::new("counted").verb(Verb::Source)
    }
}

/// A named read: the store IRI it goes to and its arguments.
type Read = (String, Vec<(String, String)>);

fn select_in(graph: &str) -> Read {
    (
        "urn:iki:store:graph-select".to_string(),
        vec![
            ("graph".to_string(), graph.to_string()),
            ("query".to_string(), COUNT.to_string()),
        ],
    )
}

fn info_of(graph: &str) -> Read {
    (
        "urn:iki:store:graph-info".to_string(),
        vec![("graph".to_string(), graph.to_string())],
    )
}

fn broad() -> Read {
    (
        "urn:iki:store:select".to_string(),
        vec![("query".to_string(), COUNT_EVERYTHING.to_string())],
    )
}

fn listing() -> Read {
    ("urn:iki:store:graphs".to_string(), vec![])
}

/// The kernel and one counter per named read.
struct World {
    kernel: Kernel,
    runs: BTreeMap<String, Arc<AtomicUsize>>,
}

impl World {
    fn new(store: DurableStore, reads: Vec<(&str, Read)>) -> World {
        let mut bound: EndpointSpace = space(store);
        let mut runs = BTreeMap::new();
        for (name, (target, args)) in reads {
            let counter = Arc::new(AtomicUsize::new(0));
            bound = bound.bind(
                Exact::new(format!("urn:example:read:{name}")),
                Counted {
                    runs: Arc::clone(&counter),
                    target,
                    args,
                },
            );
            runs.insert(name.to_string(), counter);
        }
        World {
            kernel: Kernel::new(Arc::new(bound)),
            runs,
        }
    }

    fn runs_of(&self, name: &str) -> usize {
        self.runs[name].load(Ordering::SeqCst)
    }

    /// The read's answer, and whether answering it ran the composite.
    fn read(&self, name: &str) -> (String, bool) {
        let before = self.runs_of(name);
        let answer = block_on(self.kernel.issue(
            Request::new(
                Verb::Source,
                Iri::parse(format!("urn:example:read:{name}")).unwrap(),
            ),
            &Capability::root(),
        ))
        .unwrap_or_else(|e| panic!("reading {name}: {e}"));
        (
            String::from_utf8(answer.bytes).unwrap(),
            self.runs_of(name) != before,
        )
    }

    /// Read until the answer is held, and prove that it is.
    fn warm(&self, name: &str) -> String {
        let (first, _) = self.read(name);
        self.assert_cached(name, &first);
        first
    }

    fn assert_cached(&self, name: &str, expected: &str) {
        let (answer, ran) = self.read(name);
        assert!(
            !ran,
            "`{name}` was recomputed; it should have stayed cached"
        );
        assert_eq!(answer, expected, "`{name}` changed while cached");
    }

    /// The read recomputes, and the answer it gives is the one the write produced.
    fn assert_fresh(&self, name: &str, needle: &str) {
        let (answer, ran) = self.read(name);
        assert!(ran, "`{name}` was served from the cache after a write");
        assert!(
            answer.contains(needle),
            "`{name}` is stale: wanted {needle} in {answer}"
        );
    }

    fn sink(&self, iri: &str, args: &[(&str, &str)], capability: &Capability) -> Result<String> {
        let mut request = Request::new(Verb::Sink, Iri::parse(iri).unwrap());
        for (name, value) in args {
            request = request.with_arg(*name, ArgRef::Inline(value.as_bytes().to_vec()));
        }
        block_on(self.kernel.issue(request, capability))
            .map(|answer| String::from_utf8(answer.bytes).unwrap())
    }

    /// Insert one more quad into `graph` through the scoped door, under a grant for it alone.
    fn scoped_insert(&self, graph: &str, subject: &str) {
        self.sink(
            "urn:iki:store:graph-update",
            &[
                ("graph", graph),
                (
                    "content",
                    &format!("INSERT DATA {{ GRAPH <{graph}> {{ <{subject}> <urn:p> <urn:o> }} }}"),
                ),
            ],
            &Capability::scoped([cap_write_graph(graph)]),
        )
        .expect("the scoped write");
    }

    fn seed(&self, graph: &str) {
        self.sink(
            "urn:iki:store:load",
            &[
                ("content", "<urn:seed> <urn:p> <urn:o> ."),
                ("format", "application/n-triples"),
                ("graph", graph),
            ],
            &Capability::root(),
        )
        .expect("seeding");
    }
}

fn count_is(n: usize) -> String {
    format!("\"value\":\"{n}\"")
}

fn two_graphs() -> World {
    let world = World::new(
        DurableStore::in_memory().unwrap(),
        vec![
            ("alpha", select_in(ALPHA)),
            ("beta", select_in(BETA)),
            ("alpha-info", info_of(ALPHA)),
            ("beta-info", info_of(BETA)),
        ],
    );
    world.seed(ALPHA);
    world.seed(BETA);
    world
}

#[test]
fn a_write_to_one_graph_leaves_another_graphs_cached_reads_alone() {
    let world = two_graphs();
    let beta = world.warm("beta");
    let beta_info = world.warm("beta-info");
    world.warm("alpha");
    world.warm("alpha-info");

    world.scoped_insert(ALPHA, "urn:second");

    world.assert_fresh("alpha", &count_is(2));
    world.assert_fresh("alpha-info", "quads: 2");
    world.assert_cached("beta", &beta);
    world.assert_cached("beta-info", &beta_info);
}

#[test]
fn a_read_of_several_graphs_recomputes_when_a_member_is_written_and_only_then() {
    let both = format!("{ALPHA} {BETA}");
    let world = World::new(
        DurableStore::in_memory().unwrap(),
        vec![("both", select_in(&both))],
    );
    world.seed(ALPHA);
    world.seed(BETA);
    world.seed(GAMMA);
    let held = world.warm("both");

    world.scoped_insert(GAMMA, "urn:elsewhere");
    world.assert_cached("both", &held);

    world.scoped_insert(BETA, "urn:second");
    world.assert_fresh("both", &count_is(3));
    world.warm("both");

    world.scoped_insert(ALPHA, "urn:third");
    world.assert_fresh("both", &count_is(4));
}

#[test]
fn a_scoped_delete_and_a_drop_reach_the_graphs_own_reads() {
    let world = two_graphs();
    let beta = world.warm("beta");
    world.warm("alpha-info");
    let grant = Capability::scoped([cap_write_graph(ALPHA)]);

    world
        .sink(
            "urn:iki:store:graph-update",
            &[
                ("graph", ALPHA),
                ("content", &format!("DROP GRAPH <{ALPHA}>")),
            ],
            &grant,
        )
        .expect("the drop");

    world.assert_fresh("alpha-info", "quads: 0");
    world.assert_cached("beta", &beta);
}

// ------------------------------------------- every write that can change a read reaches it

#[test]
fn a_broad_update_reaches_a_cached_read_of_a_named_graph() {
    let world = two_graphs();
    world.warm("alpha");
    world.warm("alpha-info");

    world
        .sink(
            "urn:iki:store:update",
            &[(
                "content",
                &format!("INSERT DATA {{ GRAPH <{ALPHA}> {{ <urn:wide> <urn:p> <urn:o> }} }}"),
            )],
            &Capability::root(),
        )
        .expect("the broad update");

    world.assert_fresh("alpha", &count_is(2));
    world.assert_fresh("alpha-info", "quads: 2");
}

#[test]
fn a_broad_delete_naming_no_graph_in_its_text_reaches_every_named_graph() {
    let world = two_graphs();
    world.warm("alpha");
    world.warm("beta");

    world
        .sink(
            "urn:iki:store:update",
            &[(
                "content",
                "DELETE { GRAPH ?g { ?s ?p ?o } } WHERE { GRAPH ?g { ?s ?p ?o } }",
            )],
            &Capability::root(),
        )
        .expect("the broad delete");

    world.assert_fresh("alpha", &count_is(0));
    world.assert_fresh("beta", &count_is(0));
}

#[test]
fn a_load_reaches_a_cached_read_of_the_graph_it_loads_into() {
    let world = two_graphs();
    world.warm("alpha");

    world
        .sink(
            "urn:iki:store:load",
            &[
                ("content", "<urn:loaded> <urn:p> <urn:o> ."),
                ("format", "application/n-triples"),
                ("graph", ALPHA),
            ],
            &Capability::root(),
        )
        .expect("the load");

    world.assert_fresh("alpha", &count_is(2));
}

#[test]
fn a_load_of_a_quad_document_reaches_a_graph_it_names_only_in_its_body() {
    let world = two_graphs();
    world.warm("beta");

    world
        .sink(
            "urn:iki:store:load",
            &[
                (
                    "content",
                    &format!("<urn:loaded> <urn:p> <urn:o> <{BETA}> ."),
                ),
                ("format", "application/n-quads"),
            ],
            &Capability::root(),
        )
        .expect("the load");

    world.assert_fresh("beta", &count_is(2));
}

/// A whole-dataset read cannot be confined, so a write to any graph still reaches it, and
/// so does the listing a write can add a name to.
#[test]
fn a_scoped_write_still_reaches_reads_that_are_not_confined_to_a_graph() {
    let world = World::new(
        DurableStore::in_memory().unwrap(),
        vec![("broad", broad()), ("listing", listing())],
    );
    world.seed(ALPHA);
    world.warm("broad");
    world.warm("listing");

    world.scoped_insert(BETA, "urn:new");

    world.assert_fresh("broad", &count_is(2));
    world.assert_fresh("listing", BETA);
}

fn quad<'a>(graph: &'a str) -> QuadRef<'a> {
    QuadRef::new(
        NamedNodeRef::new("urn:raw").unwrap(),
        NamedNodeRef::new("urn:p").unwrap(),
        NamedNodeRef::new("urn:o").unwrap(),
        GraphNameRef::NamedNode(NamedNodeRef::new(graph).unwrap()),
    )
}

/// A handle that can write anywhere means no read can be cached, per-graph thread or not:
/// no cut can follow a write the kernel never saw.
#[test]
fn a_raw_write_through_a_shared_handle_is_seen_by_a_named_graph_read() {
    let (store, handle) = DurableStore::in_memory_shared().unwrap();
    let world = World::new(store, vec![("alpha", select_in(ALPHA))]);
    world.seed(ALPHA);
    assert!(world.read("alpha").0.contains(&count_is(1)));

    handle.insert(quad(ALPHA)).unwrap();

    world.assert_fresh("alpha", &count_is(2));
}

/// The sharer's declared graph stays uncached, every other graph stays cached and is still
/// cut by its own scoped writes, and a read spanning both goes with the uncovered member.
#[test]
fn a_declared_sharer_leaves_only_its_own_graph_uncached() {
    let (store, handle) = DurableStore::in_memory_shared_declaring(
        SharerWrites::only_the_default_graph().and_named_graph(BETA),
    )
    .unwrap();
    let both = format!("{ALPHA} {BETA}");
    let world = World::new(
        store,
        vec![
            ("alpha", select_in(ALPHA)),
            ("beta", select_in(BETA)),
            ("both", select_in(&both)),
        ],
    );
    world.seed(ALPHA);
    world.seed(BETA);

    world.warm("alpha");
    assert!(world.read("beta").0.contains(&count_is(1)));
    assert!(world.read("both").0.contains(&count_is(2)));

    handle.insert(quad(BETA)).unwrap();

    world.assert_fresh("beta", &count_is(2));
    world.assert_fresh("both", &count_is(3));

    world.scoped_insert(ALPHA, "urn:second");
    world.assert_fresh("alpha", &count_is(2));
}

// ----------------------------------------------------------------------- graph names

/// Graph names that an encoding could confuse. Each must get its own thread: a collision
/// would over-invalidate, and a name that failed to round-trip would go stale.
#[test]
fn awkward_graph_names_each_keep_their_own_freshness() {
    let names = [
        "urn:g/a",
        "urn:g%2Fa",
        "urn:g:a",
        "http://example.org/g?x=1&y=2#frag",
        "urn:caf\u{e9}",
        "urn:cafe",
    ];
    let reads: Vec<_> = names
        .iter()
        .enumerate()
        .map(|(i, graph)| (format!("g{i}"), select_in(graph)))
        .collect();
    let world = World::new(
        DurableStore::in_memory().unwrap(),
        reads
            .iter()
            .map(|(name, read)| (name.as_str(), read.clone()))
            .collect(),
    );
    for graph in names {
        world.seed(graph);
    }

    for (written, graph) in names.iter().enumerate() {
        let held: Vec<String> = (0..names.len())
            .map(|i| world.warm(&format!("g{i}")))
            .collect();
        world.scoped_insert(graph, "urn:second");
        for (other, answer) in held.iter().enumerate() {
            if other == written {
                world.assert_fresh(&format!("g{other}"), &count_is(2));
            } else {
                world.assert_cached(&format!("g{other}"), answer);
            }
        }
    }
}

#[test]
fn a_graph_thread_cannot_be_spelled_two_ways() {
    assert_eq!(
        graph_thread("urn:g/a"),
        "urn:iki:store:graph-written:urn%3Ag%2Fa"
    );
    assert_ne!(graph_thread("urn:g/a"), graph_thread("urn:g%2Fa"));
    assert_eq!(
        graph_thread("urn:caf\u{e9}"),
        "urn:iki:store:graph-written:urn%3Acaf%C3%A9"
    );
}

// ------------------------------------------------------------- the cut, and who may ask

/// ★ The cut AFTER the commit, and why the one before it is not enough. A read landing
/// between the two reads the old data and is cached; only the second cut takes it back.
/// The sharer's writer holds the write lock, so the scoped write waits in that window for as
/// long as this test needs.
#[test]
fn a_read_cached_while_a_scoped_write_waits_is_recomputed_after_it_commits() {
    let (store, _handle) =
        DurableStore::in_memory_shared_declaring(SharerWrites::only_the_default_graph()).unwrap();
    let writer = store.shared_writer().expect("a shared store has a writer");
    let world = World::new(store, vec![("alpha", select_in(ALPHA))]);
    world.seed(ALPHA);
    world.warm("alpha");

    let (held, wait_for_lock) = mpsc::channel();
    let (release, released) = mpsc::channel::<()>();
    std::thread::scope(|scope| {
        let sharer = scope.spawn(move || {
            writer.write_declared(move |_| {
                held.send(()).unwrap();
                released.recv().unwrap();
                Ok::<_, StorageError>(())
            })
        });
        wait_for_lock.recv().unwrap();
        let write = scope.spawn(|| world.scoped_insert(ALPHA, "urn:second"));

        // The first cut has happened once the warmed read recomputes. That recomputed read
        // is the one this test is about: pre-write data, cached while the write waits.
        let give_up = Instant::now() + Duration::from_secs(30);
        loop {
            let (answer, ran) = world.read("alpha");
            assert!(
                answer.contains(&count_is(1)),
                "the write landed early: {answer}"
            );
            if ran {
                break;
            }
            assert!(Instant::now() < give_up, "the first cut never happened");
            std::thread::sleep(Duration::from_millis(1));
        }

        release.send(()).unwrap();
        write.join().unwrap();
        sharer.join().unwrap().unwrap();
    });

    world.assert_fresh("alpha", &count_is(2));
}

/// ★ The write that could not tell its readers must not happen: a sub-request past the
/// nesting budget is refused, and without the check before the write this one would have
/// committed and left every cached read of the graph stale.
#[test]
fn a_scoped_write_that_could_not_be_announced_applies_nothing() {
    let store = DurableStore::in_memory().unwrap();
    let reader = store.reader();
    let kernel = Kernel::new(Arc::new(space(store))).with_max_depth(0);
    let request = Request::new(
        Verb::Sink,
        Iri::parse("urn:iki:store:graph-update").unwrap(),
    )
    .with_arg("graph", ArgRef::Inline(ALPHA.as_bytes().to_vec()))
    .with_arg(
        "content",
        ArgRef::Inline(
            format!("INSERT DATA {{ GRAPH <{ALPHA}> {{ <urn:s> <urn:p> <urn:o> }} }}").into_bytes(),
        ),
    );

    let refusal = block_on(kernel.issue(request, &Capability::root()))
        .expect_err("a write that cannot be announced");

    let text = refusal.to_string();
    assert!(text.contains("nothing was applied"), "{text}");
    assert!(text.contains(ALPHA), "{text}");
    assert_eq!(reader.snapshot_iter().count(), 0, "the write landed");
}

/// A detached invocation has no kernel, so no cache to invalidate: its write still applies.
#[test]
fn a_detached_scoped_write_still_applies() {
    let store = DurableStore::in_memory().unwrap();
    let reader = store.reader();
    let bound = space(store);
    let request = Request::new(
        Verb::Sink,
        Iri::parse("urn:iki:store:graph-update").unwrap(),
    )
    .with_arg("graph", ArgRef::Inline(ALPHA.as_bytes().to_vec()))
    .with_arg(
        "content",
        ArgRef::Inline(
            format!("INSERT DATA {{ GRAPH <{ALPHA}> {{ <urn:s> <urn:p> <urn:o> }} }}").into_bytes(),
        ),
    );
    let Resolution::Hit(hit) = bound.resolve(&request, &Scope::empty()) else {
        panic!("the door is bound");
    };
    let capability = Capability::root();
    let invocation = Invocation::detached(&request, &hit.bindings, &capability);

    block_on(hit.endpoint.invoke(&invocation)).expect("the detached write");

    assert_eq!(reader.snapshot_iter().count(), 1);
}

#[test]
fn invalidating_a_graph_needs_the_grant_that_writes_it() {
    let world = two_graphs();
    let alpha = world.warm("alpha");
    let target = graph_thread(ALPHA);

    for capability in [
        Capability::scoped(["urn:cap:store:write"]),
        Capability::scoped(Vec::<String>::new()),
    ] {
        let refusal = world
            .sink(&target, &[], &capability)
            .expect_err("no graph write grant");
        assert!(matches!(refusal, Error::Denied(_)), "{refusal:?}");
    }
    world.assert_cached("alpha", &alpha);

    // A grant for another graph reaches the endpoint, which refuses it by name. The kernel
    // still cuts the target of a write that ran and failed (ledger #1105), so this costs
    // alpha's readers one recomputation and no data, as a failed scoped write always has.
    let refusal = world
        .sink(&target, &[], &Capability::scoped([cap_write_graph(BETA)]))
        .expect_err("a grant for another graph");
    assert!(matches!(refusal, Error::Denied(_)), "{refusal:?}");
    assert!(refusal.to_string().contains(ALPHA), "{refusal}");
    world.assert_fresh("alpha", &count_is(1));
    world.warm("alpha");

    let receipt = world
        .sink(&target, &[], &Capability::scoped([cap_write_graph(ALPHA)]))
        .expect("the grant for this graph");
    assert_eq!(receipt, format!("invalidated <{ALPHA}>\n"));
    world.assert_fresh("alpha", &count_is(1));
}

#[test]
fn a_thread_name_that_graph_thread_would_not_write_is_refused_by_name() {
    let world = two_graphs();
    for suffix in ["urn:example:alpha", "urn%3aexample", "%FF"] {
        let refusal = world
            .sink(
                &format!("urn:iki:store:graph-written:{suffix}"),
                &[],
                &Capability::root(),
            )
            .expect_err(suffix);
        assert!(
            matches!(&refusal, Error::InvalidArgument { name, .. } if name == "graph"),
            "{suffix}: {refusal:?}"
        );
    }
}
