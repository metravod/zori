//! The flow-proposal bridge — the seam between *asking* and *executing*.
//!
//! The interactive console lets an operator start a dialogue with a strong
//! model ("the supermodel") and ask it for a pipeline. The model must
//! **propose** a graph in plain language, and only after the operator agrees
//! emit it as YAML. zori — never the model — is the one that validates that
//! graph and boots it. The model has no authority to launch anything; the
//! operator's Run click is the sole execution trigger. That is the whole point
//! of this module: it turns «the model emitted some YAML» into a server-checked
//! [`crate::state::FlowProposal`] the console can gate on.
//!
//! Three concerns, deliberately kept apart:
//!   * [`extract`] — a pure scan of an agent reply for a fenced
//!     ` ```zori-flow ` block. No parsing: a reply with no block is a normal
//!     conversational turn (phase 1 of the protocol), not a failure.
//!   * [`validate`] — the SAME checks the flow builder applies to a hand-built
//!     spec (`/api/flows/validate`), so a graph that would boot from Build also
//!     passes here, and one that wouldn't is refused with the same wording.
//!   * [`harness_brief`] — a cheap, non-spawning inventory of the harnesses
//!     installed on this machine, injected into a chat node's prompt so the
//!     model plans against reality instead of a fabricated catalog (the bug
//!     this whole path exists to stop: a `model:` the CLI has never heard of).
//!
//! [`validate`] lives here rather than in `ui.rs` because BOTH the builder and
//! the proposal path must apply it verbatim; two validators would drift, and
//! the drift would show up as «Build accepted it but the chat proposal didn't».

use serde::Serialize;

use crate::config::Flow;

/// The fence language tag that marks a proposal. A model writes
/// ` ```zori-flow ` and the YAML inside it; anything else — plain ` ```yaml `,
/// an inline snippet — is left alone, so an example in prose is never mistaken
/// for a graph to run.
pub const FENCE_TAG: &str = "zori-flow";

/// How many auto-continuation turns zori spends asking the model to repair an
/// invalid block before it stops and lets the operator take over. Bounded so a
/// model stuck in a loop cannot spin the run the way the watchdog's nudges are
/// bounded; the operator always has the last word.
pub const MAX_FIX_ROUNDS: u32 = 2;

/// The continuation message zori sends back into the same agent session when a
/// ` ```zori-flow ` block it just received failed validation: name the
/// failures, demand the FULL corrected graph re-emitted in one fence, and
/// restate the two rules the protocol hinges on (never save, never run). Pure
/// so the wording is unit-testable and the runner stays thin.
pub fn fix_instruction(errors: &[String]) -> String {
    let mut s = String::from(
        "Твой блок ```zori-flow не прошёл проверку zori. Исправь и выдай ПОЛНЫЙ \
         исправленный граф заново, одним блоком ```zori-flow (не фрагмент и не \
         комментарий — целиком). Ничего не сохраняй и не запускай: файл кладёт и \
         запуск делает оператор. Ошибки проверки:\n",
    );
    for e in errors {
        s.push_str("- ");
        s.push_str(e);
        s.push('\n');
    }
    s
}

/// One validation verdict over a proposed YAML graph. Everything the console
/// needs to render a card and the launch gate needs to be honest: a proposal is
/// runnable only when [`Proposal::valid`] AND its [`Proposal::hash`] still
/// matches what the operator clicked.
#[derive(Debug, Clone, Serialize)]
pub struct Proposal {
    /// The exact YAML inside the fence (trimmed). What the card shows.
    pub raw: String,
    /// [`content_hash`] of `raw` — the swap guard the launch endpoint re-checks.
    pub hash: String,
    /// Does zori accept this graph (the builder's checks pass)?
    pub valid: bool,
    /// Blocking failures (empty when `valid`), one per message.
    pub errors: Vec<String>,
    /// Non-fatal notes — e.g. `${inputs.k}` nothing supplies yet: legitimate
    /// if the operator means to pass `--input` at run time, fatal at boot.
    pub warnings: Vec<String>,
    /// The DAG level plan when `valid`, so the console can sketch the graph
    /// without re-parsing the YAML.
    pub levels: Vec<Vec<String>>,
}

