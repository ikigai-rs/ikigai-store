//! `urn:iki:store:graph-info`: one named graph's size under the read grant for that graph
//! (ledger #147).
//!
//! `urn:iki:store:info` counts the whole store under the broad `urn:cap:store:read`, so a
//! tenant holding only `urn:cap:store:read:graph:<G>` could not ask how large its own graph
//! was. These pin what it counts (that graph, nothing else), who may ask (exactly that grant),
//! and that a write is seen (it is cached under the write threads, not frozen).

use futures::executor::block_on;
use ikigai_core::{ArgRef, Capability, Error, Iri, Kernel, Request, Verb};
use ikigai_store::{cap_read_graph, cap_write_graph, space, DurableStore, CAP_READ};
use std::sync::Arc;

const G: &str = "urn:x:info:g";
const OTHER: &str = "urn:x:info:other";

fn issue(
    kernel: &Kernel,
    cap: &Capability,
    verb: Verb,
    iri: &str,
    args: &[(&str, &str)],
) -> ikigai_core::Result<String> {
    let request = args.iter().fold(
        Request::new(verb, Iri::parse(iri).unwrap()),
        |request, (name, value)| request.with_arg(*name, ArgRef::Inline(value.as_bytes().to_vec())),
    );
    block_on(kernel.issue(request, cap)).map(|rep| String::from_utf8(rep.bytes).unwrap())
}

/// Three quads in `G`, two in `OTHER`, one in the default graph.
fn seeded() -> Kernel {
    let k = Kernel::with_meta_renderer(
        Arc::new(space(DurableStore::in_memory().unwrap())),
        Arc::new(ikigai_vocab::TurtleRenderer),
    );
    let doc = format!(
        "<urn:x:a> <urn:x:p> \"1\" <{G}> .\n<urn:x:b> <urn:x:p> \"2\" <{G}> .\n\
         <urn:x:c> <urn:x:p> \"3\" <{G}> .\n<urn:x:a> <urn:x:p> \"1\" <{OTHER}> .\n\
         <urn:x:b> <urn:x:p> \"2\" <{OTHER}> .\n<urn:x:d> <urn:x:p> \"default\" .\n"
    );
    issue(
        &k,
        &Capability::root(),
        Verb::Sink,
        "urn:iki:store:load",
        &[("content", &doc), ("format", "application/n-quads")],
    )
    .unwrap();
    k
}

fn info(k: &Kernel, cap: &Capability, graph: &str) -> ikigai_core::Result<String> {
    issue(
        k,
        cap,
        Verb::Source,
        "urn:iki:store:graph-info",
        &[("graph", graph)],
    )
}

#[test]
fn a_tenant_counts_its_own_graph_and_nothing_else() {
    let k = seeded();
    let tenant = Capability::scoped([cap_read_graph(G)]);
    assert_eq!(
        info(&k, &tenant, G).unwrap(),
        format!(
            "graph: <{G}>\nexists: true\nquads: 3\ngraph-update copy bound: {}\n",
            ikigai_store::limits::MAX_SCOPED_COPY_QUADS
        )
    );
}

#[test]
fn a_graph_that_does_not_exist_is_said_to_be_absent() {
    let k = seeded();
    let missing = "urn:x:info:missing";
    let tenant = Capability::scoped([cap_read_graph(missing)]);
    let answer = info(&k, &tenant, missing).unwrap();
    assert!(answer.contains("exists: false\nquads: 0\n"), "{answer}");
}

/// ★ Declared = enforced: a grant for ANOTHER graph passes the kernel's wildcard pre-check
/// and is refused here, naming the token; the broad grant does not satisfy the per-graph door.
#[test]
fn only_the_grant_for_that_graph_reads_its_size() {
    let k = seeded();
    let elsewhere = Capability::scoped([cap_read_graph(OTHER)]);
    let err = info(&k, &elsewhere, G).unwrap_err();
    let Error::Denied(why) = &err else {
        panic!("expected Denied, got {err:?}");
    };
    assert!(why.contains(&cap_read_graph(G)), "{why}");

    let broad = Capability::scoped([CAP_READ.to_string()]);
    assert!(
        matches!(info(&k, &broad, G), Err(Error::Denied(_))),
        "the broad read grant opened the per-graph door"
    );
}

/// A count read before a write is not served after it: the answer is cacheable under the
/// write threads, so a scoped write cuts it.
#[test]
fn a_write_is_counted() {
    let k = seeded();
    let cap = Capability::scoped([cap_read_graph(G), cap_write_graph(G)]);
    assert!(info(&k, &cap, G).unwrap().contains("quads: 3\n"));
    issue(
        &k,
        &cap,
        Verb::Sink,
        "urn:iki:store:graph-update",
        &[
            (
                "content",
                &format!("INSERT DATA {{ GRAPH <{G}> {{ <urn:x:e> <urn:x:p> \"4\" }} }}"),
            ),
            ("graph", G),
        ],
    )
    .unwrap();
    assert!(info(&k, &cap, G).unwrap().contains("quads: 4\n"));
}

#[test]
fn a_graph_that_is_not_an_iri_is_refused_by_name() {
    let k = seeded();
    let err = info(&k, &Capability::root(), "not an iri").unwrap_err();
    assert!(
        matches!(&err, Error::InvalidArgument { name, .. } if name == "graph"),
        "{err:?}"
    );
}
