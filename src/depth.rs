//! A caller's RDF document is bounded in triple-term DEPTH before it is parsed (ledger #992).
//!
//! In a build with RDF 1.2 on (`oxigraph/rdf-12`, which `ikigai-cli` gets by feature
//! unification through rudof), a triple term may be the object of a triple term, and oxrdf
//! copies, renames and drops a nested one RECURSIVELY, once per level, inside the parser and
//! the loader. A stack overflow is not an error a caller gets back: Rust aborts the whole
//! process. `urn:iki:store:load` parsed caller documents inline on the caller's thread, under
//! the store's write lock, so ~1000 levels of `<<( … )>>` (about 22 KB of Turtle) aborted a
//! debug host on a 2 MiB thread, in every syntax the door loads (reproduced in a child
//! process, `tests/triple_term_nesting.rs`).
//!
//! So [`check_rdf_nesting`] scans the document for nesting BEFORE it is parsed, without a
//! stack of its own, and refuses past [`MAX_RDF_NESTING`] as a typed `InvalidArgument` naming
//! the argument that carried it. Length is not bounded: a document with a million shallow
//! triple terms is a large document, not a deep one, and the parser carries it.
//!
//! The pre-parse checks now live in `ikigai-rdf::depth`, shared with the other RDF
//! consumers. This module dispatches to those checks and keeps the store's public paths.
//! The Turtle reading is also used for N-Triples and N-Quads: where their strict parser
//! disagrees (for example, single-quoted strings), it stops at the first parse error.
//! `crate::limits::check_load` propagates that error before any statements are written;
//! the load path neither skips parse errors nor uses a bulk loader.
//!
//! ★ **The second half: a triple term an UPDATE builds** (ledger #1333). `INSERT DATA` text is
//! bounded by [`crate::limits::check_sparql`] (`<<( … )>>` and `<< … >>` count against this
//! module's [`MAX_RDF_NESTING`], ledger #1354), but an update can wrap
//! a STORED term in `TRIPLE(…)` or a `<<( … )>>` template and store the result, a few dozen
//! levels deeper a request, each request inside every text bound. oxigraph encodes, decodes and
//! serializes a stored triple term recursively, so the term grows until a request that touches
//! it aborts the host: a debug build aborted on the writer thread at about 11,800 levels
//! (`tests/triple_term_growth.rs`). So the store keeps an invariant: **no update leaves a
//! quad whose object nests deeper than [`MAX_RDF_NESTING`]**, the depth a loaded document may
//! have, checked before the update commits ([`nests_deeper_than_the_bound`], applied by
//! `crate::confine`). A refused update applies nothing.
//!
//! ⚠ What that does not reach: a host writing the store through its own handle (the
//! `Arc<Store>` a `DurableStore::*_shared` constructor returns, or a [`crate::SharedWriter`]
//! closure) writes what it builds, and can call [`nests_deeper_than_the_bound`] itself. And a
//! store that already holds a deeper term, written before this check, still aborts whatever
//! decodes it: oxigraph decodes a stored term recursively before any code here sees it, so
//! nothing here can refuse that read instead.

use ikigai_core::{Error, Result};
use oxigraph::io::RdfFormat;
use oxigraph::model::TermRef;

/// How deep a loaded document may nest RDF 1.2 triple terms: **64**, counted per `<<( … )>>` or
/// `<< … >>` in the Turtle family and per `rdf:parseType="Triple"` element in RDF/XML.
///
/// The same number as [`crate::limits::MAX_SPARQL_NESTING`] and `ikigai-rdf`'s
/// `MAX_TURTLE_NESTING`: one bound on caller nesting across the doors. Real data nests triple
/// terms a handful deep; a debug build loaded 300 levels on a 2 MiB thread and aborted at 1000.
pub const MAX_RDF_NESTING: usize = ikigai_rdf::MAX_TURTLE_NESTING;

