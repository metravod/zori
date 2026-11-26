//! zori — run one task across several CLI coding agents as a small DAG.
//!
//! Design summary (see `.omut/decisions.md`):
//! - `config`: the YAML surface, a `Flow` of `Task`s, each `{ in, harness,
//!   model, out, depends_on }`, per the owner's brief.
//! - `dag`: level-based plan ported from zymi-core (D1: scheduler only, not
//!   the event-sourced engine).
//! - `connectors`: coddy / codex / zcode built-ins plus user overrides; models
//!   selected per run where the CLI supports it (D1: zcode has no `--model`).
//! - `artifacts`: the single owner of the on-disk run layout — per-task
//!   attempt dirs, the manifest, checksums.
//! - `runner`: our own driver, spawning a child per task, level by level.
//! - `watchdog`: the supervisor — verdicts over finished attempts (nudge /
//!   retry / park-a-question) and the ask-protocol preamble.

pub mod catalog;
pub mod artifacts;
pub mod config;
pub mod connectors;
pub mod dag;
pub mod fleet;
pub mod models;
pub mod proposal;
pub mod runner;
pub mod skills;
pub mod state;
pub mod ui;
pub mod watchdog;

pub use config::{Flow, Task};
