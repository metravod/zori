//! `zori ui` — the interactive console server.
//!
//! Turns zori's one-shot runner into a long-lived, collaborative console: the
//! server owns one or more [`TrackedRun`]s, exposes each as a live [`Emitter`]
//! (authoritative state + a broadcast of whole snapshots), persists every
//! snapshot to `state.json`, and offers HTTP endpoints to read a run, watch it
//! over SSE, steer a finished node (resume its harness session with a human
//! turn), and re-run nodes. The browser SPA is a pure function of the snapshot.
//!
//! Endpoints (JSON in/out unless noted):
//!   GET  /                                  the SPA (embedded HTML)
//!   GET  /api/runs                          list runs (id, name, status, …)
//!   POST /api/runs   {path, inputs?}        boot a flow file, returns run_id
//!   GET  /api/runs/:id                      the full RunState snapshot
//!   GET  /api/runs/:id/events               SSE stream of RunState snapshots
//!   GET  /api/runs/:id/transcript/:node     a node's turn list
//!   POST /api/runs/:id/steer  {node,text}   resume a Done node with a message
//!   POST /api/runs/:id/answer {node,text}   answer a Waiting node's parked question
//!   POST /api/runs/:id/rerun  {node}        re-run a node from scratch
//!   POST /api/runs/:id/rerun-stale          re-run every Stale node
//!
//! Flow-builder endpoints (the "Build" tab of the SPA — click a pipeline
//! together instead of hand-writing YAML; the server is the single authority
//! on what is a valid flow, the browser never re-implements validation):
//!   GET  /api/harnesses                     built-in harness catalog (id, model support, cmd)
//!   GET  /api/harnesses/:id/models          live model list for that harness (?refresh=1)
//!   GET  /api/flows                         list *.yml under the flows dir
//!   GET  /api/flows/open?path=…             read + parse a flow file → {yaml, flow}
//!   POST /api/flows/validate {flow}         check a spec → {ok, levels} or {ok:false, error}
//!   POST /api/flows/save   {flow, path?}    validate, write YAML → {path, yaml}
//!   POST /api/flows/boot   {flow, path?, inputs?}  save then boot → {run_id, path}
//!
//! The `state.json` on disk is the same shape as `GET /api/runs/:id`; a restart
//! can reload a finished run's transcript from it (in-progress children do not
//! survive a restart and come back `Stale`).

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Context as _;
use axum::extract::{Path as AxPath, Query, State};
use axum::http::header::{HOST, ORIGIN};
use axum::http::{Method, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::Stream;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::broadcast;

use crate::config::{Flow, Task};
use crate::runner::{plan, TrackedRun};
use crate::state::{Emitter, RunState, TaskStatus};

mod catalog;
mod chat;
mod control;
mod guard;
mod proposal;
mod router;
mod runs;
mod spec;
mod state;

use catalog::{get_fleet, harness_models, harnesses, list_flows, list_skills, open_flow, planner_catalog, put_fleet};
use chat::{planner_run_context, start_chat};
use control::{BootReq, answer, boot_flow, epoch_secs, rerun, rerun_stale, slug, steer};
use guard::{AllowedHosts, boundary_guard};
use proposal::{reject_proposal, run_proposal};
use runs::{boot, events, get_run, list_runs, load_existing_runs, spawn_persister, transcript};
use spec::{boot_spec, merge_ref_deps, missing_inputs, missing_inputs_error, missing_inputs_filtered, parse_spec, save_flow, spec_to_yaml, step_refs, upstream_closure, validate_flow, validate_spec, write_flow_file};
use state::{ApiError, AppState, ManagedRun, RunHandle, lookup, runs_guard};
#[cfg(test)]
use chat::{chat_flow_yaml, resolve_project_dir};
#[cfg(test)]
use control::{NodeAction, claim_node, rerunnable, validate_task_edit};
#[cfg(test)]
use guard::normalize_host;
#[cfg(test)]
use runs::{load_run, manifest_run_inputs};
#[cfg(test)]
use spec::{claim_flow_file, input_refs};
#[cfg(test)]
use state::lock_recovering;
#[cfg(test)]
use chat::ChatRequest;
#[cfg(test)]
use control::RerunReq;
pub use router::start;
pub use spec::{missing_input_warnings, validate_flow_spec};

#[cfg(test)]
mod ui_tests;
