//! Getting a **value** into a query without it becoming **syntax**.
//!
//! # ★ Why this module exists, and why it is not an escaper
//!
//! `urn:iki:store:update` takes a SPARQL string, and a consumer that holds
//! [`CAP_WRITE`](crate::CAP_WRITE) holds `DROP ALL`. So a comment body of
//! `" } ; DROP ALL ; INSERT DATA { <urn:x> <urn:y> "` interpolated naively is not a
//! rendering bug — it is the keys to the store. `ikigai-ledger` wrote its own
//! `literal()` and a hostile-content test for exactly this; `ikigai-cli`'s binding arc
//! was the second consumer to face it. **One of them will get it wrong, and the failure
//! is silent and total.**
//!
//! ⚠ **Nothing here escapes anything.** That is the point. Every function builds an
//! [`oxigraph::model::Term`] — a *parsed* RDF term with no syntax left in it — and then
//! asks **oxigraph** to serialize it. The only correct escaper for a grammar is the one
//! that owns the grammar, so this crate does not write a second one and neither should a
//! consumer. `NamedNode::new` rejects `>`, a space and `{` at construction;
//! `Literal`'s `Display` escapes `\`, `"`, `\n`, `\r`, `\t` and every other control
//! character as `\uXXXX`. Both are pinned by tests in this file, so an upstream change
//! that weakened either would fail here rather than in a consumer.
//!
//! # Which door to use
//!
//! | you are writing | use |
//! | --- | --- |
//! | a **query** (SELECT/ASK/CONSTRUCT/DESCRIBE) | `bindings=` — see [`bindings_json`]. The value never reaches the parser at all. |
//! | an **update** | the term constructors below. |
//!
//! The asymmetry is upstream's, not a choice: oxigraph's `PreparedSparqlQuery` has
//! `substitute_variable` and its `PreparedSparqlUpdate` has nothing, and there is no
//! public access to a parsed update's AST from outside the crate. [`bindings_json`]
//! documents the consequence; `urn:iki:store:update` **refuses** a `bindings` argument
//! rather than accepting one it would have to implement by rewriting text.
//!
//! ```
//! use ikigai_store::sparql::{iri, literal};
//!
//! let update = format!(
//!     "INSERT DATA {{ {} <http://purl.org/dc/terms/title> {} }}",
//!     iri("urn:example:item:7", "about")?,
//!     literal(r#"" } ; DROP ALL ; INSERT DATA { <urn:x> <urn:y> ""#),
//! );
//! // The literal opens and the injected quote is immediately escaped, so the statement
//! // never closes early and `DROP ALL` is just some characters in a title.
//! assert!(update.contains(r#""\" } ; DROP ALL ;"#));
//! # Ok::<_, ikigai_core::Error>(())
//! ```

use ikigai_core::{Error, Result};

/// The term types, re-exported so a consumer names ONE `Term`.
///
/// Same argument as [`crate::Store`]: Rust unifies `oxigraph::model::Term` across crates
/// only when every crate in the build resolves the same `oxigraph`, and depending on
/// these rather than adding a direct `oxigraph` dependency makes that alignment
/// structural instead of coincidental. [`bindings_json`] takes a `Term`, so without this
/// a consumer would have to take that dependency to call it.
pub use oxigraph::model::{Literal, NamedNode, Term};

/// A plain (`xsd:string`) literal, serialized by oxigraph.
///
/// ```
/// use ikigai_store::sparql::literal;
/// assert_eq!(literal("a \"b\" c"), r#""a \"b\" c""#);
/// assert_eq!(literal("line\nbreak"), r#""line\nbreak""#);
/// ```
pub fn literal(text: &str) -> String {
    Literal::new_simple_literal(text).to_string()
}

/// A literal with a datatype IRI, e.g. `xsd:dateTime`.
///
/// The datatype must be a valid IRI; `arg` names the argument a refusal is about.
///
/// ```
/// use ikigai_store::sparql::typed_literal;
/// assert_eq!(
///     typed_literal("2026-09-13T00:00:00Z", "http://www.w3.org/2001/XMLSchema#dateTime", "filed")?,
///     r#""2026-09-13T00:00:00Z"^^<http://www.w3.org/2001/XMLSchema#dateTime>"#
/// );
/// # Ok::<_, ikigai_core::Error>(())
/// ```
pub fn typed_literal(text: &str, datatype: &str, arg: &str) -> Result<String> {
    let dt = NamedNode::new(datatype).map_err(|e| Error::InvalidArgument {
        name: arg.to_string(),
        detail: format!("`{datatype}` is not a datatype IRI: {e}"),
    })?;
    Ok(Literal::new_typed_literal(text, dt).to_string())
}

