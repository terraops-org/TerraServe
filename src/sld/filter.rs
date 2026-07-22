// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! `<ogc:Filter>` AST + parser (comparison + logical operators).
//!
//! Task 1 defined the enums so `model::Rule.filter` compiles; Task 3 adds `parse_filter`, a
//! `roxmltree`-based, namespace-tolerant (local-name-only) recursive-descent parser over OGC
//! Filter Encoding 1.0/1.1 `<ogc:Filter>` documents. Unknown/unsupported operators, or a
//! comparison missing a required operand, return `None` rather than panicking — the same
//! lenient-interop stance `sld::parse` takes.
//!
//! **CORR-4 (whole-filter fail-open):** a child's parse failure inside an `And`/`Or`/`Not`
//! propagates to the WHOLE enclosing clause, which returns `None`. An earlier version silently
//! dropped unparseable `And`/`Or` children and kept the rest — but dropping a conjunct makes an
//! `And` match MORE features, and dropping a disjunct makes an `Or` match FEWER, with no signal
//! that anything was lost. Propagating `None` up to the top means a malformed `<Filter>` fails to
//! parse entirely → `Rule.filter` stays `None` → the rule matches all features (the documented,
//! consistent fail-open stance) instead of a clause being silently reshaped mid-tree.
//!
//! Like the rest of `sld::`, this file may depend on `std` + `roxmltree` ONLY. The generic
//! roxmltree helpers (`local_name`, `element_children`, `child`, `text`) live in the sibling
//! `super::xml` module (SIMP-7), shared with `parse`.

use super::xml::{child, element_children, local_name, text};

/// A parsed `<ogc:Filter>` expression tree (comparison + logical operators).
#[derive(Clone, Debug, PartialEq)]
pub enum SldFilter {
    Comparison {
        op: CompOp,
        prop: String,
        value: String,
    },
    Between {
        prop: String,
        lo: String,
        hi: String,
    },
    Like {
        prop: String,
        pattern: String,
    },
    IsNull {
        prop: String,
    },
    And(Vec<SldFilter>),
    Or(Vec<SldFilter>),
    Not(Box<SldFilter>),
}

/// Comparison operator for `SldFilter::Comparison`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompOp {
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
}

// -------------------------------------------------------------------------------------------
// Parser.
// -------------------------------------------------------------------------------------------

/// Parse an `<ogc:Filter>` element (or, recursively, one of its operator children) into an
/// `SldFilter`. Namespace-tolerant: every element is matched by *local* name only, so `ogc:`,
/// any other prefix, or no prefix at all are all accepted uniformly.
///
/// Returns `None` for an unrecognized operator, or a recognized operator missing a required
/// operand (e.g. a comparison with no `<Literal>`) — never panics.
///
/// `And`/`Or`/`Not` propagate any child's parse failure to the whole clause (CORR-4): if a child
/// element is PRESENT but fails to parse, the enclosing `And`/`Or`/`Not` returns `None` rather
/// than silently dropping that child and keeping the rest. A genuinely EMPTY `And`/`Or` (zero
/// element children — nothing present to fail) is a distinct case, unaffected by that
/// propagation: it parses to `Some(And(vec![]))`/`Some(Or(vec![]))`, the vacuous-match values
/// (`And([])` evals true, `Or([])` evals false).
pub fn parse_filter(node: roxmltree::Node) -> Option<SldFilter> {
    parse_filter_depth(node, 0)
}

/// Maximum `And`/`Or`/`Not`/`Filter` nesting depth (SEC-3). A deeply-nested filter would recurse
/// once here at parse time and again, per request, in `sld_lower::lower_filter` and
/// `Filter::eval` — three unbounded descents over the same tree. Capping at parse time keeps the
/// constructed IR inherently shallow, so the repeatable per-request eval/lower paths can never
/// overflow the stack on a pathological operator-supplied `.sld`. 64 is far past any real SLD
/// filter. (Note: `roxmltree`'s own DOM parse also recurses on element nesting, so a document deep
/// enough to threaten *that* fails at load in the dependency before this cap is reached — a
/// separate, load-time-only, operator-config concern this cap doesn't and can't address.)
const MAX_FILTER_DEPTH: usize = 64;

