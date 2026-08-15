// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! One XML escaper, shared by every protocol front-end.
//!
//! There used to be two, and they disagreed. `wms::xml_escape` covered `& < >`;
//! `wmts::escape_xml` covered `& < > "`. Neither escaped `'`. `wms.rs` called BOTH of them,
//! reaching into `crate::wmts::escape_xml` in two places while using its own elsewhere.
//!
//! That divergence was not cosmetic. The WEAKER escaper was the one building two ATTRIBUTE
//! values: `xlink:href="{href}"` in `wms::online_resource` (fed by `--public-url`) and
//! `code="{...}"` in `wms::exception_with_code`. A value containing `"` closed the attribute
//! early, so the rest of the URL was reinterpreted as further attributes.
//!
//! The lesson generalised: a text-position escaper and an attribute-position escaper that
//! look interchangeable will eventually be swapped by someone who does not know which is
//! which. Rather than document the distinction, this escapes the full set so BOTH positions
//! are safe with one function, and there is no second one to pick by mistake.
//!
//! `'` is escaped as the numeric `&#39;` rather than `&apos;`: `&apos;` is defined in XML but
//! NOT in HTML 4, and these documents are routinely parsed by clients using lenient HTML
//! parsers. The numeric reference is understood by both.

/// Escape a string for XML, safe in both text and attribute position.
///
/// `&` must be replaced FIRST, or the ampersands introduced by the later replacements would
/// themselves be escaped, producing `&amp;lt;` where `&lt;` was meant.
pub fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[cfg(test)]
mod tests {
    use super::escape;

    #[test]
    fn escapes_all_five_and_in_the_right_order() {
        assert_eq!(escape(r#"a&b<c>d"e'f"#), "a&amp;b&lt;c&gt;d&quot;e&#39;f");
    }

    /// The ordering trap: `&` first. If `<` were replaced before `&`, the `&` of the emitted
    /// `&lt;` would then be escaped again into `&amp;lt;`.
    #[test]
    fn ampersand_is_escaped_before_the_entities_it_would_corrupt() {
        assert_eq!(escape("<"), "&lt;");
        assert_eq!(
            escape("&lt;"),
            "&amp;lt;",
            "a literal '&lt;' must survive as text"
        );
        assert!(!escape("<").contains("&amp;lt;"));
    }

    /// Round trip: whatever goes in comes back out of an attribute unchanged. This is the
    /// property that actually matters at the `xlink:href="..."` call site.
    #[test]
    fn attribute_value_round_trips() {
        for raw in [
            r#"http://x/"><injected attr="#,
            "plain",
            "a'b",
            "ampersand & more",
            "",
        ] {
            let esc = escape(raw);
            assert!(!esc.contains('"'), "raw quote left in {esc}");
            assert!(!esc.contains('\''), "raw apostrophe left in {esc}");
            let back = esc
                .replace("&quot;", "\"")
                .replace("&#39;", "'")
                .replace("&lt;", "<")
                .replace("&gt;", ">")
                .replace("&amp;", "&");
            assert_eq!(back, raw, "round trip failed for {raw:?}");
        }
    }

    #[test]
    fn leaves_ordinary_text_untouched() {
        assert_eq!(escape("EPSG:3857"), "EPSG:3857");
        assert_eq!(escape("cos2023"), "cos2023");
    }
}