/// A language-tagged literal. An ill-formed BCP 47 tag is **refused**, not dropped —
/// storing the same text under no language is a different statement.
///
/// ```
/// use ikigai_store::sparql::lang_literal;
/// assert_eq!(lang_literal("bonjour", "fr", "title")?, r#""bonjour"@fr"#);
/// assert!(lang_literal("bonjour", "not a tag", "title").is_err());
/// # Ok::<_, ikigai_core::Error>(())
/// ```
pub fn lang_literal(text: &str, tag: &str, arg: &str) -> Result<String> {
    Literal::new_language_tagged_literal(text, tag)
        .map(|l| l.to_string())
        .map_err(|e| Error::InvalidArgument {
            name: arg.to_string(),
            detail: format!("`{tag}` is not a language tag: {e}"),
        })
}

/// An IRI term, `<…>`, **after** checking that it really is an IRI.
///
/// ⚠ Validation, not escaping, and the difference matters: an IRI has no escape for
/// `>`, so a value carrying one cannot be written at all and must be refused. Stripping
/// or percent-encoding it here would store a *different* IRI than the caller named,
/// silently — which is the same failure class as an injection, arriving by politeness.
///
/// ```
/// use ikigai_store::sparql::iri;
/// assert_eq!(iri("urn:example:a", "about")?, "<urn:example:a>");
/// assert!(iri("urn:example:a>b", "about").is_err());
/// # Ok::<_, ikigai_core::Error>(())
/// ```
pub fn iri(value: &str, arg: &str) -> Result<String> {
    Ok(named_node(value, arg)?.to_string())
}

/// An `xsd:integer` term.
pub fn integer(value: i64) -> String {
    Term::from(Literal::from(value)).to_string()
}

/// An `xsd:boolean` term.
pub fn boolean(value: bool) -> String {
    Term::from(Literal::from(value)).to_string()
}

/// Any already-built [`Term`], serialized the one correct way.
///
/// The general case behind every constructor above, for a consumer that already holds
/// oxigraph terms (a row read back from `urn:iki:store:select`, say).
pub fn term(value: &Term) -> String {
    value.to_string()
}