/// Refuse an RDF document in `format` that nests triple terms deeper than [`MAX_RDF_NESTING`],
/// without parsing it, naming the argument `arg` that carried it.
///
/// The Turtle family uses [`ikigai_rdf::check_turtle_nesting`], and RDF/XML uses
/// [`ikigai_rdf::check_rdfxml_nesting`]. A format this scan does not know is refused
/// rather than passed: a door that loads a new syntax must teach the scan to read it first.
///
/// ```
/// use ikigai_store::depth::{check_rdf_nesting, MAX_RDF_NESTING};
/// use oxigraph::io::RdfFormat;
///
/// let nested = |n: usize| {
///     format!("<urn:s> <urn:p> {}<urn:o>{} .", "<<( <urn:s> <urn:p> ".repeat(n), " )>>".repeat(n))
/// };
/// assert!(check_rdf_nesting(nested(MAX_RDF_NESTING).as_bytes(), RdfFormat::Turtle, "content").is_ok());
/// let refusal = check_rdf_nesting(nested(MAX_RDF_NESTING + 1).as_bytes(), RdfFormat::NTriples, "content")
///     .unwrap_err()
///     .to_string();
/// assert!(refusal.contains("`content`") && refusal.contains("MAX_RDF_NESTING"));
/// // A `<<` inside a literal is text, not nesting.
/// let quoted = format!("<urn:s> <urn:p> \"{}\" .", "<<".repeat(100));
/// assert!(check_rdf_nesting(quoted.as_bytes(), RdfFormat::Turtle, "content").is_ok());
/// ```
pub fn check_rdf_nesting(text: &[u8], format: RdfFormat, arg: &str) -> Result<()> {
    let checked = match format {
        RdfFormat::Turtle | RdfFormat::TriG | RdfFormat::NTriples | RdfFormat::NQuads => {
            let text = std::str::from_utf8(text).map_err(|e| Error::InvalidArgument {
                name: arg.to_string(),
                detail: format!("this {} document is not UTF-8: {e}", format.name()),
            })?;
            ikigai_rdf::check_turtle_nesting(text, arg)
        }
        RdfFormat::RdfXml => ikigai_rdf::check_rdfxml_nesting(text, arg),
        other => Err(Error::InvalidArgument {
            name: arg.to_string(),
            detail: format!(
                "{} documents cannot be checked for triple-term nesting before parsing, so \
                 they are not loaded",
                other.name()
            ),
        }),
    };
    // Keep the public store bound's name in refusals, as well as its public constant.
    checked.map_err(|err| match err {
        Error::InvalidArgument { name, detail } => Error::InvalidArgument {
            name,
            detail: detail.replace("MAX_TURTLE_NESTING", "MAX_RDF_NESTING"),
        },
        other => other,
    })
}

/// Whether `term` nests RDF 1.2 triple terms deeper than [`MAX_RDF_NESTING`]: the check the
/// update doors make on every quad an update would add, before it commits (ledger #1333).
///
/// For a host that writes the store through its own handle and wants the same invariant: a
/// term deeper than this is one a later read of the store may not survive, since oxigraph
/// decodes and serializes a stored triple term recursively.
///
/// Read off the term's N-Triples form (`<<( s p o )>>` a level) with the scan
/// [`check_rdf_nesting`] uses, because naming the triple-term variant would not compile in a
/// build without RDF 1.2 (see the `rdf-12` gate in `Cargo.toml`). Writing that form recurses
/// once per level, which is safe for the terms this is meant for: anything an update builds
/// from a store that keeps the invariant is at most [`MAX_RDF_NESTING`] plus what one update's
/// text can add. An IRI, a blank node or a literal answers `false` without being written.
///
/// ```
/// use ikigai_store::depth::nests_deeper_than_the_bound;
/// use oxigraph::model::{Literal, NamedNode, TermRef};
///
/// assert!(!nests_deeper_than_the_bound(TermRef::from(&NamedNode::new("urn:o").unwrap())));
/// let quoted = Literal::new_simple_literal("<<( ".repeat(100));
/// assert!(!nests_deeper_than_the_bound(TermRef::from(&quoted)));
/// ```
pub fn nests_deeper_than_the_bound(term: TermRef<'_>) -> bool {
    if matches!(
        term,
        TermRef::NamedNode(_) | TermRef::BlankNode(_) | TermRef::Literal(_)
    ) {
        return false;
    }
    ikigai_rdf::check_turtle_nesting(&term.to_string(), "content").is_err()
}

