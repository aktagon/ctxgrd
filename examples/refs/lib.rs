// Demo Rust file for the reference scanner.
//
// Resolved: PMR-001 exists at examples/pmrs/ — no diagnostic.
// Dangling: the const below fires.
// Filtered: HTTP-style identifiers shape-match the scanner regex
// but the HTTP namespace is not declared in ctxgrd.toml, so they are
// silently ignored by the namespace filter (ADR-001 § REF-005).

/// See PMR-001 for the post-mortem of the projection-lag incident.
pub const FALLBACK_PROPOSAL: &str = "ADR-8888";
