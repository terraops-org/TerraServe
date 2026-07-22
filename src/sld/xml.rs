// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! Generic `roxmltree` helpers shared by the SLD document parser (`parse`) and the `<ogc:Filter>`
//! parser (`filter`). Namespace-tolerant by design: every match is on the element's *local* name,
//! so `sld:`/`se:`/`ogc:`/unprefixed spellings are all accepted uniformly (SIMP-7: `parse` and
//! `filter` used to each carry byte-identical private copies of these).
//!
//! Like the rest of `src/sld/`, this file depends on `std` + `roxmltree` ONLY. The sibling import
//! (`use super::xml::*;`) stays within the `sld` module tree, so it satisfies `score.sh`'s
//! self-contained-module gate — which allows `super::` but not imports from the crate root.

use roxmltree::Node;

/// An element's local (namespace-stripped) name.
pub(crate) fn local_name<'a, 'input>(node: Node<'a, 'input>) -> &'a str {
    node.tag_name().name()
}

/// All direct element children, in document order.
pub(crate) fn element_children<'a, 'input>(
    node: Node<'a, 'input>,
) -> impl Iterator<Item = Node<'a, 'input>> {
    node.children().filter(|c| c.is_element())
}

/// All direct element children whose local name is `name`, in document order.
pub(crate) fn children_named<'a, 'input>(
    node: Node<'a, 'input>,
    name: &str,
) -> Vec<Node<'a, 'input>> {
    node.children()
        .filter(|c| c.is_element() && local_name(*c) == name)
        .collect()
}

/// The first direct element child whose local name is `name`.
pub(crate) fn child<'a, 'input>(node: Node<'a, 'input>, name: &str) -> Option<Node<'a, 'input>> {
    node.children()
        .find(|c| c.is_element() && local_name(*c) == name)
}

/// An element's direct text content, trimmed; `None` if absent or blank (e.g. an element whose
/// only child is itself an element, or a genuinely empty element).
pub(crate) fn text(node: Node) -> Option<String> {
    let t = node.text()?.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}