fn parse_filter_depth(node: roxmltree::Node, depth: usize) -> Option<SldFilter> {
    // Beyond the cap: fail this subtree to `None`. Combined with the whole-filter fail-open stance
    // (`Rule.filter == None` matches all features), a maliciously deep (but roxmltree-parseable)
    // document degrades to "no filter", never a per-request crash.
    if depth > MAX_FILTER_DEPTH {
        return None;
    }
    match local_name(node) {
        // The `<Filter>` wrapper itself: drill into its single operator child.
        "Filter" => element_children(node)
            .next()
            .and_then(|c| parse_filter_depth(c, depth + 1)),

        "And" => {
            let children: Vec<_> = element_children(node).collect();
            if children.is_empty() {
                Some(SldFilter::And(Vec::new()))
            } else {
                let items: Vec<SldFilter> = children
                    .into_iter()
                    .map(|c| parse_filter_depth(c, depth + 1))
                    .collect::<Option<_>>()?;
                Some(SldFilter::And(items))
            }
        }
        "Or" => {
            let children: Vec<_> = element_children(node).collect();
            if children.is_empty() {
                Some(SldFilter::Or(Vec::new()))
            } else {
                let items: Vec<SldFilter> = children
                    .into_iter()
                    .map(|c| parse_filter_depth(c, depth + 1))
                    .collect::<Option<_>>()?;
                Some(SldFilter::Or(items))
            }
        }
        "Not" => {
            // A `<Not>` with no element child at all (`element_children(node).next()` is `None`)
            // is treated the same as a `<Not>` whose single child fails to parse: `None`. There's
            // no vacuous case for `Not` (unlike empty `And`/`Or`) — a `<Not>` with nothing to
            // negate has no defined boolean value.
            let inner = element_children(node).next()?;
            parse_filter_depth(inner, depth + 1).map(|f| SldFilter::Not(Box::new(f)))
        }

        "PropertyIsEqualTo" => parse_comparison(node, CompOp::Eq),
        "PropertyIsNotEqualTo" => parse_comparison(node, CompOp::Ne),
        "PropertyIsLessThan" => parse_comparison(node, CompOp::Lt),
        "PropertyIsGreaterThan" => parse_comparison(node, CompOp::Gt),
        "PropertyIsLessThanOrEqualTo" => parse_comparison(node, CompOp::Le),
        "PropertyIsGreaterThanOrEqualTo" => parse_comparison(node, CompOp::Ge),

        "PropertyIsBetween" => parse_between(node),
        "PropertyIsLike" => parse_like(node),
        "PropertyIsNull" => parse_is_null(node),

        _ => None,
    }
}

fn parse_comparison(node: roxmltree::Node, op: CompOp) -> Option<SldFilter> {
    let prop = child(node, "PropertyName").and_then(text)?;
    let value = child(node, "Literal").and_then(text)?;
    Some(SldFilter::Comparison { op, prop, value })
}

fn parse_between(node: roxmltree::Node) -> Option<SldFilter> {
    let prop = child(node, "PropertyName").and_then(text)?;
    let lo = child(node, "LowerBoundary").and_then(boundary_text)?;
    let hi = child(node, "UpperBoundary").and_then(boundary_text)?;
    Some(SldFilter::Between { prop, lo, hi })
}

/// `LowerBoundary`/`UpperBoundary` each wrap an `ogc:Expression` — typically `<ogc:Literal>`.
/// Prefer that child's text; fall back to the boundary element's own direct text for a bare
/// (non-conformant but harmless) `<LowerBoundary>1</LowerBoundary>` shape.
fn boundary_text(node: roxmltree::Node) -> Option<String> {
    child(node, "Literal").and_then(text).or_else(|| text(node))
}

/// `<PropertyIsLike>` carries its `wildCard`/`singleChar`/`escapeChar` delimiters as *attributes
/// on the element itself* (not operands) — per OGC Filter Encoding they're required, but real
/// documents sometimes omit one, so an absent attribute defaults to the classic SQL/SLD triple
/// (`%`/`_`/`\`). The `<Literal>` pattern is normalized out of the document's own delimiter
/// convention into the engine's canonical one (`%`/`_`/`\`) here, at parse time, so every
/// downstream consumer (`sld_lower`, `Filter::Like`'s matcher) only ever sees canonical patterns.
fn parse_like(node: roxmltree::Node) -> Option<SldFilter> {
    let prop = child(node, "PropertyName").and_then(text)?;
    let raw_pattern = child(node, "Literal").and_then(text)?;
    let wild_card = like_delim_attr(node, "wildCard", '%');
    let single_char = like_delim_attr(node, "singleChar", '_');
    let escape_char = like_delim_attr(node, "escapeChar", '\\');
    let pattern = normalize_like_pattern(&raw_pattern, wild_card, single_char, escape_char);
    Some(SldFilter::Like { prop, pattern })
}

