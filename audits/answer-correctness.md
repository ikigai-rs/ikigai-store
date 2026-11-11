# Query and update answer audit

This audit uses public kernel doors over scratch in-memory stores and a temporary RocksDB
store. It does not use a production dataset. The tests are in
[`directional_bindings.rs`](../tests/directional_bindings.rs) and
[`update_answers.rs`](../tests/update_answers.rs).

## Fixed: bindings discarded a literal's direction

Before the fix, `bindings_json` serialized a directional language-tagged literal as an ordinary
language-tagged literal, losing `its:dir`. The query doors also ignored an explicitly supplied
`its:dir`, even an invalid value such as `"up"`. Thus a binding for `"text"@en--rtl` could
match `"text"@en` instead. The invalid-direction and round-trip tests failed against the
unchanged implementation before the fix.

Directional bindings now use the engine's SPARQL results serializer and parser. With RDF 1.2
results support enabled, both directions survive the round trip and match only the corresponding
stored literal, through broad and scoped SELECT. Without that support, a direction is refused
rather than discarded. Ordinary RDF 1.1 bindings keep their existing path. No dependency or
version changes are needed. The helpers do not depend on this crate's feature flag, so engine
features enabled by a consumer are honored as well.

Run the regression tests with:

```sh
cargo test --test directional_bindings
cargo test --all-features --test directional_bindings
```

## Unfixed: an empty transfer source leaves the destination graph absent

These requests succeed, but `urn:destination` is absent afterward:

```sparql
CREATE GRAPH <urn:empty>;
COPY GRAPH <urn:empty> TO GRAPH <urn:destination>
```

The same defect occurs with MOVE and ADD and with an empty default graph as source.
COPY and MOVE also remove an existing destination's registration when the source is empty.
The [COPY](https://www.w3.org/TR/sparql11-update/#copy),
[MOVE](https://www.w3.org/TR/sparql11-update/#move), and
[ADD](https://www.w3.org/TR/sparql11-update/#add) specifications say that the destination graph
is created if it does not exist. This store records empty graphs, so the distinction is
observable through `graphs`, `graph-info`, `GRAPH <urn:destination> {}`, and a later CREATE.

Run the three expected-to-fail reproductions explicitly:

```sh
cargo test --test update_answers empty_source -- --ignored --nocapture
```

They are marked ignored with the defect named in each reason. Green CI does **not** mean this
defect is fixed. Each reproduction checks both named and default empty sources and both absent
and existing destinations, collecting missing registrations before failing.

The cause is the `spargebra` parser's lowering of these operations: COPY becomes DROP SILENT
plus INSERT/WHERE, MOVE adds a source DROP, and ADD becomes INSERT/WHERE. None creates an empty
destination. An INSERT/WHERE with no solutions correctly creates no graph. By the time
`confine::parse` receives the algebra, it cannot distinguish a transfer from an explicitly
written primitive sequence. Repair requires preserving that distinction in the parser; guessing
from the lowered sequence would change valid INSERT/WHERE behavior. This audit does not vendor
the parser or add a second SPARQL text parser. Observed with spargebra 0.4.8 and oxigraph 0.5.13.

## Other checks

The active tests check:

- Exact COPY, MOVE, and ADD quad sets across named/default graph combinations, including
  transfers onto themselves; source registration after MOVE.
- INSERT DATA duplicate elimination, DELETE DATA, DELETE/INSERT/WHERE evaluation against
  the old state, and DELETE WHERE, including default-graph updates.
- CLEAR and DROP for DEFAULT, NAMED, ALL, and one named graph, including empty registrations
  and the continued availability of the default graph.
- Runtime failure in a later operation rolls back earlier deletes and inserts at both update
  doors; scoped write receipts, graph-info counts, and graph listings agree after changes.
- INSERT DATA blank nodes are shared within one operation and fresh between requests at both
  update doors.
- A scoped OPTIONAL over a variable naming an outside graph keeps its left row with no outside
  binding; EXISTS returns false. Scoped update EXISTS likewise stores false. These answers do
  not depend on whether the outside graph exists. Explicit outside graph names follow the
  documented policy of refusal based on syntax, independently of graph existence.
- RocksDB reopen preserves every stored quad's serialized terms and graph registrations,
  including custom datatypes, language tags, blank-node labels, escaped text, empty graphs,
  and, with RDF 1.2 enabled, direction tags and triple terms. A failed later request leaves
  the committed state intact, and a scoped commit survives a second reopen without an
  explicit flush.

The existing `graph_freshness` tests observe cache invocation counts: writes refresh affected
reads while unrelated scoped reads stay cached. The existing `confine` differential and
property tests compare partial-copy updates with whole-graph evaluation.

Limits: this is a bounded audit, not full SPARQL conformance. Reopen drops all handles between
operations; it does not simulate a killed process, power loss, or damaged storage. Persistence
compares stored terms before and after reopening, not input lexical forms that the engine
canonicalizes. Accessibility validation here consists of code and wording assertions; no
browser or assistive technology was used.
