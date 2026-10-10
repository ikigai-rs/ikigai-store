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
//! ★ **The scan mirrors the PARSER'S lexing, in the mode this crate parses in** — the lesson
//! of the same fix in `ikigai-shacl` (PR 17), where a scan that read the grammar rather than
//! the lexer could be bypassed. This crate parses STRICT (`RdfParser::from_format`, no
//! `lenient()`), with oxttl 0.2's lexer in its Turtle mode (Turtle, TriG) or its N-Triples mode
//! (N-Triples, N-Quads), and quick-xml 0.37 under oxrdfxml (RDF/XML). The reading only has to
//! agree with the parser up to the parser's FIRST error: `Store::load_from_reader` stops there
//! and oxttl returns an error before reading another token, so nothing after it is ever built.
//! Where the strict lexer and a lenient reading differ (a line end in a short string), the
//! strict one errors, so the lenient reading the scan uses is a safe one.
//!
//! ⚠ **This is a COPY.** `ikigai-shacl`'s `depth::check_turtle_nesting` is the same Turtle scan
//! (in lenient mode, for rudof), and ledger #976 is where the copies fold into one shared crate.
//! No published crate exports it in a form this one can take: `ikigai-shacl` would pull rudof
//! into a store. Keep the two Turtle readings in step until the fold.
//!
//! ⚠ Not bounded here: a triple term built by SPARQL rather than parsed. `INSERT DATA` text is
//! bounded by [`crate::limits::check_sparql`] (`<<` counts as nesting), but an update can wrap a
//! STORED term in `TRIPLE(…)` and store the result, one level deeper a call. See the README.

use ikigai_core::{Error, Result};
use oxigraph::io::RdfFormat;

/// How deep a loaded document may nest RDF 1.2 triple terms: **64**, counted per `<<( … )>>` or
/// `<< … >>` in the Turtle family and per `rdf:parseType="Triple"` element in RDF/XML.
///
/// The same number as [`crate::limits::MAX_SPARQL_NESTING`] and `ikigai-shacl`'s
/// `MAX_TURTLE_NESTING`: one bound on caller nesting across the doors. Real data nests triple
/// terms a handful deep; a debug build loaded 300 levels on a 2 MiB thread and aborted at 1000.
pub const MAX_RDF_NESTING: usize = 64;

/// Refuse an RDF document in `format` that nests triple terms deeper than [`MAX_RDF_NESTING`],
/// without parsing it, naming the argument `arg` that carried it.
///
/// Turtle and TriG are read as oxttl's Turtle-mode lexer reads them, N-Triples and N-Quads as
/// its N-Triples-mode lexer does (no `'` strings, no long strings), and RDF/XML as quick-xml
/// does. A format this scan does not know is refused rather than passed: a door that loads a
/// new syntax must teach the scan to read it first.
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
    let deeper = match format {
        RdfFormat::Turtle | RdfFormat::TriG => terse_exceeds(text, Lexer::Turtle),
        RdfFormat::NTriples | RdfFormat::NQuads => terse_exceeds(text, Lexer::NTriples),
        RdfFormat::RdfXml => xml_exceeds(text, arg)?,
        other => {
            return Err(Error::InvalidArgument {
                name: arg.to_string(),
                detail: format!(
                    "{} documents cannot be checked for triple-term nesting before parsing, so \
                     they are not loaded",
                    other.name()
                ),
            })
        }
    };
    if deeper {
        return Err(Error::InvalidArgument {
            name: arg.to_string(),
            detail: format!(
                "this {} document nests RDF 1.2 triple terms deeper than {MAX_RDF_NESTING} \
                 (MAX_RDF_NESTING), counting `<<( … )>>` and `<< … >>` (or \
                 `rdf:parseType=\"Triple\"` elements) outside literals, IRIs and comments: the \
                 RDF library copies a nested triple term recursively, once per level, where a \
                 stack overflow aborts the whole host. Nothing was loaded",
                format.name()
            ),
        });
    }
    Ok(())
}

/// Which of oxttl's lexer modes reads the text.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Lexer {
    /// Turtle and TriG: `"` and `'` strings, each short or long (tripled).
    Turtle,
    /// N-Triples and N-Quads: only short `"` strings. oxttl's N-Triples mode reads `'` as no
    /// string at all and `"""` as an empty string and then another (`lexer.rs`, the `b'"'` and
    /// `b'\''` arms).
    NTriples,
}

