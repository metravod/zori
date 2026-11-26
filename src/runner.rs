//! The driver: run a [`Flow`]'s DAG by executing each task as a child process
//! of its harness, level by level, and feeding each task's stdout to whoever
//! depends on it.
//!
//! This is the "own driver" half of D1: zymi's executor is not reusable, so we
//! write the ~300 lines that shell out to a CLI, capture its output, and wire
//! outputs into downstream prompts. Semantics we deliberately *fix* relative to
//! zymi (where docs and code disagreed — see the explorer report):
//!   * A failed task cascades: tasks that (transitively) depend on it are
//!     skipped. Independent branches still run. zymi only cascaded on
//!     `when:false`, not on failure; for a review fan-out the intuitive
//!     behaviour is to not run a joiner whose input died.
//!   * We keep zymi's level barrier (a level's slowest task gates the level)
//!     because it is simple and deterministic; parallelism inside a level is
//!     capped by `max_parallel`.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard, Semaphore};

use crate::artifacts::{self, AttemptMeta, AttemptRecord, ManifestStore, OutFile, RunDirs};
use crate::config::{Flow, PromptVia, Task, Workspace};
use crate::connectors::{self, Connector};
use crate::dag::{build_plan, ExecutionPlan};
use crate::state::{Emitter, FlowProposal, ProposalStatus, Role, RunEvent, SessionRef, TaskStatus};
use crate::watchdog::activity::{Activity, ObservedOutput, EXTENSION};
use crate::watchdog::{Attempt, Settings, Verdict};

mod capture;
mod env;
mod exec;
mod graph;
mod groups;
mod paths;
mod planning;
mod prompt;
mod questions;
mod run;
mod util;

pub use planning::{dry_run, plan};
use capture::{first_line, spawn_captured};
use env::{connector_for, env_keys, env_pairs, resolved_inputs};
use exec::{ExecMode, TrackedOutcome, attempt_verdict, mode_nonce, resumable, verdict_failure};
use groups::{GroupRegistration, install_death_handlers};
use paths::{RunArtifacts, dry_preview};
#[cfg(test)]
use paths::project_dir_of;
use planning::{HEARTBEAT_SECS, flow_default_timeout, render_argv, supervision_settings, timeout_for};
#[cfg(test)]
use planning::DEFAULT_TIMEOUT_SECS;
use prompt::{PromptCtx, missing_output, render_prompt, skill_block};
#[cfg(test)]
use prompt::interpolate;
use questions::{publish_questions, question_status, question_verdict};
use util::{new_coddy_session_id, now_secs};
pub use run::TrackedRun;
pub use graph::GraphPatch;
pub(crate) use paths::effective_cwd;
pub(crate) use prompt::canvas_prompt;

#[cfg(test)]
mod tracked_tests;
#[cfg(all(test, unix))]
mod tests;