/// Whether SPARQL update `text` could build an RDF 1.2 triple term, judged from the text
/// alone, erring toward yes: the gate on [`crate::confine::check_inserted`], which reads the
/// quads an update's `INSERT` templates can reach, so that an update that cannot build a
/// triple term does not pay for it.
///
/// The constructors are the `<<( … )>>` term, the `<< … >>` reified triple, an annotation
/// block `{| … |}`, a reifier `~`, and the `TRIPLE(…)` function, any of which can wrap a term
/// bound from the store. So: any `<<`, `{|` or `~`, the word `triple` in any case (and so in an
/// IRI or a string too), and any `\u`/`\U` escape, since spargebra's
/// `standard-unicode-escaping` feature rewrites those before tokenizing and a sibling crate
/// could turn it on. A false positive costs a read of the reachable quads; a false negative
/// would be the abort. Without a constructor an update can only copy a stored term, never
/// deepen one.
///
/// ⚠ **This is a COPY** of `ikigai-sparql`'s `stored::may_construct_triple_term`
/// (ikigai-rs/ikigai-linkeddata PR 41); ledger #976 is where the copies fold into one.
pub(crate) fn may_construct_triple_term(text: &str) -> bool {
    text.contains("<<")
        || text.contains("{|")
        || text.contains('~')
        || text.contains("\\u")
        || text.contains("\\U")
        || text.to_ascii_lowercase().contains("triple")
}