/// Whether Turtle-family `text` nests `<<` deeper than [`MAX_RDF_NESTING`].
///
/// The reading is oxttl 0.2's lexer (`lexer.rs`) in the parts that decide what is code:
/// strings closing at the FIRST unescaped delimiter (a long string at the first tripled one, a
/// short one even across a line end — where the strict lexer errors and the parse stops),
/// `<…>` IRIs as everything up to the first `>` (both modes stop there; strict validates after),
/// `#` comments to the line end, and `\`-escapes. So a `<<` inside a literal, an IRI or a
/// comment is text, not nesting.
///
/// Copied from `ikigai-shacl`'s `check_turtle_nesting` (fold: ledger #976), with the
/// N-Triples mode added.
fn terse_exceeds(b: &[u8], lexer: Lexer) -> bool {
    let at = |i: usize| b.get(i).copied();
    let mut depth = 0usize;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'#' => {
                while i < b.len() && b[i] != b'\n' && b[i] != b'\r' {
                    i += 1;
                }
            }
            q @ (b'"' | b'\'') if q == b'"' || lexer == Lexer::Turtle => {
                if lexer == Lexer::Turtle && at(i + 1) == Some(q) && at(i + 2) == Some(q) {
                    // A long string: ends at the first unescaped triple quote.
                    i += 3;
                    while i < b.len() {
                        if b[i] == b'\\' {
                            i += 2;
                        } else if b[i] == q && at(i + 1) == Some(q) && at(i + 2) == Some(q) {
                            i += 3;
                            break;
                        } else {
                            i += 1;
                        }
                    }
                } else {
                    // A short string: ends at the quote. The strict lexer stops at a line end
                    // with an error (and the parse with it); reading on through is the reading
                    // that consumed what the lexer's error token consumed.
                    i += 1;
                    while i < b.len() {
                        match b[i] {
                            b'\\' => i += 2,
                            c if c == q => {
                                i += 1;
                                break;
                            }
                            _ => i += 1,
                        }
                    }
                }
            }
            b'<' if at(i + 1) == Some(b'<') => {
                depth += 1;
                if depth > MAX_RDF_NESTING {
                    return true;
                }
                i += 2;
            }
            b'<' => {
                // An IRI: everything up to the first `>`, a `\` escaping what follows.
                let mut j = i + 1;
                while j < b.len() && b[j] != b'>' {
                    j += if b[j] == b'\\' { 2 } else { 1 };
                }
                if j >= b.len() {
                    // Never closed: the parser fails here, and builds nothing after it.
                    break;
                }
                i = j + 1;
            }
            b'>' if at(i + 1) == Some(b'>') => {
                depth = depth.saturating_sub(1);
                i += 2;
            }
            // `\` outside a string is a prefixed name's escape (`ex:a\#b`): skip what it escapes.
            b'\\' => i += 2,
            _ => i += 1,
        }
    }
    false
}

/// Whether RDF/XML `text` nests `parseType` elements that can make a triple term deeper than
/// [`MAX_RDF_NESTING`].
///
/// A triple term in RDF/XML is a property element with `rdf:parseType="Triple"`; its one node
/// element's properties are the next level down. So the depth is the number of such elements
/// open at once, read the way quick-xml 0.37 reads markup (`reader/mod.rs`, `parser/*.rs`):
///
/// - a comment ends at the first `-->` at least six bytes after its `<`, a CDATA section at
///   the first `]]>`, a processing instruction at the first `?>`, all quote-blind;
/// - a DOCTYPE ends at the `>` that balances its `<`s, quote-blind;
/// - a start or end tag ends at the first `>` outside a `"…"` or `'…'` attribute value.
///
/// An element counts when it carries an attribute whose local name is `parseType`, under any
/// prefix, and whose value is anything but `Literal`, `Resource` or `Collection` written out:
/// an entity reference may expand to `Triple` (oxrdfxml reads `<!ENTITY>` declarations), so a
/// value that could is counted. That over-counts only documents no real data looks like.
///
/// ⚠ The text must be UTF-8, which is all this build's quick-xml reads: a document with a NUL
/// byte (every UTF-16 one) is refused, since a scan of bytes cannot read it and a quick-xml
/// built with its `encoding` feature (unification again) could.
fn xml_exceeds(b: &[u8], arg: &str) -> Result<bool> {
    if b.contains(&0) || b.starts_with(&[0xFF, 0xFE]) || b.starts_with(&[0xFE, 0xFF]) {
        return Err(Error::InvalidArgument {
            name: arg.to_string(),
            detail: "this RDF/XML document is not UTF-8 (it has a NUL byte or a UTF-16 byte \
                     order mark): it is read as UTF-8 or not at all"
                .to_string(),
        });
    }
    let find = |from: usize, pat: &[u8]| -> Option<usize> {
        b.get(from..)?
            .windows(pat.len())
            .position(|w| w == pat)
            .map(|p| from + p)
    };
    // The open elements, each remembering whether it counted.
    let mut open: Vec<bool> = Vec::new();
    let mut depth = 0usize;
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'<' {
            i += 1;
            continue;
        }
        let end = match b.get(i + 1) {
            Some(b'!') => match b.get(i + 2) {
                // `<!--` … `-->`, whose `>` is at least six bytes on (`<!---->` is the shortest).
                Some(b'-') => find(i + 4, b"-->").map(|p| p + 2),
                Some(b'[') => find(i + 3, b"]]>").map(|p| p + 2),
                Some(b'D' | b'd') => {
                    let mut balance = 0usize;
                    let mut end = None;
                    for (j, &c) in b.iter().enumerate().skip(i + 2) {
                        if c == b'<' {
                            balance += 1;
                        } else if c == b'>' {
                            if balance == 0 {
                                end = Some(j);
                                break;
                            }
                            balance -= 1;
                        }
                    }
                    end
                }
                // Not markup quick-xml reads: it fails here.
                _ => None,
            },
            Some(b'?') => find(i + 1, b"?>").map(|p| p + 1),
            Some(_) => {
                let mut quote = None;
                let mut end = None;
                for (j, &c) in b.iter().enumerate().skip(i + 1) {
                    match (quote, c) {
                        (None, b'>') => {
                            end = Some(j);
                            break;
                        }
                        (None, b'"' | b'\'') => quote = Some(c),
                        (Some(q), c) if c == q => quote = None,
                        _ => {}
                    }
                }
                let Some(end) = end else { break };
                let tag = &b[i + 1..end];
                if tag.first() == Some(&b'/') {
                    // An end tag closes the innermost element; one with nothing open is an
                    // error the parser stops at.
                    match open.pop() {
                        Some(true) => depth -= 1,
                        Some(false) => {}
                        None => break,
                    }
                } else {
                    let counts = may_be_triple(tag);
                    if counts {
                        depth += 1;
                        if depth > MAX_RDF_NESTING {
                            return Ok(true);
                        }
                    }
                    // A self-closing element opens and closes at once.
                    if tag.last() == Some(&b'/') {
                        if counts {
                            depth -= 1;
                        }
                    } else {
                        open.push(counts);
                    }
                }
                Some(end)
            }
            None => None,
        };
        match end {
            Some(end) => i = end + 1,
            // Unclosed markup: the parser fails here, and builds nothing after it.
            None => break,
        }
    }
    Ok(false)
}

