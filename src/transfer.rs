//! Preserve empty COPY/MOVE/ADD destinations lost by spargebra 0.4.8's lowering.
//!
//! A lowered transfer is indistinguishable from an explicit INSERT/WHERE, which must
//! not create a graph when its pattern matches nothing. Identify transfers in the source
//! syntax, using spargebra itself to validate operation boundaries and resolve names.
//! Keep the original AST (including blank-node identities) and add CREATE SILENT only
//! after the insertion of an actual, nonidentity transfer to a named graph.

use crate::budget::Deadline;
use ikigai_core::{Error, Result};
use spargebra::term::GraphNamePattern;
use spargebra::{GraphUpdateOperation, SparqlParser, Update};
use std::borrow::Cow;
use std::collections::VecDeque;

const OPERATIONS: &[&[u8]] = &[
    b"LOAD", b"CLEAR", b"DROP", b"ADD", b"MOVE", b"COPY", b"CREATE", b"INSERT", b"DELETE", b"WITH",
];

fn keyword(text: &[u8], at: usize) -> Option<&'static [u8]> {
    OPERATIONS.iter().copied().find(|word| {
        text.get(at..at + word.len())
            .is_some_and(|s| s.eq_ignore_ascii_case(word))
    })
}

/// Called only after the entire request has parsed and passed its algebra bound.
pub(crate) fn register_destinations(
    text: &str,
    update: &mut Update,
    deadline: &Deadline,
) -> Result<()> {
    let (scan, unicode) = scan_text(text);
    // Most updates need no extra scan or parse.
    if !scan.as_bytes().windows(3).any(|s| {
        s.eq_ignore_ascii_case(b"ADD")
            || s.eq_ignore_ascii_case(b"COP")
            || s.eq_ignore_ascii_case(b"MOV")
    }) {
        return Ok(());
    }
    let positions = crate::limits::top_level_positions(&scan, |at| {
        scan.as_bytes()[at] == b';' || keyword(scan.as_bytes(), at).is_some()
    })?;
    let parse = |part: &str| {
        // The text has already been preprocessed once. Shield each remaining backslash
        // so the parser's preprocessing restores it rather than decoding it again.
        let shielded;
        let part = if unicode {
            shielded = part.replace('\\', "\\u005c");
            &shielded
        } else {
            part
        };
        SparqlParser::new().parse_update(part)
    };
    // The parser accepts a prologue by itself. Prefix names that happen to contain an
    // operation keyword cannot end a valid prologue, so they cannot become operations.
    let mut start = None;
    for &at in &positions {
        if keyword(scan.as_bytes(), at).is_none() {
            continue;
        }
        deadline.check()?;
        if parse(&scan[..at]).is_ok_and(|prefix| prefix.operations.is_empty()) {
            start = Some(at);
            break;
        }
    }
    let Some(start) = start else { return Ok(()) };
    let prologue = &scan[..start];
    let mut cursor = start;
    let mut index = 0;
    let mut additions = Vec::new();
    for end in positions
        .iter()
        .copied()
        .filter(|&at| at >= start && scan.as_bytes()[at] == b';')
        .chain(std::iter::once(scan.len()))
    {
        deadline.check()?;
        let segment = &scan[cursor..end];
        let Ok(operation) = parse(&format!("{prologue}{segment}")) else {
            // A conservative lexical reading may suggest a semicolon that is actually
            // inside an expression or literal. Only the parser may split an operation.
            continue;
        };
        let first = skip_space_and_comments(&scan[cursor..end]);
        let kind = keyword(first.as_bytes(), 0);
        let insertion = match kind {
            Some(b"ADD") => Some(0),
            Some(b"COPY" | b"MOVE") => Some(1),
            _ => None,
        };
        if let Some(insertion) = insertion {
            // Identity transfers lower to no operations, including absent self-graphs.
            if !operation.operations.is_empty() {
                let expected = match kind {
                    Some(b"ADD") => 1,
                    Some(b"COPY") => 2,
                    _ => 3,
                };
                if operation.operations.len() != expected {
                    return Err(lowering_changed());
                }
                let Some(GraphUpdateOperation::DeleteInsert { insert, .. }) =
                    operation.operations.get(insertion)
                else {
                    return Err(lowering_changed());
                };
                let [quad] = insert.as_slice() else {
                    return Err(lowering_changed());
                };
                if let GraphNamePattern::NamedNode(graph) = &quad.graph_name {
                    // Compare the transfer's complete lowering with the original parse.
                    // Ordinary operation chunks are used only to count AST positions:
                    // reparsing their blank nodes could allocate different identities.
                    if update.operations.get(index..index + expected)
                        != Some(operation.operations.as_slice())
                    {
                        return Err(lowering_changed());
                    }
                    additions.push((index + insertion + 1, graph.clone()));
                }
            }
        }
        index += operation.operations.len();
        cursor = end + usize::from(end < scan.len());
    }
    if index != update.operations.len() || cursor != scan.len() {
        return Err(lowering_changed());
    }
    // Reverse insertion keeps every index relative to the original AST.
    for (at, graph) in additions.into_iter().rev() {
        update.operations.insert(
            at,
            GraphUpdateOperation::Create {
                silent: true,
                graph,
            },
        );
    }
    Ok(())
}

fn lowering_changed() -> Error {
    Error::InvalidArgument {
        name: "content".to_owned(),
        detail: "This graph transfer's parser representation is unsupported. Nothing was \
                 evaluated. Use CREATE SILENT, CLEAR, INSERT/WHERE, and DROP explicitly instead."
            .to_owned(),
    }
}

fn skip_space_and_comments(mut text: &str) -> &str {
    loop {
        text = text.trim_start_matches([' ', '\t', '\r', '\n']);
        if !text.starts_with('#') {
            return text;
        }
        text = text.find(['\r', '\n']).map_or("", |at| &text[at..]);
    }
}

/// Feature unification may enable spargebra's global Unicode preprocessing. Match its
/// character lookahead, including invalid escapes in comments: those are valid SPARQL
/// and must not turn into different operation boundaries. Like spargebra's
/// UnescapeUnicodeCharIterator, an invalid escape buffers the characters it looked at;
/// a buffered backslash still takes its lookahead from the underlying input iterator.
fn scan_text(text: &str) -> (Cow<'_, str>, bool) {
    if !(text.contains("\\u") || text.contains("\\U"))
        || SparqlParser::new().parse_update("\\u0043LEAR ALL").is_err()
    {
        return (Cow::Borrowed(text), false);
    }
    let mut decoded = String::with_capacity(text.len());
    let mut input = text.chars();
    let mut buffered = VecDeque::new();
    while let Some(c) = buffered.pop_front().or_else(|| input.next()) {
        if c != '\\' {
            decoded.push(c);
            continue;
        }
        let Some(next) = input.next() else {
            decoded.push('\\');
            continue;
        };
        buffered.push_back(next);
        let width = match next {
            'u' => 4,
            'U' => 8,
            _ => 0,
        };
        if width == 0 {
            decoded.push('\\');
            continue;
        }
        let mut complete = true;
        for _ in 0..width {
            if let Some(c) = input.next() {
                buffered.push_back(c);
            } else {
                complete = false;
                break;
            }
        }
        let hex: String = buffered.iter().skip(1).collect();
        let escaped = complete
            .then(|| u32::from_str_radix(&hex, 16).ok())
            .flatten()
            .and_then(char::from_u32);
        if let Some(c) = escaped {
            buffered.clear();
            decoded.push(c);
        } else {
            decoded.push('\\');
        }
    }
    (Cow::Owned(decoded), true)
}