/// Review M12: the fenced block is model output — untrusted and unbounded. A
/// megabyte "proposal" used to be copied out of the reply and handed to the
/// YAML parser (and through the full builder validation) with no ceiling. No
/// flow anyone means to run is this big; past this size the block is reported
/// as oversized so the repair loop asks for a real graph.
pub const MAX_PROPOSAL_BYTES: usize = 256 * 1024;

/// What the fence scan found. Kept private; [`extract`] and [`from_reply`]
/// project it.
enum Found {
    /// No tagged block at all — plain conversation.
    Nothing,
    /// A tagged block within [`MAX_PROPOSAL_BYTES`].
    Block(String),
    /// A tagged block over the cap, with its size. Not a parse (the bytes are
    /// never copied or handed to YAML), but not silence either.
    Oversized(usize),
}

/// The block the model marked as its proposed graph, or `None` when the reply
/// is plain conversation — or carries only a block past [`MAX_PROPOSAL_BYTES`]
/// (use [`from_reply`] to tell those apart: it reports the oversized case as
/// an invalid proposal, so the operator sees why no card appeared).
/// The **last** tagged block wins: a model that shows a draft then a corrected
/// version has decided the later one is the proposal.
pub fn extract(reply: &str) -> Option<String> {
    match extract_inner(reply) {
        Found::Block(yaml) => Some(yaml),
        _ => None,
    }
}

fn extract_inner(reply: &str) -> Found {
    let mut best: Found = Found::Nothing;
    // (fence char, its opening run length, its language tag)
    let mut fence: Option<(char, usize, String)> = None;
    let mut body: Vec<String> = Vec::new();
    let mut bytes: usize = 0;
    for line in reply.lines() {
        let t = line.trim();
        match &fence {
            None => {
                if let Some((marker, run, lang)) = fence_open(t) {
                    fence = Some((marker, run, lang));
                    body.clear();
                    bytes = 0;
                }
            }
            Some((marker, open, lang)) => {
                // The fence closes only on a bare marker of the SAME character
                // (so ``` doesn't close ~~~) and at least as long as the
                // opening run — a 4-marker open used to die on 3 markers
                // inside, before the graph was whole. A tagged close like
                // ```yaml is data, not the close.
                if is_close(t, *marker, *open) {
                    if lang.eq_ignore_ascii_case(FENCE_TAG) {
                        best = if bytes > MAX_PROPOSAL_BYTES {
                            Found::Oversized(bytes)
                        } else {
                            Found::Block(body.join("\n"))
                        };
                    }
                    fence = None;
                    body.clear();
                    bytes = 0;
                } else {
                    // M12: count every line (so the verdict names the real
                    // size) but copy only while the block can still be a
                    // proposal — the megabyte is never copied out of the reply
                    // into its own String, let alone handed to the parser.
                    bytes += line.len() + 1;
                    if bytes <= MAX_PROPOSAL_BYTES {
                        body.push(line.to_string());
                    }
                }
            }
        }
    }
    // An unterminated ```zori-flow (the reply was cut off mid-graph) is not a
    // complete proposal — leave it for the watchdog's nudge rather than
    // validating a half-written graph.
    best
}

/// Does this trimmed line OPEN a fence, and with what language? Returns
/// `(fence char, opening run length, lowercased language)`; language `""` for
/// a bare fence. The run length travels to [`is_close`]: a fence opened with
/// four markers closes only under four or more — the old parser remembered
/// the character but not the length, so a 4-marker open died on a 3-marker
/// line that was graph data (review, minor).
fn fence_open(t: &str) -> Option<(char, usize, String)> {
    let marker = t.chars().next()?;
    if marker != '`' && marker != '~' {
        return None;
    }
    let run = t.chars().take_while(|&c| c == marker).count();
    // A fence is three-or-more of the marker; the rest is the language tag.
    // The marker is ASCII, so the run counts bytes as well.
    if run < 3 {
        return None;
    }
    let lang = t[run..].trim();
    // The closing line has no language; treat a line that is only more markers
    // as a close, never an open.
    if lang.chars().all(|c| c == marker) {
        return None;
    }
    // The tag is up to the first space.
    let lang = lang.split_whitespace().next().unwrap_or("").to_lowercase();
    Some((marker, run, lang))
}

