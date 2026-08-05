//! contextguard kernel — library crate.
//!
//! Ship vehicle for the document linter binary `ctxgrd`. Modules are
//! introduced phase by phase per `docs/briefs/001-contextguard-kernel.md`.
//! Parsing lives in sources; rules only check.
//!
//! # Public API
//!
//! Today the only consumer of this crate is the in-tree `ctxgrd` binary
//! at `src/bin/ctxgrd.rs`. The boundary between `pub` and `pub(crate)`
//! is set by what the binary touches plus the types those entry points
//! transitively expose. A future LSP server (see
//! `../docs/briefs/002-contextguard-lsp.md`) is the second consumer
//! we expect; the surface is sized for it without committing to it.
//!
//! `pub` modules — consumer-facing entry points and their types:
//! [`config`], [`diagnostic`], [`document`], [`id`], [`introspect`],
//! [`reporter`], [`run`], [`scaffold`], [`source`], [`status`].
//!
//! `pub(crate)` modules — kernel internals; semver-volatile and not
//! part of any contract. Do not depend on these from outside the crate.

pub(crate) mod agent_guide;
pub(crate) mod ast;
pub(crate) mod builtin_rules;
pub use builtin_rules::builtin_param_names;
pub mod changelog;
pub mod config;
pub(crate) mod coverage;
pub(crate) mod dag;
pub mod diagnostic;
pub mod document;
pub(crate) mod envelope;
pub(crate) mod ext;
pub(crate) mod frontmatter;
pub mod id;
pub mod introspect;
pub mod list;
pub mod lsp;
pub mod pack;
pub mod path_claims;
pub mod pin;
pub(crate) mod reference;
pub mod reporter;
pub(crate) mod rules;
pub mod run;
pub mod scaffold;
pub mod serve;
pub mod source;
pub mod status;
pub(crate) mod subprocess;
