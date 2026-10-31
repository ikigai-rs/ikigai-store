# Accessibility

What `ikigai-store` emits that people read, the rules that output follows, how it is tested, and
how to report a barrier. It answers SPARQL results and graphs, a few short text answers, and the
text of its errors and refusals; it writes no HTML, SVG or terminal output of its own. It is not a
conformance claim. Adapted from the
[ACCESSIBILITY.md template](https://github.com/mgifford/ACCESSIBILITY.md) (MIT).

| Field | Value |
|---|---|
| Target | WCAG 2.2 Level AA, for any page that presents this output; this repository writes text only |
| Status | Evaluation in progress. No conformance claim. |
| Owner | the maintainers of this repository |
| Report a barrier | [a GitHub issue](https://github.com/ikigai-rs/ikigai-store/issues/new?labels=accessibility) with the `accessibility` label; anything security-sensitive through [private vulnerability reporting](https://github.com/ikigai-rs/ikigai-store/security/advisories/new) |
| Last reviewed | 2026-10-31 |

## What it emits that people read

- **Short text/plain answers** (`src/endpoints.rs`): `urn:iki:store:info` (`backing: …`,
  `quads: …`, `covered: …`, and a `sharer writes: …` line when the host declared one),
  `urn:iki:store:graphs` (the graphs a caller may read, one IRI per line),
  `urn:iki:store:graph-info`, and the one-line receipts of the writing doors
  (`updated: <before> -> <after> quads`, `updated <graph>`, `loaded <media type>: <before> ->
  <after> quads`).
- **Query answers and dumps**: SPARQL result sets and graphs in the syntaxes each door declares,
  and `urn:iki:store:dump`'s N-Quads. They are data for programs that people also read.
- **Error and refusal messages**: the `Denied`, `InvalidArgument`, `Timeout`, `OverBudget` and
  other errors in `src/endpoints.rs`, `src/budget.rs`, `src/limits.rs`, `src/confine.rs`,
  `src/scope.rs`, `src/service.rs` and `src/depth.rs`, which a host, or a crate that uses this
  one's limits and budget, shows to whoever made the request.
- `examples/dump-memory.rs` prints timings to a terminal; it is a measurement, not a shipped
  binary.

Not this repository's: the page, error page or terminal a host shows any of this in.

## The rules that output follows

Followed by reading the source:

- A capability refusal names what was asked (the graph, the door) and the exact grant that would
  allow it, and says what a broader grant does and does not cover. A refusal about a graph names
  the graph and never the data in it.
- A bound refusal names the argument or the bound and its number, says the answer was refused
  and not truncated, and says how the bound is raised (by the host, through a grant) or how to
  narrow the request.
- An argument that is present but unreadable is refused naming it, never treated as absent.
- A text answer is plain lines of `label: value` or one IRI per line; nothing is written in color.

## How it is tested

- Wording is pinned by name in many tests, among them: `tests/endpoints.rs`
  (`info_reports_the_backing_and_whether_coverage_holds`,
  `malformed_input_names_the_argument_that_was_wrong`), `tests/multi_graph_read.rs`
  (`one_missing_grant_refuses_the_read_and_names_the_missing_token`), `tests/graph_scope.rs`
  (`the_refusal_names_the_graph_and_never_the_statement`), `tests/update_read_grant.rs`
  (`an_escape_refusal_names_graphs_and_never_data`), `tests/service_egress.rs`
  (`the_refusal_says_where_remote_data_comes_in_instead`), `tests/shared_writer.rs`
  (`the_raw_write_refusal_names_the_declaration_and_the_way_out`), and `tests/timeout_wording.rs`
  and `tests/overdue_cap_wording.rs`, which hold the time-budget refusals.
- **No automated accessibility checker, browser or assistive technology has been used on this
  output.**

## Known limitations

Found by reading the source on 2026-10-31, not by testing:

- Many refusals cite an internal work-item number in parentheses (in `src/budget.rs`,
  `src/confine.rs` and `src/endpoints.rs`), which a reader outside the project cannot look up.
- Several refusals run to a paragraph: what to do comes after the reasoning, and a reader
  listening to it hears the whole explanation first.
- `urn:iki:store:info`'s `covered: true` or `covered: false` names a term the answer does not
  explain; what it means (whether this process is the store's only writer) is in the endpoint's
  description, not the answer.
- `urn:iki:store:graphs` answers an empty body when there is nothing to list, so "no graphs" is
  said by silence.
- Some errors pass a library's message through after a stage name (`counting quads: …`,
  `listing graphs: …`, `looking up <…>: …`) and say nothing about what to do.

## Contributing

Read this file before changing any output listed above: a text answer or an error message. Keep a
refusal a sentence that names what was asked, what is missing and what to do, and keep the
wording tests in step on purpose. Say in the pull request what was tested, with what, and what
was not.

## Reporting a barrier

[Open an issue](https://github.com/ikigai-rs/ikigai-store/issues/new?labels=accessibility) with
the `accessibility` label (the link sets it when GitHub lets you add labels; otherwise a maintainer
adds it): what you were doing, what happened, and your terminal or browser and any assistive
technology, if you are comfortable sharing them. Anything security-sensitive goes through
[private vulnerability reporting](https://github.com/ikigai-rs/ikigai-store/security/advisories/new).
**We acknowledge a report within 7 days**, and promise nothing further.

## Change history

| Date | Change |
|---|---|
| 2026-10-31 | First version. |