/// Is this trimmed line a close of a fence opened with `marker` and a run of
/// `open` markers (and no language)? Only a run at least as long as the
/// opening one closes it; `open` is already >= 3 by construction.
fn is_close(t: &str, marker: char, open: usize) -> bool {
    if t.is_empty() {
        return false;
    }
    // The line must be the marker and nothing else.
    t.chars().all(|c| c == marker) && t.chars().count() >= open
}

/// Validate a YAML graph exactly as the flow builder would: parse as a [`Flow`],
/// then apply the builder's checks (task ids, non-empty prompts, resolvable
/// harnesses, models the harness accepts, an acyclic `${steps…}` graph), plus a
/// warning list for `${inputs.k}` nothing supplies. Every failure is collected
/// into human-readable strings; this never panics and never touches the disk.
///
/// An unparseable YAML is a soft failure (`valid: false`, the parse error as
/// the single `errors` entry), because a malformed proposal is the model's to
/// fix, not a server error.
pub fn validate(yaml: &str) -> Proposal {
    let raw = yaml.trim().to_string();
    let hash = content_hash(&raw);
    let flow: Flow = match serde_yaml::from_str(yaml) {
        Ok(f) => f,
        Err(e) => {
            return Proposal {
                raw,
                hash,
                valid: false,
                errors: vec![format!("flow YAML не разобран: {e}")],
                warnings: Vec::new(),
                levels: Vec::new(),
            };
        }
    };
    match crate::ui::validate_flow_spec(&flow) {
        Ok(plan) => Proposal {
            raw,
            hash,
            valid: true,
            errors: Vec::new(),
            warnings: crate::ui::missing_input_warnings(&flow),
            levels: plan.levels,
        },
        Err(errors) => Proposal {
            raw,
            hash,
            valid: false,
            errors,
            warnings: Vec::new(),
            levels: Vec::new(),
        },
    }
}

/// The whole pipeline: extract the proposal block and validate it, or `None`
/// when the reply carries no ` ```zori-flow ` block at all. An oversized block
/// is *not* silence: it comes back as an invalid proposal (with no `raw`, so
/// the megabyte is never stored) and the chat's repair loop asks the model for
/// a real graph — otherwise a truncated reply would look like plain
/// conversation and the turn would end with no card and no explanation.
pub fn from_reply(reply: &str) -> Option<Proposal> {
    match extract_inner(reply) {
        Found::Nothing => None,
        Found::Block(yaml) => Some(validate(&yaml)),
        Found::Oversized(bytes) => Some(Proposal {
            raw: String::new(),
            hash: content_hash(""),
            valid: false,
            errors: vec![format!(
                "предложение слишком большое: {bytes} байт (максимум {MAX_PROPOSAL_BYTES}) — предложи более компактный граф"
            )],
            warnings: Vec::new(),
            levels: Vec::new(),
        }),
    }
}

/// Review M12: the same pipeline off the async worker. A proposal is model
/// output, and its YAML parse plus the full builder validation are CPU work —
/// both the model-reply gate and the Run-button re-parse used to run them on
/// the runtime's worker threads, where one megabyte of adversarial YAML
/// stalls every other run's event handling. `spawn_blocking` moves it away.
/// A panicked validation worker is logged and treated as "no block" (the
/// operator can steer and ask again) rather than crashing the supervisor.
pub async fn from_reply_async(reply: String) -> Option<Proposal> {
    match tokio::task::spawn_blocking(move || from_reply(&reply)).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("  [warning] proposal validation worker failed: {e}");
            None
        }
    }
}