/// Read a `<PropertyIsLike>` delimiter attribute, taking only the first `char` of its value (the
/// OGC-intended shape is a single character; a multi-character value is out-of-spec — a defensive
/// best-effort read rather than a hard error). Falls back to `default` when the attribute is
/// absent or empty.
fn like_delim_attr(node: roxmltree::Node, attr: &str, default: char) -> char {
    node.attribute(attr)
        .and_then(|s| s.chars().next())
        .unwrap_or(default)
}

/// Normalize a `<PropertyIsLike>` source pattern — expressed in the document's own
/// `wild_card`/`single_char`/`escape_char` convention — into the engine's canonical
/// `Filter::Like` convention: `%` = any run of characters (incl. empty), `_` = any single
/// character, `\` escapes the next char as a literal. Walked char-by-char:
///   * source `escape_char` → the next source char is a literal: emitted as-is, EXCEPT if it is
///     itself one of the canonical metacharacters (`%`, `_`, `\`) it is emitted escaped (`\%`,
///     `\_`, `\\`) so the canonical reader doesn't misparse it.
///   * source `wild_card` char → emit canonical `%`.
///   * source `single_char` char → emit canonical `_`.
///   * any other char that happens to collide with a canonical metachar (e.g. a literal `%` in
///     the source when `wild_card` is something else, like `*`) → emitted escaped, same reasoning
///     as the escaped-literal case above.
///   * anything else → emitted as-is.
///
/// Defensive edge cases (deliberate, not spec-mandated): a trailing `escape_char` with nothing
/// following it is emitted as a literal `escape_char` (itself escaped if it collides with a
/// canonical metachar) rather than panicking or being dropped. If `wild_card`/`single_char`/
/// `escape_char` collide with each other, the escape check runs first, then wildcard, then
/// single-char, so escaping always wins and wildcard wins over single-char.
fn normalize_like_pattern(
    pattern: &str,
    wild_card: char,
    single_char: char,
    escape_char: char,
) -> String {
    let mut out = String::with_capacity(pattern.len());
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        if c == escape_char {
            let literal = chars.next().unwrap_or(escape_char);
            emit_canonical_literal(&mut out, literal);
        } else if c == wild_card {
            // Collapse consecutive wildcard `%` (SEC-2): they're semantically one, and a shorter
            // canonical pattern is cheaper to match. Only *wildcard* `%` merge — an escaped literal
            // `\%` (emitted by `emit_canonical_literal`) also ends in `%` but must be preserved.
            // (Cosmetic edge: after an escaped literal backslash `\\`, a following `%` isn't merged
            // because the `\\%` suffix looks like an escaped `\%` — at most one redundant `%`
            // survives, which the O(n·m) matcher handles identically. Harmless, not worth the extra
            // escape-state tracking to close.)
            if !(out.ends_with('%') && !out.ends_with("\\%")) {
                out.push('%');
            }
        } else if c == single_char {
            out.push('_');
        } else {
            emit_canonical_literal(&mut out, c);
        }
    }
    out
}

/// Emit `c` as a literal into the canonical pattern, backslash-escaping it first if it would
/// otherwise be misread as a canonical metacharacter (`%`, `_`, `\`).
fn emit_canonical_literal(out: &mut String, c: char) {
    if c == '%' || c == '_' || c == '\\' {
        out.push('\\');
    }
    out.push(c);
}

fn parse_is_null(node: roxmltree::Node) -> Option<SldFilter> {
    let prop = child(node, "PropertyName").and_then(text)?;
    Some(SldFilter::IsNull { prop })
}

// The generic roxmltree helpers this parser uses (`local_name`, `element_children`, `child`,
// `text`) live in `super::xml` (SIMP-7) — imported at the top of the file, shared with `parse`.