/// The refusal of an update that would leave a triple term deeper than [`MAX_RDF_NESTING`],
/// on argument `arg`.
pub(crate) fn grown_too_deep(arg: &str) -> Error {
    Error::InvalidArgument {
        name: arg.to_string(),
        detail: format!(
            "this update would leave the store holding an RDF 1.2 triple term nested deeper \
             than {MAX_RDF_NESTING} (MAX_RDF_NESTING), so nothing was applied: the store \
             encodes, decodes and serializes a triple term recursively, once per level, so a \
             deep enough one aborts the whole host on whatever request reads it, and a term \
             that grows by an update at a time has no other limit. Nest terms less deeply, or \
             reify instead (`<< … >>` names a triple with a fresh reifier rather than nesting \
             it)"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nested(n: usize) -> String {
        format!(
            "{}<urn:o>{}",
            "<<( <urn:s> <urn:p> ".repeat(n),
            " )>>".repeat(n)
        )
    }

    fn turtle(text: &str) -> Result<()> {
        check_rdf_nesting(text.as_bytes(), RdfFormat::Turtle, "content")
    }

    fn ntriples(text: &str) -> Result<()> {
        check_rdf_nesting(text.as_bytes(), RdfFormat::NTriples, "content")
    }

    fn rdfxml(text: &str) -> Result<()> {
        check_rdf_nesting(text.as_bytes(), RdfFormat::RdfXml, "content")
    }

    fn xml_nested(n: usize, attr: &str) -> String {
        format!(
            "<rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\
             <rdf:Description rdf:about=\"urn:s\">{}<ex:p rdf:resource=\"urn:o\"/>{}\
             </rdf:Description></rdf:RDF>",
            format!("<ex:p {attr}><rdf:Description rdf:about=\"urn:s\">").repeat(n),
            "</rdf:Description></ex:p>".repeat(n),
        )
    }

    #[test]
    fn the_bound_is_the_number_the_integration_tests_restate() {
        // `tests/triple_term_nesting.rs` says 64 rather than naming this.
        assert_eq!(MAX_RDF_NESTING, 64);
        assert_eq!(MAX_RDF_NESTING, crate::limits::MAX_SPARQL_NESTING);
    }

    #[test]
    fn triple_terms_are_refused_one_past_the_bound_and_pass_at_it_in_every_mode() {
        for check in [turtle, ntriples] {
            assert!(check(&format!("<urn:a> <urn:p> {} .", nested(MAX_RDF_NESTING))).is_ok());
            let err = check(&format!(
                "<urn:a> <urn:p> {} .",
                nested(MAX_RDF_NESTING + 1)
            ))
            .unwrap_err();
            assert!(
                matches!(&err, Error::InvalidArgument { name, .. } if name == "content"),
                "{err}"
            );
        }
        // A reified triple (`<< … >>`) counts the same.
        let reified = format!(
            "<urn:a> <urn:p> {}<urn:o>{} .",
            "<< <urn:s> <urn:p> ".repeat(MAX_RDF_NESTING + 1),
            " >>".repeat(MAX_RDF_NESTING + 1)
        );
        assert!(turtle(&reified).is_err());
    }

    #[test]
    fn closed_triple_terms_do_not_accumulate() {
        let flat = format!("<urn:a> <urn:p> {} .", vec![nested(2); 500].join(", "));
        assert!(turtle(&flat).is_ok());
        let lines = format!("<urn:a> <urn:p> {} .\n", nested(2)).repeat(500);
        assert!(ntriples(&lines).is_ok());
    }

    #[test]
    fn brackets_in_strings_iris_comments_and_escapes_are_not_nesting() {
        let deep = "<<".repeat(200);
        for text in [
            format!("<urn:a> <urn:p> \"{deep}\" ."),
            format!("<urn:a> <urn:p> '{deep}' ."),
            format!("<urn:a> <urn:p> \"\"\"x\"\"{deep}\n\"\"\" ."),
            format!("<urn:a> <urn:p> '''{deep}''' ."),
            format!("<urn:a> <urn:p> \"\\\"{deep}\" ."),
            format!("# {deep}\n<urn:a> <urn:p> <urn:b> ."),
            format!("<urn:a> <urn:p> <http://x/a'b#c> . # {deep}\n"),
            format!("ex:a\\' ex:p ex:b . # {deep}\n"),
            format!("<urn:a> <urn:p> \"\"\"a\"\"\"\", {deep}"),
            format!("<urn:a> <urn:p> <http://x/ {deep}"),
        ] {
            assert!(turtle(&text).is_ok(), "{text}");
        }
        // In N-Triples only `"` opens a string, so these are text in both readings that apply.
        for text in [
            format!("<urn:a> <urn:p> \"{deep}\" ."),
            format!("# {deep}\n"),
            format!("<urn:a> <urn:p> <urn:x{deep}> ."),
        ] {
            assert!(ntriples(&text).is_ok(), "{text}");
        }
    }

    #[test]
    fn nesting_after_a_string_or_iri_is_still_counted() {
        let over = nested(MAX_RDF_NESTING + 1);
        for text in [
            format!("<urn:a> <urn:p> \"<<\", '''>>''', <http://x/#>, {over} ."),
            format!("<urn:a> <urn:p> \"\"\"a\"\" b\"\"\", {over} ."),
            format!("<urn:a> <urn:p> <x\">, {over} . # \""),
            format!("<urn:a> <urn:p> \"x\n\", {over} ."),
        ] {
            assert!(turtle(&text).is_err(), "{text}");
        }
        assert!(ntriples(&format!("<urn:a> <urn:p> {over} . # '\n")).is_err());
        // The shared Turtle reading hides these openers in strings, but the strict
        // N-Triples parser errors at the quotes before it can build a nested term.
        for text in [
            format!("<urn:a> <urn:q> 'x . <urn:a> <urn:p> {over} . # '"),
            format!("<urn:a> <urn:p> \"\"\"\" {over} . # \"\"\""),
        ] {
            assert!(ntriples(&text).is_ok(), "{text}");
            assert_load_parse_error_and_empty(text.as_bytes(), "application/n-triples");
        }
    }

    #[test]
    fn rdfxml_counts_parse_type_triple_elements() {
        let triple = "rdf:parseType=\"Triple\"";
        assert!(rdfxml(&xml_nested(MAX_RDF_NESTING, triple)).is_ok());
        let err = rdfxml(&xml_nested(MAX_RDF_NESTING + 1, triple)).unwrap_err();
        assert!(
            matches!(&err, Error::InvalidArgument { name, .. } if name == "content"),
            "{err}"
        );
        // Any prefix, single quotes, spaces around `=`, and a value an entity could expand.
        for attr in [
            "x:parseType='Triple'",
            "rdf:parseType = \"Triple\"",
            "rdf:parseType=\"&t;\"",
            "rdf:parseType=\"&#84;riple\"",
        ] {
            assert!(
                rdfxml(&xml_nested(MAX_RDF_NESTING + 1, attr)).is_err(),
                "{attr}"
            );
        }
        // The three other parse types nest no triple term.
        for attr in [
            "rdf:parseType=\"Resource\"",
            "rdf:parseType='Collection'",
            "rdf:resource=\"urn:parseType\"",
        ] {
            assert!(rdfxml(&xml_nested(200, attr)).is_ok(), "{attr}");
        }
        // Closed elements do not accumulate.
        let flat = format!(
            "<rdf:RDF>{}</rdf:RDF>",
            "<ex:p rdf:parseType=\"Triple\"><rdf:Description/></ex:p>".repeat(500)
        );
        assert!(rdfxml(&flat).is_ok());
    }

    #[test]
    fn rdfxml_markup_that_quick_xml_skips_hides_nothing_it_reads() {
        let open = "<ex:p rdf:parseType=\"Triple\">".repeat(MAX_RDF_NESTING + 1);
        let close = "</ex:p>".repeat(MAX_RDF_NESTING + 1);
        // Skipped markup really is skipped…
        for text in [
            format!("<r><!-- {open} --></r>"),
            format!("<r><![CDATA[{open}]]></r>"),
            format!("<?pi {open} ?><r/>"),
            format!("<!DOCTYPE r [ <!ENTITY e \"{open}\"> ]><r/>"),
            // A value in `'…'` holding the `"`-quoted openers: one tag, self-closed.
            format!("<r a='{open}'/>"),
        ] {
            assert!(rdfxml(&text).is_ok(), "{text}");
        }
        // …and a closer inside it closes nothing, nor does a `>` inside a value end a tag.
        for text in [
            format!("{open}<!-- {close} -->{open}"),
            format!("{open}<![CDATA[{close}]]>{open}"),
            format!("<!-->{close}-->{open}"),
            format!("<!--->{close}-->{open}"),
            format!("<r a=\">\" {open}"),
            format!("<!DOCTYPE r [ <!ENTITY e '>'> ]>{open}"),
        ] {
            assert!(rdfxml(&text).is_err(), "{text}");
        }
    }

    #[test]
    fn rdfxml_utf16_passes_the_scan_but_fails_to_load() {
        let utf16: Vec<u8> = "<r/>".encode_utf16().flat_map(u16::to_le_bytes).collect();
        assert!(check_rdf_nesting(&utf16, RdfFormat::RdfXml, "content").is_ok());
        assert_load_parse_error_and_empty(&utf16, "application/rdf+xml");
    }

    fn assert_load_parse_error_and_empty(document: &[u8], media: &str) {
        use futures::executor::block_on;
        use ikigai_core::{ArgRef, Capability, Iri, Kernel, Request, Verb};
        use std::sync::Arc;

        let (store, dataset) = crate::DurableStore::in_memory_shared().unwrap();
        let kernel = Kernel::new(Arc::new(crate::space(store)));
        let request = Request::new(Verb::Sink, Iri::parse("urn:iki:store:load").unwrap())
            .with_arg("content", ArgRef::Inline(document.to_vec()))
            .with_arg("format", ArgRef::Inline(media.as_bytes().to_vec()));
        let err = block_on(kernel.issue(request, &Capability::root())).unwrap_err();
        assert!(
            matches!(&err, Error::InvalidArgument { name, detail }
                if name == "content" && detail.starts_with(&format!("parsing {media}:"))),
            "{err}"
        );
        assert!(dataset.is_empty().unwrap());
    }

    #[test]
    fn a_format_the_scan_cannot_read_is_refused() {
        let err = check_rdf_nesting(b"{}", RdfFormat::N3, "content").unwrap_err();
        assert!(
            matches!(&err, Error::InvalidArgument { name, .. } if name == "content"),
            "{err}"
        );
    }

    #[test]
    fn every_constructor_spelling_is_detected() {
        for text in [
            "INSERT { <urn:a> <urn:p> <<( <urn:s> <urn:p> ?t )>> } WHERE { <urn:a> <urn:p> ?t }",
            "INSERT { <urn:a> <urn:p> << <urn:s> <urn:p> ?t >> } WHERE { <urn:a> <urn:p> ?t }",
            "INSERT { <urn:s> <urn:p> ?t {| <urn:q> <urn:r> |} } WHERE { <urn:a> <urn:p> ?t }",
            "INSERT { <urn:s> <urn:p> ?t ~ <urn:r> } WHERE { <urn:a> <urn:p> ?t }",
            "INSERT { <urn:a> <urn:p> ?n } WHERE { <urn:a> <urn:p> ?t \
             BIND(tRiPlE(<urn:s>, <urn:p>, ?t) AS ?n) }",
            "INSERT { <urn:a> <urn:p> ?n } WHERE { <urn:a> <urn:p> ?t \
             BIND(\\u0054RIPLE(<urn:s>, <urn:p>, ?t) AS ?n) }",
        ] {
            assert!(may_construct_triple_term(text), "{text}");
        }
    }

    #[test]
    fn plain_updates_are_not_gated_in() {
        for text in [
            "INSERT DATA { <urn:s> <urn:p> \"o\" }",
            "DELETE WHERE { ?s ?p ?o }",
            "CLEAR ALL",
            "INSERT { ?s <urn:q> ?o } WHERE { ?s <urn:p> ?o FILTER(?o < 3) }",
        ] {
            assert!(!may_construct_triple_term(text), "{text}");
        }
    }

    #[test]
    fn the_refusal_names_the_argument_and_the_bound() {
        let err = grown_too_deep("content").to_string();
        assert!(
            err.starts_with("invalid argument `content`") && err.contains("MAX_RDF_NESTING"),
            "{err}"
        );
    }

    /// The stored-term check at and past the bound. Only where RDF 1.2 is on (this crate's
    /// `rdf-12` gate implies it), since nothing else can hold a triple term.
    #[cfg(feature = "rdf-12")]
    #[test]
    fn a_stored_term_is_measured_at_and_past_the_bound() {
        use oxigraph::model::{Literal, NamedNode, Term, Triple};
        // Built, not parsed: oxrdf's term parser refuses deep nesting itself.
        let wrap = |bottom: Term, n: usize| {
            let (s, p) = (
                NamedNode::new("urn:s").unwrap(),
                NamedNode::new("urn:p").unwrap(),
            );
            (0..n).fold(bottom, |t, _| Triple::new(s.clone(), p.clone(), t).into())
        };
        let term = |n: usize| wrap(NamedNode::new("urn:o").unwrap().into(), n);
        assert!(!nests_deeper_than_the_bound(term(1).as_ref()));
        assert!(!nests_deeper_than_the_bound(term(MAX_RDF_NESTING).as_ref()));
        assert!(nests_deeper_than_the_bound(
            term(MAX_RDF_NESTING + 1).as_ref()
        ));
        // A literal inside the term, carrying the opening text, is text.
        let quoted = wrap(Literal::new_simple_literal("<<( ".repeat(100)).into(), 3);
        assert!(!nests_deeper_than_the_bound(quoted.as_ref()));
    }
}