/// A short, deterministic FNV-1a hex digest of the proposal text. Not a
/// security primitive — just a compact, stable fingerprint the Run button
/// round-trips so a proposal validated a moment ago cannot be swapped under the
/// operator's click (a fresh block would carry a different hash and be refused).
pub fn content_hash(s: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// A cheap inventory of the harnesses this machine can actually run, rendered
/// as a markdown section for a chat node's starting prompt. Deliberately does
/// NOT spawn a CLI to list models (that is the model's own job via `zori
/// models`, and the slow, flaky part) — it reports only what resolving a
/// connector and checking PATH tell us for free: whether the CLI exists,
/// whether it takes a `model:` per run, whether zori can steer it. That single
/// fact («zcode is not installed, opencode cannot take a model») is the whole
/// class of dead-node bugs the last chat run hit.
pub fn harness_brief() -> Option<String> {
    use crate::connectors;
    let mut rows: Vec<String> = Vec::new();
    for id in connectors::builtin_ids() {
        // Skip the non-agent harnesses from the planner's view: they are data
        // plumbing (`prompt`) and smoke tests (`echo`), not agents to plan with.
        if matches!(*id, "echo" | "prompt") {
            continue;
        }
        let Ok(conn) = connectors::resolve(id, None) else {
            continue;
        };
        if !conn.is_agent {
            continue;
        }
        let installed = conn
            .command
            .first()
            .map(|cmd| crate::catalog::executable_found(cmd, conn.env.get("PATH")))
            .unwrap_or(false);
        if !installed {
            rows.push(format!(
                "- **{id}** — НЕ установлен на этой машине (бинарь «{}» не найден). Не используй этот харнесс.",
                conn.command.first().map(String::as_str).unwrap_or(id)
            ));
            continue;
        }
        let model = if conn.supports_model {
            "модель за прогон: да"
        } else {
            "модель за прогон: НЕТ (не ставь `model:`)"
        };
        let steer = if connectors::steerable(&conn) {
            "можно доводить ходы"
        } else {
            "без доведения"
        };
        rows.push(format!("- **{id}** — установлен; {model}; {steer}."));
    }
    if rows.is_empty() {
        return None;
    }
    Some(format!(
        "# Харнессы на этой машине\n\n\
         Реальное состояние CLI-агентов (сверь с ним `harness:`/`model:`, выдуманный харнесс или модель уронят узел):\n{}",
        rows.join("\n")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_grabs_the_last_zori_flow_block() {
        let reply = "вот черновик:\n```yaml\nname: wrong\n```\n\
                     а вот готовый:\n```zori-flow\nname: ok\ntasks:\n  - id: a\n    harness: echo\n    in: hi\n```\n";
        assert_eq!(
            extract(reply).as_deref(),
            Some("name: ok\ntasks:\n  - id: a\n    harness: echo\n    in: hi")
        );
    }

    #[test]
    fn extract_is_none_without_a_tagged_block() {
        assert!(extract("просто текст, без блока").is_none());
        assert!(extract("```yaml\nname: x\n```\n").is_none(), "plain yaml is not a proposal");
    }

    #[test]
    fn extract_refuses_an_unterminated_fence() {
        // A reply cut off mid-graph is not a complete proposal.
        assert!(extract("```zori-flow\nname: x\ntasks:\n").is_none());
    }

    #[test]
    fn extract_handles_tilde_and_uppercase_fence() {
        let reply = "~~~ZORI-FLOW\nname: ok\ntasks:\n  - id: a\n    harness: echo\n    in: hi\n~~~\n";
        assert!(extract(reply).is_some());
    }

    #[test]
    fn nested_backticks_inside_do_not_close_early() {
        // A YAML value that itself contains a single/double backtick must not
        // end the fence; only a bare ``` line does.
        let reply = "```zori-flow\nname: has-`tick`\ntasks:\n  - id: a\n    harness: echo\n    in: hi\n```\n";
        assert_eq!(extract(reply).unwrap().lines().count(), 5);
    }

    #[test]
    fn validate_accepts_a_minimal_flow() {
        let p = validate("name: ok\ntasks:\n  - id: a\n    harness: echo\n    in: hi\n");
        assert!(p.valid, "{:?}", p.errors);
        assert_eq!(p.levels, vec![vec!["a".to_string()]]);
    }

    #[test]
    fn validate_rejects_unknown_harness_and_reports_it() {
        let p = validate("name: ok\ntasks:\n  - id: a\n    harness: nosuch\n    in: hi\n");
        assert!(!p.valid);
        assert!(p.errors.iter().any(|e| e.contains("nosuch")), "{:?}", p.errors);
    }

    #[test]
    fn validate_rejects_a_cycle() {
        let p = validate(
            "name: ok\ntasks:\n\
             - id: a\n  harness: echo\n  in: \"${steps.b.output}\"\n\
             - id: b\n  harness: echo\n  in: \"${steps.a.output}\"\n",
        );
        assert!(!p.valid, "a cycle must not validate");
    }

    #[test]
    fn validate_warns_on_unsupplied_inputs() {
        let p = validate("name: ok\ntasks:\n  - id: a\n    harness: echo\n    in: \"${inputs.need}\"\n");
        assert!(p.valid, "unsupplied input is a warning, not an error");
        assert!(p.warnings.iter().any(|w| w.contains("need")), "{:?}", p.warnings);
    }

    #[test]
    fn validate_reports_malformed_yaml_without_panicking() {
        let p = validate("name: ok\n  : : [ [ bad");
        assert!(!p.valid);
        assert!(!p.errors.is_empty());
    }

    #[test]
    fn hash_is_stable_and_sensitive() {
        assert_eq!(content_hash("abc"), content_hash("abc"));
        assert_ne!(content_hash("abc"), content_hash("abd"));
    }

    #[test]
    fn from_reply_none_without_block_some_with_block() {
        assert!(from_reply("hello").is_none());
        let p = from_reply("```zori-flow\nname: ok\ntasks:\n  - id: a\n    harness: echo\n    in: hi\n```\n").unwrap();
        assert!(p.valid);
    }

    // Review M12: an oversized block is neither silence nor a parse. extract
    // refuses it, from_reply surfaces it as an invalid proposal so the repair
    // loop (and the operator) learn why no card appeared — and `raw` stays
    // empty so the megabyte is never stored on the node.
    #[test]
    fn from_reply_reports_an_oversized_block_instead_of_silence() {
        let big = "x".repeat(MAX_PROPOSAL_BYTES + 1024);
        let reply = format!("```zori-flow\nname: ok\nnote: {big}\n```\n");
        assert!(extract(&reply).is_none(), "extract must refuse the giant");
        let p = from_reply(&reply).expect("oversized is reported, not dropped");
        assert!(!p.valid);
        assert!(p.raw.is_empty(), "the megabyte must not be kept as raw");
        assert!(
            p.errors[0].contains("слишком большое") && p.errors[0].contains("максимум"),
            "the error must name the size and the cap: {:?}",
            p.errors[0]
        );
    }

    // Review M12 (minor): a fence opened with four markers used to die on the
    // first three-marker line inside, because the parser remembered the fence
    // character but not its opening run length. The three-marker line is data.
    #[test]
    fn extract_keeps_a_three_marker_line_inside_a_four_marker_fence() {
        let reply = "````zori-flow\nname: ok\n# ``` not a close\ntasks:\n  - id: a\n    harness: echo\n    in: hi\n````\n";
        let yaml = extract(reply).expect("the fence survives its own data");
        assert!(yaml.contains("# ``` not a close"));
        let p = from_reply(reply).unwrap();
        assert!(p.valid, "errors: {:?}", p.errors);
    }

    // The async entrypoint the proposal gate uses: same verdict, off-worker.
    #[tokio::test]
    async fn from_reply_async_matches_the_sync_verdict() {
        let reply = "```zori-flow\nname: ok\ntasks:\n  - id: a\n    harness: echo\n    in: hi\n```\n";
        let p = from_reply_async(reply.to_string()).await.unwrap();
        assert!(p.valid);
        assert_eq!(p.hash, content_hash(&p.raw));
        assert!(from_reply_async("plain talk".to_string()).await.is_none());
    }
}
