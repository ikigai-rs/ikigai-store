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
| Last reviewed | 2026-11-12 |

## What it emits that people read

- **Short text/plain answers** (`src/endpoints.rs`): `urn:iki:store:info` (`backing: …`,
  `quads: …`, `covered: …`, and a `sharer writes: …` line when the host declared one),
  `urn:iki:store:graphs` (the graphs a caller may read, one IRI per line),
  `urn:iki:store:graph-info`, and the one-line receipts of the writing doors
  (`updated`, `updated <graph>: +<added> -<removed> quads`, `loaded <media type>: +<added>
  quads`, and without the numbers for a caller that may not read), and
  `urn:iki:store:graph-written:…`'s `invalidated <graph>`.
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

These are the wording rules for contributions; the checks below cover specific examples,
not every possible message:

- A capability refusal names what was asked (the graph, the door) and the exact grant that would
  allow it, and says what a broader grant does and does not cover. A refusal about a graph names
  the graph and never the data in it.
- A bound refusal names the argument or the bound and its number, says the answer was refused
  and not truncated, and says how the bound is raised (by the host, through a grant) or how to
  narrow the request.
- An argument that is present but unreadable is refused naming it, never treated as absent.
- A text answer is plain lines of `label: value` or one IRI per line; nothing is written in color.

## How it is tested

Every named check has a result: Pass, Fail or Blocked. Pass covers only the stated check,
not overall accessibility. The automated checks below use `cargo test --all-features`,
Cargo 1.99.0 (5f94df478 2026-08-27) and rustc 1.99.0 (b940084d7 2026-09-28), run on
2026-11-12 against the unchanged code from main commit
[`ea0626a2f140e1a2713c39bfbfb2ff9ce4a6e93a`](https://github.com/ikigai-rs/ikigai-store/commit/ea0626a2f140e1a2713c39bfbfb2ff9ce4a6e93a). [CI](.github/workflows/ci.yml) repeats the all-features tests.
They check output strings and behavior, not how a host presents the output or how a person
understands it. Ignored measurement tests are not evidence of a Pass.

| Check | Result | Evidence |
|---|---|---|
| Info labels and malformed-argument wording | Pass | `tests/endpoints.rs`: `info_reports_the_backing_and_whether_coverage_holds` and `malformed_input_names_the_argument_that_was_wrong`, through the command above. |
| Missing read grants name the required token | Pass | `tests/multi_graph_read.rs`: `one_missing_grant_refuses_the_read_and_names_the_missing_token`, through the command above. |
| Graph refusals name graphs without revealing statements | Pass | `tests/graph_scope.rs`: `the_refusal_names_the_graph_and_never_the_statement`; `tests/update_read_grant.rs`: `an_escape_refusal_names_graphs_and_never_data`, through the command above. |
| Out-of-scope update refusals name the graph and remedy | Pass | `tests/update_outside_graph.rs`: `a_delete_naming_another_graph_or_the_default_graph_is_refused` and `a_pattern_that_reads_the_default_graph_is_refused`, through the command above. |
| Remote-data and shared-writer refusals give a way forward | Pass | `tests/service_egress.rs`: `the_refusal_says_where_remote_data_comes_in_instead`; `tests/shared_writer.rs`: `the_raw_write_refusal_names_the_declaration_and_the_way_out`, through the command above. |
| Time-budget and overdue-cap wording | Pass | `tests/timeout_wording.rs` and `tests/overdue_cap_wording.rs`, through the command above. Checks names, limits and advice for store and consumer calls. |
| Running-update cap names the cap and allows recovery | Pass | `src/budget.rs`: `a_write_at_the_writer_cap_is_refused_naming_the_cap_and_the_writer_recovers`, through the command above. |
| Graph-written and cached-read invalidation refusals | Pass | `tests/graph_freshness.rs`, through the command above. Checks grants, graph names and refused writes when cached reads cannot be invalidated. |
| Literal-direction binding refusals and preservation | Pass | `tests/directional_bindings.rs`, through the command above with `rdf-12` enabled. Checks `InvalidArgument` on `bindings`, variable names, remedies and direction preservation; does not establish every downstream feature combination. |
| Info explains the meaning of `covered` in the answer | Fail | Source review on 2026-11-12: `InfoEndpoint` in `src/endpoints.rs` still emits `covered: true` or `covered: false` without an explanation. The endpoint description explains it. |
| Empty graph list explicitly says there are no readable named graphs | Fail | Source review on 2026-11-12: `GraphsEndpoint` in `src/endpoints.rs` builds an empty string when there are no names. An empty list does not mean an empty store. |
| Library-error messages always provide a remedy | Fail | Source review on 2026-11-12: `src/endpoints.rs` passes library errors through after `counting quads:`, `listing graphs:` or `looking up <…>:` without adding a remedy. |
| Refusal readability and action order across all messages | Blocked | The 2026-10-31 source review reported paragraph-length refusals with advice after reasoning. Some wording changed afterward; no complete current readability review or user evaluation was performed. CI string assertions do not establish comprehension. |
| Plain-text formatting and absence of color across all output | Blocked | The 2026-10-31 source review described plain labels and IRI lines without color; no complete current formatting audit was performed, including the measurement example. Individual info assertions above cover only their fixtures. |
| Browser presentation and manual keyboard use | Blocked | No browser or person's keyboard pass was run. This crate provides no host page or interactive terminal interface to evaluate; a host integration is needed to test presentation. |
| Automated accessibility checker | Blocked | No axe or other automated accessibility checker was run. No host page was configured for this evaluation; Rust wording tests are not an accessibility checker. |
| Screen reader | Blocked | No screen reader was used on this output; no screen-reader session with a host integration was available for this evaluation. |
| Screen magnifier | Blocked | No screen magnifier was used on this output; no magnifier session with a host integration was available for this evaluation. |
| Speech input | Blocked | No speech-input software was used on this output; no speech-input session with a host integration was available for this evaluation. |
| Switch access | Blocked | No switch-access software was used on this output; no switch-access session with a host integration was available for this evaluation. |
| Braille display | Blocked | No braille display was used on this output; no braille display with a host integration was available for this evaluation. |

## Known limitations

The Fail rows above record current source findings: unexplained `covered` values, silence for an
empty graph list, and library errors without a remedy. The historical readability finding
remains Blocked pending a fresh review; the wording changes recorded below do not establish
that every refusal is easy to understand. Browser and assistive-technology results remain
Blocked until someone evaluates the output in a host. None of the passing Rust tests evaluates
WCAG 2.2 AA conformance.

## Contributing

Read this file before changing any output listed above: a text answer or an error message. Keep a
refusal a sentence that names what was asked, what is missing and what to do, and keep the
wording tests in step on purpose. In the pull request, give each check a row in a
`Check | Result | Evidence` table: Pass, Fail or Blocked, with the command, tool and version,
or why it could not run. Keep unrun checks as Blocked, including assistive technology nobody
used. A historical run that CI does not repeat stays Blocked with its date.

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
| 2026-11-07 | Removed internal work-item numbers from refusals and moved the actions first in budget, copy-bound, and dump refusals. |
| 2026-11-09 | Added the `urn:iki:store:graph-written:…` receipt and refusals, and the scoped write's refusal when cached reads could not be invalidated. |
| 2026-11-11 | Added direction-preserving bindings and refusals of invalid or unsupported literal directions. |
| 2026-11-12 | Recorded Pass, Fail and Blocked results with commands, versions and scope; retained unrun assistive-technology checks and historical findings explicitly. |
