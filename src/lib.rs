//! # pbpp — protobuf preprocessor
//!
//! `.proto` → `.proto`: selection-driven trimming and normalization of
//! proto3 files. The pipeline is
//! `text → parse → CST → select → (transform)* → text`; nothing here
//! generates target-language code.
//!
//! ## Formatting a single file
//!
//! ```
//! let src = "syntax=\"proto3\";  message M{int32 a=1;}";
//! let cst = pbpp::parse(src)?;
//! assert_eq!(
//!     pbpp::format(&cst),
//!     "syntax = \"proto3\";\n\nmessage M {\n  int32 a = 1;\n}\n"
//! );
//! # Ok::<(), pbpp::Error>(())
//! ```
//!
//! ## Selecting and pruning a file set
//!
//! ```
//! let sources = vec![(
//!     "acme/v1/api.proto".to_string(),
//!     r#"syntax = "proto3";
//! package acme.v1;
//! message Keep { int32 id = 1; }
//! message Gone { string x = 1; }
//! "#
//!     .to_string(),
//! )];
//!
//! let pipeline = pbpp::Pipeline::new(
//!     sources.iter().map(|(p, s)| (p.clone(), s.as_str())).collect(),
//! )?;
//!
//! // Rules can come from the DSL or be built programmatically.
//! let rules = pbpp::rules::parse_rules("+ acme.v1.Keep\n")?;
//! let mut pruned = pipeline.prune(&rules)?;
//! pruned.format();
//! assert!(pruned.files[0].text.contains("message Keep"));
//! assert!(!pruned.files[0].text.contains("Gone"));
//! # Ok::<(), pbpp::Error>(())
//! ```
//!
//! ## Layers
//!
//! - [`lex`] / [`mod@parse`] / [`cst`]: lossless CST with byte spans and
//!   attached trivia;
//! - [`mod@format`]: the pipeline's only printer (idempotent,
//!   semantics-preserving, stable comment attachment);
//! - [`fileset`] / [`sema`]: multi-file symbol table, import visibility,
//!   reference resolution;
//! - [`rules`] / [`mod@select`]: the selection stage — marks on nodes, with
//!   provenance for "why was this kept?";
//! - [`mod@prune`]: materialization — deletion plus `reserved` insertions;
//! - [`digest`]: canonical semantic digest, the round-trip equality oracle;
//! - [`Pipeline`]: the orchestration façade over all of the above;
//! - [`mod@fs`]: the only layer with filesystem side effects — input
//!   discovery, path validation, manifest-tracked output sync, atomic
//!   writes — shared by `pbtrim` and build scripts.
//!
//! Errors are self-contained ([`Error`] implements [`core::error::Error`])
//! and render caret diagnostics via `Display`.

#![warn(missing_docs)]

pub mod cst;
pub mod digest;
pub mod error;
pub mod fileset;
pub mod format;
pub mod fs;
pub mod lex;
pub mod parse;
pub mod pipeline;
pub mod prune;
pub mod rules;
pub mod select;
pub mod sema;
pub mod span;
pub mod wkt;

pub use error::Error;
pub use fileset::FileSet;
pub use format::format;
pub use parse::parse;
pub use pipeline::Pipeline;
pub use prune::{PruneOutput, prune};
pub use rules::{Polarity, RuleSet};
pub use select::{Mark, Selected, select};
pub use sema::{Sema, SymId, SymKind, analyze};
