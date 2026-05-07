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
    pub list_items: Vec<ListItem>,
    pub links: Vec<Link>,
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
            "list_items",
            "links",
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
        };
        let json = serde_json::to_string(&ast).unwrap();
        let back: Ast = serde_json::from_str(&json).unwrap();
        assert_eq!(ast, back);
    }
}