/// Whether a start tag's text (between `<` and `>`) carries a `parseType` attribute whose value
/// may be `Triple`. Every `parseType` followed by `=` and a quoted value is looked at, wherever
/// it sits, so one inside another attribute's value over-counts and nothing under-counts.
fn may_be_triple(tag: &[u8]) -> bool {
    const NAME: &[u8] = b"parseType";
    let space = |c: u8| matches!(c, b' ' | b'\t' | b'\r' | b'\n');
    let mut from = 0;
    while let Some(p) = tag
        .get(from..)
        .and_then(|t| t.windows(NAME.len()).position(|w| w == NAME))
    {
        let start = from + p;
        from = start + NAME.len();
        // A local name: at the start of the attribute name or right after its prefix's `:`.
        if start == 0 || !(space(tag[start - 1]) || tag[start - 1] == b':') {
            continue;
        }
        let mut j = from;
        while j < tag.len() && space(tag[j]) {
            j += 1;
        }
        if tag.get(j) != Some(&b'=') {
            continue;
        }
        j += 1;
        while j < tag.len() && space(tag[j]) {
            j += 1;
        }
        let Some(&q @ (b'"' | b'\'')) = tag.get(j) else {
            continue;
        };
        let value_start = j + 1;
        let value_end = tag[value_start..]
            .iter()
            .position(|&c| c == q)
            .map_or(tag.len(), |e| value_start + e);
        let value = &tag[value_start..value_end];
        if !matches!(value, b"Literal" | b"Resource" | b"Collection") {
            return true;
        }
    }
    false
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
        // N-Triples mode: a `'` opens nothing, and `"""` is an empty string then another —
        // so neither hides what follows the way the Turtle reading would let it.
        for text in [
            format!("<urn:a> <urn:p> {over} . # '\n"),
            format!("<urn:a> <urn:q> 'x . <urn:a> <urn:p> {over} . # '"),
            format!("<urn:a> <urn:p> \"\"\"\" {over} . # \"\"\""),
        ] {
            assert!(ntriples(&text).is_err(), "{text}");
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
    fn rdfxml_that_is_not_utf8_is_refused() {
        let utf16: Vec<u8> = "<r/>".encode_utf16().flat_map(u16::to_le_bytes).collect();
        assert!(check_rdf_nesting(&utf16, RdfFormat::RdfXml, "content").is_err());
    }

    #[test]
    fn a_format_the_scan_cannot_read_is_refused() {
        let err = check_rdf_nesting(b"{}", RdfFormat::N3, "content").unwrap_err();
        assert!(
            matches!(&err, Error::InvalidArgument { name, .. } if name == "content"),
            "{err}"
        );
    }
}