/// A `bindings=` argument for the query endpoints, from name → term pairs.
///
/// ★ **This is the door that makes injection unrepresentable rather than merely
/// handled.** The bytes of a bound value never pass through the SPARQL parser: the
/// endpoint parses this JSON into [`Term`]s and hands them to oxigraph's
/// `substitute_variable`, so the query's syntax tree is fixed before any value is in
/// sight. [`literal`] and its siblings are the fallback for updates, where upstream
/// offers no such door.
///
/// The shape is the term object of the [SPARQL 1.1 Query Results JSON
/// Format](https://www.w3.org/TR/sparql11-results-json/) — the same shape a row comes
/// back in — so a value read out of one query binds into the next without conversion.
///
/// ⚠ **Every bound variable must appear in the query's projection.** `SELECT ?s WHERE
/// { ?s ?p ?o }` cannot bind `o`; `SELECT * WHERE { ?s ?p ?o }` can, and ASK, CONSTRUCT
/// and DESCRIBE (which have no projection to restrict) can bind any variable in the
/// pattern. That is oxigraph's rule, it is **enforced by oxigraph**, and a binding the
/// query does not mention is therefore *refused* rather than silently ignored — see
/// [`crate::endpoints`] for why refusing is the right half of that choice.
///
/// ⚠ **An RDF-star quoted triple panics.** It has no term object in the shape above, and
/// the endpoint that parses this argument could not rebuild one, so there is nothing
/// honest to emit; see `term_to_json` for why that arm exists at all and why it is a
/// catch-all. The variant is only reachable in a build where some crate has enabled
/// `oxrdf/rdf-12`.
///
/// ```
/// // The term types come from here too, so a consumer needs no `oxigraph` dependency
/// // of its own — and therefore cannot end up with a second, incompatible `Term`.
/// use ikigai_store::sparql::{bindings_json, Literal, Term};
///
/// let json = bindings_json([("title", Term::from(Literal::new_simple_literal("a \" b")))]);
/// assert_eq!(json, r#"{"title":{"type":"literal","value":"a \" b"}}"#);
/// ```
pub fn bindings_json<'a, I>(pairs: I) -> String
where
    I: IntoIterator<Item = (&'a str, Term)>,
{
    let map: serde_json::Map<String, serde_json::Value> = pairs
        .into_iter()
        .map(|(name, term)| (name.to_string(), term_to_json(&term)))
        .collect();
    serde_json::Value::Object(map).to_string()
}

fn term_to_json(term: &Term) -> serde_json::Value {
    use serde_json::json;
    match term {
        Term::NamedNode(n) => json!({"type": "uri", "value": n.as_str()}),
        Term::BlankNode(b) => json!({"type": "bnode", "value": b.as_str()}),
        Term::Literal(l) => match (l.language(), l.datatype()) {
            (Some(tag), _) => json!({"type": "literal", "value": l.value(), "xml:lang": tag}),
            (None, dt) if dt.as_str() == XSD_STRING => {
                json!({"type": "literal", "value": l.value()})
            }
            (None, dt) => {
                json!({"type": "literal", "value": l.value(), "datatype": dt.as_str()})
            }
        },
        // ★ THIS ARM IS THE FIX FOR 0.2.2, WHICH DID NOT COMPILE FOR A REAL CONSUMER.
        //
        // What was here was a comment saying `Term` has exactly three variants "in the
        // build this crate pins", because the RDF-star triple term is "behind a feature
        // nothing here enables". Both halves were true of THIS crate's dependency graph
        // and neither was true of a consumer's: cargo feature unification is global, so
        // `oxrdf/rdf-12` turned on by ANY sibling in the build adds `Term::Triple` and
        // this match stops being exhaustive. `ikigai-cli` is such a build (rudof, through
        // `ikigai-shacl`), and 0.2.2 therefore failed to compile there with E0004 —
        // published, and broken for everyone downstream of it.
        //
        // ⚠ **Exhaustiveness over a third-party enum with feature-gated variants is not a
        // property a downstream crate can assert at all.** There is no build in which all
        // of `Term`'s variants are simultaneously visible-and-required, and a crate cannot
        // `cfg` on a dependency's feature, so no arm and no `cfg` can make the match both
        // total and portable. A catch-all is not a preference here; it is the only shape
        // that compiles in both configurations.
        //
        // So the original reasoning survives where it can: a wildcard must not COERCE. The
        // failure it guarded against was a fourth variant silently becoming a
        // wrong-but-plausible literal, and this arm refuses instead — loudly, naming the
        // term. A quoted triple has no term object in the SPARQL 1.1 Query Results shape
        // this module speaks, and — the part that settles it — `parse_bindings` could not
        // rebuild one even if this side invented a shape, because constructing
        // `Term::Triple` needs a feature this crate does not control. Emitting a shape we
        // can never accept would break exactly the round-trip `bindings_json` promises
        // (`bindings_json_round_trips_through_parse_bindings` is the pin).
        //
        // It panics because the signature is infallible and 0.2.3 is a patch: making this
        // fallible would change `bindings_json`'s return type and break every consumer to
        // fix a case none of them can currently reach. If a non-fatal path is ever wanted,
        // the additive shape is a `try_bindings_json() -> Result<String>` sibling.
        #[allow(unreachable_patterns)] // reachable only when a sibling crate enables `oxrdf/rdf-12`
        other => panic!(
            "ikigai-store: `bindings` cannot carry {other}. The SPARQL results term object \
             has shapes for an IRI, a blank node and a literal and nothing else, and the \
             endpoint that parses this argument can only rebuild those three — a shape \
             emitted here that cannot be read back there is not a binding, it is a \
             plausible-looking lie. Bind the parts separately, or put the term into the \
             query text with `ikigai_store::sparql::term`, which serializes any term \
             oxigraph can build"
        ),
    }
}

const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";
const XSD_INTEGER: &str = "http://www.w3.org/2001/XMLSchema#integer";
const XSD_DOUBLE: &str = "http://www.w3.org/2001/XMLSchema#double";
const XSD_BOOLEAN: &str = "http://www.w3.org/2001/XMLSchema#boolean";

/// Parse a `bindings=` argument into variable → term pairs.
///
/// Accepted for each value, all of them unambiguous because JSON's own types are:
///
/// | JSON | term |
/// | --- | --- |
/// | `"text"` | a plain (`xsd:string`) literal — the common case, written as itself |
/// | `12` | `xsd:integer` |
/// | `1.5` | `xsd:double` |
/// | `true` | `xsd:boolean` |
/// | `{"type":"uri","value":"urn:…"}` | an IRI |
/// | `{"type":"literal","value":"…","datatype":"…"}` | a typed literal |
/// | `{"type":"literal","value":"…","xml:lang":"en"}` | a language-tagged literal |
///
/// Everything else is **refused with the reason**, never coerced:
/// - `null` — an unbound variable is not a binding. A query that should match anything
///   must simply not bind that variable.
/// - an array or a nested object — there is no term it could mean.
/// - `{"type":"bnode"}` — a blank node is scoped to the document that minted it, so
///   binding one names nothing the store can match. Skolemize (this ecosystem does
///   everywhere) and bind the IRI.
pub(crate) fn parse_bindings(json: &str) -> Result<Vec<(String, Term)>> {
    let value: serde_json::Value = serde_json::from_str(json).map_err(|e| {
        bad(format!(
            "not JSON: {e}. `bindings` is a JSON object of variable name → value, e.g. \
         {{\"title\": \"some text\"}}"
        ))
    })?;
    let serde_json::Value::Object(map) = value else {
        return Err(bad(
            "must be a JSON OBJECT of variable name → value, e.g. {\"title\": \"some text\"}"
                .to_string(),
        ));
    };
    map.into_iter()
        .map(|(name, value)| {
            // `Variable::new` would accept the name later; catching it here names the
            // offending key, which the evaluator's error does not.
            if name.is_empty() || !name.chars().all(|c| c.is_alphanumeric() || c == '_') {
                return Err(bad(format!(
                    "`{name}` is not a SPARQL variable name (letters, digits and `_`); write it \
                     without the leading `?`"
                )));
            }
            let term = json_to_term(&name, value)?;
            Ok((name, term))
        })
        .collect()
}

fn json_to_term(name: &str, value: serde_json::Value) -> Result<Term> {
    use serde_json::Value;
    match value {
        Value::String(s) => Ok(Literal::new_simple_literal(s).into()),
        Value::Bool(b) => Ok(Literal::new_typed_literal(b.to_string(), xsd(XSD_BOOLEAN)).into()),
        Value::Number(n) => {
            let datatype = if n.is_f64() { XSD_DOUBLE } else { XSD_INTEGER };
            Ok(Literal::new_typed_literal(n.to_string(), xsd(datatype)).into())
        }
        Value::Null => Err(bad(format!(
            "`{name}` is null, and an unbound variable is not a binding: a query that should \
             match anything must simply not bind that variable"
        ))),
        Value::Array(_) => Err(bad(format!(
            "`{name}` is an array, and there is no RDF term that could mean. Bind one value, or \
             write the alternatives into the query as a VALUES block"
        ))),
        Value::Object(o) => object_to_term(name, o),
    }
}

fn object_to_term(name: &str, object: serde_json::Map<String, serde_json::Value>) -> Result<Term> {
    let string = |key: &str| object.get(key).and_then(|v| v.as_str()).map(str::to_string);
    let kind = string("type").ok_or_else(|| {
        bad(format!(
            "`{name}` is an object with no `type`: use the SPARQL results term shape, \
             {{\"type\":\"uri\"|\"literal\",\"value\":…}}, or just a bare JSON string for a \
             plain literal"
        ))
    })?;
    let lexical = string("value")
        .ok_or_else(|| bad(format!("`{name}` is a `{kind}` term with no `value`")))?;
    match kind.as_str() {
        "uri" | "iri" => Ok(named_node(&lexical, "bindings")
            .map_err(|e| bad(format!("`{name}`: {e}")))?
            .into()),
        "literal" | "typed-literal" => match (
            string("xml:lang").or_else(|| string("lang")),
            string("datatype"),
        ) {
            (Some(_), Some(_)) => Err(bad(format!(
                "`{name}` carries both `xml:lang` and `datatype`; a literal has one or the other"
            ))),
            (Some(tag), None) => Literal::new_language_tagged_literal(lexical, &tag)
                .map(Into::into)
                .map_err(|e| bad(format!("`{name}`: `{tag}` is not a language tag: {e}"))),
            (None, Some(dt)) => Ok(Literal::new_typed_literal(
                lexical,
                named_node(&dt, "bindings")
                    .map_err(|e| bad(format!("`{name}`'s datatype: {e}")))?,
            )
            .into()),
            (None, None) => Ok(Literal::new_simple_literal(lexical).into()),
        },
        "bnode" => Err(bad(format!(
            "`{name}` is a blank node, which is scoped to the document that minted it and so \
             names nothing this store can match. Skolemize it — as everything in this ecosystem \
             does — and bind the IRI"
        ))),
        other => Err(bad(format!(
            "`{name}` has type `{other}`; one of `uri`, `literal`"
        ))),
    }
}

fn xsd(iri: &str) -> NamedNode {
    NamedNode::new_unchecked(iri)
}

fn named_node(value: &str, arg: &str) -> Result<NamedNode> {
    NamedNode::new(value).map_err(|e| Error::InvalidArgument {
        name: arg.to_string(),
        detail: format!(
            "`{value}` is not an IRI ({e}). It is refused rather than escaped because an IRI has \
             no escape for `<`, `>`, `\"`, `{{`, `}}`, `|`, `^`, `` ` ``, `\\` or a space — \
             percent-encode it, since quietly storing a different IRI is worse than a refusal"
        ),
    })
}

fn bad(detail: String) -> Error {
    Error::InvalidArgument {
        name: "bindings".to_string(),
        detail,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ★ The test this module exists for, inherited from `ikigai-ledger` so the next
    /// consumer gets the proof and not just the helper. A comment body that tries to
    /// close the literal and start a new statement must come back as ONE literal.
    #[test]
    fn an_injected_literal_cannot_escape_its_quotes() {
        let hostile = r#"" } ; DROP ALL ; INSERT DATA { <urn:x> <urn:y> ""#;
        let escaped = literal(hostile);
        let unescaped_quotes = escaped
            .char_indices()
            .filter(|(i, c)| *c == '"' && (*i == 0 || escaped.as_bytes()[i - 1] != b'\\'))
            .count();
        assert_eq!(unescaped_quotes, 2, "escaped form was {escaped}");
        assert!(escaped.starts_with('"') && escaped.ends_with('"'));
        assert!(escaped[1..escaped.len() - 1].contains("\\\""));
    }

    #[test]
    fn a_backslash_is_escaped_before_a_quote_can_hide_behind_it() {
        assert_eq!(literal("a\\\"b"), r#""a\\\"b""#);
    }

    /// ⚠ Pins an upstream property this module RELIES on rather than implements: a raw
    /// control character in a query is accepted by some parsers and rejected by others,
    /// and "accepted by some" is how a round-trip silently loses a byte. If oxigraph
    /// ever stopped escaping these, this fails here instead of in a consumer.
    #[test]
    fn oxigraph_escapes_control_characters_for_us() {
        assert_eq!(literal("a\u{7}b"), "\"a\\u0007b\"");
        assert_eq!(literal("a\u{0}b"), "\"a\\u0000b\"");
        assert_eq!(literal("a\nb\tc\rd"), r#""a\nb\tc\rd""#);
    }

    /// ⚠ The other upstream property: `NamedNode::new` REFUSES the characters an IRI
    /// term cannot carry, so `iri` never has to strip or encode anything.
    #[test]
    fn an_iri_with_a_closing_bracket_is_refused_not_mangled() {
        for hostile in ["urn:x:a>b", "urn:x:a b", "urn:x:a{b}", "urn:x:a\"b"] {
            assert!(iri(hostile, "about").is_err(), "accepted {hostile}");
        }
        assert_eq!(
            iri("urn:repo:file:ikigai-cli/src/main.rs", "about").unwrap(),
            "<urn:repo:file:ikigai-cli/src/main.rs>"
        );
    }

    #[test]
    fn scalars_carry_their_datatypes() {
        assert_eq!(
            integer(3),
            r#""3"^^<http://www.w3.org/2001/XMLSchema#integer>"#
        );
        assert_eq!(
            boolean(true),
            r#""true"^^<http://www.w3.org/2001/XMLSchema#boolean>"#
        );
    }

    #[test]
    fn a_bare_json_string_is_a_plain_literal() {
        let bound = parse_bindings(r#"{"title": "a \" b"}"#).unwrap();
        assert_eq!(bound.len(), 1);
        assert_eq!(bound[0].0, "title");
        assert_eq!(bound[0].1.to_string(), r#""a \" b""#);
    }

    #[test]
    fn json_scalars_become_typed_literals() {
        let bound = parse_bindings(r#"{"n": 3, "x": 1.5, "b": true}"#).unwrap();
        let rendered: Vec<String> = bound.iter().map(|(_, t)| t.to_string()).collect();
        assert!(rendered.contains(&integer(3)), "{rendered:?}");
        assert!(rendered.contains(&boolean(true)), "{rendered:?}");
        assert!(
            rendered
                .iter()
                .any(|t| t.ends_with("XMLSchema#double>") && t.starts_with("\"1.5\"")),
            "{rendered:?}"
        );
    }

    #[test]
    fn the_results_json_term_shapes_round_trip() {
        let bound = parse_bindings(
            r#"{"s": {"type":"uri","value":"urn:example:a"},
                "d": {"type":"literal","value":"3","datatype":"http://www.w3.org/2001/XMLSchema#integer"},
                "l": {"type":"literal","value":"bonjour","xml:lang":"fr"}}"#,
        )
        .unwrap();
        let by_name: std::collections::BTreeMap<_, _> =
            bound.into_iter().map(|(n, t)| (n, t.to_string())).collect();
        assert_eq!(by_name["s"], "<urn:example:a>");
        assert_eq!(by_name["d"], integer(3));
        assert_eq!(by_name["l"], r#""bonjour"@fr"#);
    }

    /// Every refusal states what to do instead; none of them coerces.
    #[test]
    fn unbindable_values_are_refused_with_the_reason() {
        for (json, expect) in [
            (r#"{"x": null}"#, "not a binding"),
            (r#"{"x": [1,2]}"#, "no RDF term"),
            (r#"{"x": {"type":"bnode","value":"b0"}}"#, "Skolemize"),
            (r#"{"x": {"value":"no type"}}"#, "no `type`"),
            (
                r#"{"x": {"type":"uri","value":"not an iri at all"}}"#,
                "not an IRI",
            ),
            (
                r#"{"?x": "leading question mark"}"#,
                "without the leading `?`",
            ),
            (r#"["not","an","object"]"#, "JSON OBJECT"),
            ("not json", "not JSON"),
        ] {
            let err = parse_bindings(json).unwrap_err().to_string();
            assert!(err.contains(expect), "{json} gave {err}");
        }
    }

    #[test]
    fn bindings_json_round_trips_through_parse_bindings() {
        let json = bindings_json([
            ("title", Term::from(Literal::new_simple_literal("a \" b"))),
            ("s", Term::from(NamedNode::new("urn:example:a").unwrap())),
            ("n", Term::from(Literal::from(3i64))),
        ]);
        let back: std::collections::BTreeMap<_, _> = parse_bindings(&json)
            .unwrap()
            .into_iter()
            .map(|(n, t)| (n, t.to_string()))
            .collect();
        assert_eq!(back["title"], r#""a \" b""#);
        assert_eq!(back["s"], "<urn:example:a>");
        assert_eq!(back["n"], integer(3));
    }

    /// ★ The fourth `Term` variant, in the ONLY build that can see it.
    ///
    /// ⚠ The `cfg` here is sound in the direction the arm's `cfg` would not be: this
    /// crate's `rdf-12` feature enables `oxigraph/rdf-12`, so if the feature is on the
    /// variant certainly exists. The converse fails — a consumer reaches the variant
    /// through rudof without this feature — which is why `term_to_json`'s arm is a
    /// catch-all and this test is not what protects that build. What this pins is the
    /// BEHAVIOUR of the arm: a refusal naming the term, never a binding.
    #[test]
    #[cfg(feature = "rdf-12")]
    #[should_panic(expected = "cannot carry")]
    fn a_quoted_triple_is_refused_and_never_coerced_into_a_literal() {
        use oxigraph::model::Triple;
        let quoted = Term::Triple(Box::new(Triple::new(
            NamedNode::new("urn:example:s").unwrap(),
            NamedNode::new("urn:example:p").unwrap(),
            NamedNode::new("urn:example:o").unwrap(),
        )));
        let _ = bindings_json([("t", quoted)]);
    }
}
