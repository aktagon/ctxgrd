//! Typed body AST. CORE-006 in the brief.
//!
//! A source that interprets its body as markdown-ish content publishes
//! an [`Ast`] on the envelope. The kernel threads the AST through to
//! the per-document `context` block on the rule's stdin (ADR-002
//! § RUL-002) so rules can consume it verbatim without re-parsing.
//! All position fields are 1-indexed; `0` means unknown.
//!
//! The struct / field / enum-variant names are the wire names — serde
//! round-trips directly to and from the JSON shape documented in
//! `docs/briefs/001-contextguard-kernel.md` Contracts section.

use serde::{Deserialize, Serialize};

/// Full parsed structural view of a document body.
///
/// Empty arrays, never omissions — the schema requires every top-level
/// key to be present when `ast` is populated. Every contained item
/// carries the line/col at which it starts in the original body.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ast {
    pub headings: Vec<Heading>,
    pub code_blocks: Vec<CodeBlock>,
    pub inline_code_spans: Vec<InlineCodeSpan>,
    pub strikethrough_spans: Vec<StrikethroughSpan>,
    pub cross_ref_tokens: Vec<CrossRefToken>,
    pub req_ref_tokens: Vec<CrossRefToken>,
    pub list_items: Vec<ListItem>,
    pub links: Vec<Link>,
    /// Image destinations (`![alt](path)`), same shape as [`Link`] with
    /// `text` holding the alt text. Separate from `links` because the two
    /// answer different questions — a rule may lint prose references
    /// without linting asset paths (ADR-125 § LNK-004).
    ///
    /// `serde(default)` so an external source written against the
    /// pre-ADR-125 envelope still deserialises; the field is always
    /// *emitted*, keeping the "empty arrays, never omissions" invariant
    /// this module's docs state.
    #[serde(default)]
    pub images: Vec<Link>,
    /// Reference-style links whose definition is missing — `[text][ref]`
    /// with no `[ref]: <url>` line. Such a link produces no [`Link`] at
    /// all (it renders as literal text), so it is invisible to anything
    /// reading `links` (ADR-125 § LNK-005).
    #[serde(default)]
    pub broken_refs: Vec<BrokenRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Heading {
    pub level: u8,
    pub text: String,
    pub line: u32,
    pub col: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeBlock {
    pub kind: CodeBlockKind,
    /// Language tag from a fenced block, if any. `None` for indented
    /// blocks and fenced blocks without a language string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lang: Option<String>,
    pub line_start: u32,
    pub line_end: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CodeBlockKind {
    Fenced,
    Indented,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InlineCodeSpan {
    pub line: u32,
    pub col_start: u32,
    pub col_end: u32,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StrikethroughSpan {
    pub line: u32,
    pub col_start: u32,
    pub col_end: u32,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrossRefToken {
    pub token: String,
    pub namespace: String,
    pub number: u32,
    pub line: u32,
    pub col: u32,
    pub in_code: bool,
    pub in_strikethrough: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListItem {
    pub line: u32,
    pub indent: u32,
    pub marker: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Link {
    pub href: String,
    pub text: String,
    pub line: u32,
    pub col: u32,
}

/// A reference-style link with no matching definition.
///
/// `reference` is the label as written — the `bar` in `[foo][bar]`, or
/// the `foo` in `[foo][]`. Bare shortcut links (`[foo]`) are deliberately
/// *not* recorded: prose in these repos is full of `[ADR-046]`-style
/// bracketed tokens that were never meant as links (ADR-125 § LNK-005).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrokenRef {
    pub reference: String,
    pub line: u32,
    pub col: u32,
    /// True when the reference was written `![alt][ref]` rather than
    /// `[text][ref]`. pulldown reports both as `LinkType::Reference`, so
    /// without this a consumer cannot honour an images-only switch.
    #[serde(default)]
    pub is_image: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_ast_serialises_with_all_required_keys() {
        let ast = Ast::default();
        let value: serde_json::Value = serde_json::to_value(&ast).unwrap();
        let obj = value.as_object().unwrap();
        for key in [
            "headings",
            "code_blocks",
            "inline_code_spans",
            "strikethrough_spans",
            "cross_ref_tokens",
            "req_ref_tokens",
            "list_items",
            "links",
            "images",
            "broken_refs",
        ] {
            assert!(obj.contains_key(key), "missing top-level key {key}");
            assert!(
                obj[key].as_array().unwrap().is_empty(),
                "{key} should be []"
            );
        }
    }

    #[test]
    fn code_block_kind_lowercase_wire_form() {
        let fenced = CodeBlock {
            kind: CodeBlockKind::Fenced,
            lang: Some("rust".into()),
            line_start: 1,
            line_end: 5,
        };
        let s = serde_json::to_string(&fenced).unwrap();
        assert!(s.contains("\"kind\":\"fenced\""), "got {s}");
        assert!(s.contains("\"lang\":\"rust\""), "got {s}");

        let bare = CodeBlock {
            kind: CodeBlockKind::Indented,
            lang: None,
            line_start: 2,
            line_end: 3,
        };
        let s = serde_json::to_string(&bare).unwrap();
        assert!(s.contains("\"kind\":\"indented\""), "got {s}");
        assert!(!s.contains("lang"), "None lang must not serialise; got {s}");
    }

    #[test]
    fn cross_ref_token_round_trips() {
        let t = CrossRefToken {
            token: "ADR-042".into(),
            namespace: "ADR".into(),
            number: 42,
            line: 17,
            col: 5,
            in_code: false,
            in_strikethrough: false,
        };
        let json = serde_json::to_string(&t).unwrap();
        let back: CrossRefToken = serde_json::from_str(&json).unwrap();
        assert_eq!(t, back);
    }

    #[test]
    fn ast_round_trips_through_json() {
        let ast = Ast {
            headings: vec![Heading {
                level: 2,
                text: "Status".into(),
                line: 11,
                col: 1,
            }],
            code_blocks: vec![CodeBlock {
                kind: CodeBlockKind::Fenced,
                lang: Some("rust".into()),
                line_start: 20,
                line_end: 25,
            }],
            inline_code_spans: vec![InlineCodeSpan {
                line: 15,
                col_start: 5,
                col_end: 20,
                text: "let x = 1".into(),
            }],
            strikethrough_spans: vec![StrikethroughSpan {
                line: 18,
                col_start: 10,
                col_end: 20,
                text: "ADR-404".into(),
            }],
            cross_ref_tokens: vec![CrossRefToken {
                token: "ADR-042".into(),
                namespace: "ADR".into(),
                number: 42,
                line: 17,
                col: 5,
                in_code: false,
                in_strikethrough: false,
            }],
            req_ref_tokens: vec![CrossRefToken {
                token: "FR-007".into(),
                namespace: "FR".into(),
                number: 7,
                line: 22,
                col: 18,
                in_code: false,
                in_strikethrough: false,
            }],
            list_items: vec![ListItem {
                line: 25,
                indent: 0,
                marker: "-".into(),
                text: "First item".into(),
            }],
            links: vec![Link {
                href: "../adrs/ADR-001.md".into(),
                text: "ADR-001".into(),
                line: 30,
                col: 3,
            }],
            images: vec![Link {
                href: "assets/rule_tour.json".into(),
                text: "the rule tour".into(),
                line: 34,
                col: 1,
            }],
            broken_refs: vec![BrokenRef {
                reference: "brief-001".into(),
                line: 38,
                col: 5,
                is_image: false,
            }],
        };
        let json = serde_json::to_string(&ast).unwrap();
        let back: Ast = serde_json::from_str(&json).unwrap();
        assert_eq!(ast, back);
    }

    #[test]
    fn ast_from_a_pre_adr125_envelope_still_deserialises() {
        // An external source written before `images`/`broken_refs` existed
        // omits both keys. `serde(default)` is what keeps its envelopes
        // valid; without it every such source breaks at the wire.
        let json = r#"{
            "headings": [], "code_blocks": [], "inline_code_spans": [],
            "strikethrough_spans": [], "cross_ref_tokens": [], "req_ref_tokens": [],
            "list_items": [], "links": []
        }"#;
        let ast: Ast = serde_json::from_str(json).expect("legacy envelope must deserialise");
        assert!(ast.images.is_empty());
        assert!(ast.broken_refs.is_empty());
    }
}
