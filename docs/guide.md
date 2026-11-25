# Using Zori

## Run artifacts

Every run — classic `zori run` and `zori ui` alike — writes a tree under the
runs root (`zori-runs/`, override: `ZORI_RUNS_ROOT` for `run`, `--runs-root`
for `ui`):

```
zori-runs/<flow>-<unix-millis>/
  manifest.json                provenance index: inputs, levels, per-attempt
                               status/timings/checksums (atomic rewrites)
  state.json                   the ui server's live state (zori ui runs)
  sessions/                    coddy session storage for this run (ui runs)
  tasks/<task>/work/           the task's isolated working directory
  tasks/<task>/<attempt>/      one attempt: r0 = first run, later IDs = follow-up attempts
    prompt.txt  out.txt  err.txt  meta.json
```

* Each task runs **isolated by default**: its process cwd (and the harness's
  `--cwd`/`-C` flag, always as an absolute path) is `tasks/<task>/work`, so
  relative output paths land in a separate directory per task. Set `workspace: flow` on
  a task that must operate directly in the project; an explicit `cwd:` always
  wins over both.
* The fully rendered prompt is kept per attempt — the provenance a re-run or
  a future `fork` replays. `meta.json` records the exact argv, env var *names*,
  exit code, timings and checksums. `err.txt` is written always, not only on
  failure.
* Prompts can name paths: `${run.dir}`, `${project.dir}` (the flow `cwd`, or
  zori's own), `${task.dir}`, `${task.work}`, and `${steps.<id>.dir}` — the
  upstream task's subtree, so a join can read the files its branches wrote
  (`${steps.a.dir}/work/note.txt`). Children also get `ZORI_RUN_DIR`,
  `ZORI_TASK_DIR`, `ZORI_TASK_WORK`, `ZORI_PROJECT_DIR` in their env.
* Run ids are millisecond stamps, monotonic in-process — back-to-back boots
  never share a directory.

## Steer it live: `zori ui`

Open the DAG in a live console at
`http://127.0.0.1:8787`:

```bash
zori ui                      # reloads past runs, boot new ones from the page
zori ui examples/smoke-echo.yml --input prompt="…"
zori ui --bind 127.0.0.1:9000 --runs-root zori-runs --flows-root zori-flows
```

`zori ui` takes no flow and still works — it boots an empty canvas and reloads
every prior run under `--runs-root` (`zori-runs`). It binds local-only by
default and has **no auth**, so keep `--bind` on `127.0.0.1` unless you front it
yourself. `--flows-root` (`zori-flows`, created if missing) is the directory
**⤓ Template** writes saved pipelines to and the **Pipelines** dropdown reads back.

Each block on the canvas represents one task. Open a block for its starting prompt, transcript and configuration. The status indicator follows the attached run. Pan the canvas by dragging its background and use the zoom or fit controls to bring a branch into view.

After a turn finishes, send a follow-up in the task's chat to resume the same Coddy, Codex or OpenCode session. A changed output marks downstream tasks stale. Rerun one of those tasks, or rerun the stale part of the graph. If a task fails while its siblings are still running, edit its model or prompt in **Config**, then click **▶** on the block or **⟳ Rerun** in the inspector. The next level waits for the retry and uses its new output. Save a new flow to change task IDs or dependencies.

The console persists run state on disk and reloads it after a restart. Use the run selector to return to a previous conversation or inspect an earlier attempt.

### The watchdog: nudges, retries, and questions to you

The console supervises first
runs, reruns and their automatic follow-ups. A manual chat steer runs once. Classic `zori run` executes each task once.

- At an agent's deadline the console checks the last ten minutes for fresh
  stdout bytes (including a partial line) or a new transcript write from the
  current Coddy session. With evidence of activity, the same attempt gets ten
  more minutes and the node transcript records the reason. Each extension
  requires fresh evidence. Silence, stderr diagnostics and Zori heartbeats
  alone leave the deadline in force. Set `watchdog: {extend_on_progress: false}`
  on the flow or a task for a hard timeout. Batch `zori run` keeps its hard timeout.
- **An unfinished reply** (the `<response clipped` marker, empty assistant text,
  or an entire reply consisting of placeholders such as "Continuing…") is **nudged**: the *same* session is resumed with a message that
  orders the model to stop working and produce the final answer now — a naive
  "continue" would feed the very loop that killed the node.
  If the last nudge also yields an unfinished reply, the node fails.
- **Coddy's step limit** applies to one turn. Coddy can exit successfully
  with a progress report on stdout and an `agent.max_turns` notice on stderr.
  With a nudge available, Zori asks the same session once for the requested
  result using its existing history. The prompt asks Coddy to report completed
  work and checks, and to flag outstanding work with `ZORI_INCOMPLETE`.
  An unfinished or failed summary leaves the node failed and its transcript
  available for manual continuation. With no nudges left, the original
  partial report is retained as a failed attempt.
- **OpenCode can finish a report after a tool requested interactive input.**
  If its final step closes with `stop` and OpenCode subsequently reports the
  cancelled input request, the watchdog checks the final step's text. A usable
  reply goes downstream immediately. The attempt retains the original exit
  code, diagnostic and raw stream, and the node chat records the decision.
- **A failed attempt** (timeout, non-zero exit, a Codex/OpenCode stream error,
  an interrupted model turn, or a standalone diagnostic such as `connection lost` or
  `usage limit exceeded`) is **retried from
  scratch** — a fresh session, a fresh attempt dir (`r1`, `r2`, …).
  Deterministic driver errors (a broken prompt, a missing binary) are not
  retried. The manifest records the reason alongside the actual exit code.
  Failed Codex/OpenCode attempts also retain the captured event stream in `stdout.log`.
- **Blocking questions** use a standalone `ASK: Which branch?` line. The agent
  ends its turn and the node waits for the operator.
- **Inline questions** use `ASK_INLINE:`. The console displays them as soon as
  the CLI emits the assistant message. The agent can finish independent work
  during this turn. Either prefix accepts plain text or a single-line payload
  such as `ASK_INLINE: {"question":"Report format?","options":["CSV","JSON"]}`.
  Choose an option or enter a free-text answer in the node chat or the attached
  run's **Questions** menu. The tab title flags pending questions.
- At the end of the turn, the node waits for **all** outstanding answers.
  Replies can arrive in any order. Zori passes them together on the next turn
  of the same session. The normal level barrier holds during the wait.
  `ask_timeout_secs` starts at this boundary and defaults to unlimited.
  Reloading the page restores the attached run and its pending questions.
  Restarting the server leaves interrupted nodes stale, with their saved
  questions and answers visible in the chat history.
- **Unattended mode** (overnight): `watchdog: {unattended: true}` swaps the
  preamble for "nobody will answer — assume and proceed", and a question
  that slips through anyway is auto-answered with exactly that, capped at
  two canned answers per node.

Every automated action is recorded honestly: a `zori` **system turn** in the
transcript (never masquerading as you), one artifact dir per attempt
(`r0`, `r1`, `t<secs>` for every resume — nudge, answer or steer alike), and
the manifest indexes them all. The knob block, flow-wide with per-task
overrides on any field:

```yaml
watchdog:
  unattended: true      # "don't ask" preamble + canned auto-answers
  ask: true             # teach the ASK: protocol (default)
  nudges: 1             # auto-continues for unfinished replies (default 1)
  extend_on_progress: true # extend an active attempt at its deadline
  retry: 1              # from-scratch retries for failed attempts (default 1)
  ask_timeout_secs: 0   # how long a parked question waits (0 = forever)
tasks:
  - id: fixes
    watchdog: {nudges: 3, retry: 0}   # per-task override, rest inherited
```

Absent block = the defaults shown (the supervisor is on; `ask: false` +
`nudges: 0` + `retry: 0` switches it off). Coddy, Codex and OpenCode receive the question protocol by default.
Text CLIs can opt in with `transcript_chat: true` in their harness definition.
The console posts `{node, question_id, text}` to
`/api/runs/:id/answer`. Older clients can omit the ID for a single
unanswered question on a waiting node. Each ID accepts one answer.

### Start a prompt or a model chat

The right panel opens in **Chat with the supermodel**. Type a message and press
**Enter**, then choose the harness and model for the first turn. **Shift+Enter**
adds a line. The planner receives the canvas and, when a run is attached,
its node statuses and artifact directory. You can ask why a task failed or
what reached a downstream task in the same conversation used to plan the flow.

Select a block to see its own prompt, conversation and configuration. Click
the canvas background or press **Esc** to return to the planner. Its session
and draft survive the switch. For a run launched from a planner proposal,
opening the run also opens the originating conversation when available.
Earlier chats remain accessible in the run selector.

### Build a flow without writing YAML

The console is a Visio-style canvas for the same YAML you would hand-write:
you start from one empty block and grow the graph, then **▶ Run** — on the
very same blocks a run later paints its live status (see the console section
above; building and running share one view).

- **Grow the graph by clicking.** Hover a block and hit **▸** to add a block that
  *depends on* this one (drawn to its right), or **∥** to add a *parallel*
  sibling (below, same stage, same dependencies). The header's **＋ Task ▾**
  drops a free block anywhere — **Prompt** in the menu makes it a starter
  `prompt` block. Drag a block to place it — positions are stored on the task
  (`pos: {x, y}` in the YAML, ignored by the runner) so the layout survives a
  save/reopen. A flow with no stored positions lays itself out by execution
  level — from this page's own Kahn BFS, so it is horizontal the moment it
  loads, before the server has answered anything — until you drag something.
- **Configure each block in the folded config strip.** Click a block (a click,
  not a drag) and the right panel opens on it: the prompt card is the chat's
  first card, and the collapsible config above carries id, harness (a dropdown
  that greys `model` out for CLIs that don't honour it), per-block
  *Dependencies* checkboxes, the task **directory** (`cwd`), and an *optional*
  (best-effort branch) toggle. The **Model** field autocompletes from
  the harness's *live* catalog — pulled from the CLI itself (coddy's config,
  `codex debug models`, `opencode models`), never a hardcoded table — with its
  own default shown and a ↻ to refetch; you can always type any id instead.
  The **Inputs** button opens
  a window for the flow's `key=value` inputs.
- **Wire the DAG** two ways: tick *Dependencies* in the config strip, or just
  type `${steps.<id>.output}` in a prompt — references auto-wire the edge, and
  the edges, execution levels and the YAML (small button, top right) update as
  you go.
- **Watch it validate live.** The server runs the *same* check a real run uses
  (`plan` + `connectors::resolve`) on every edit — unknown harness, a model on a
  harness that can't set one, a dependency cycle — and shows the error inline
  before you can save. The YAML view shows exactly what will be written.
- **⤓ Template** (Save) writes the file to `--flows-root` (`zori-flows` by
  default); **open saved…** re-loads any of them (or a path) back onto the
  canvas. **▶ Run** (Save & Run) persists it, boots it as a run, and attaches
  that run to this same canvas — its blocks light up with live status, and their
  chats open the dialogue.

The server is the single authority on validity: the builder never saves a flow
the runner would reject. The endpoints are thin — the browser only renders the
spec and the server's verdict — grouped as: flow authoring (`/api/flows[/open|
/validate|/save|/boot]`, `/api/harnesses[/:id/models]`), runs and steering
(`/api/runs[/:id/{events,transcript/:node,steer,answer,rerun,rerun-stale,
run-proposal,reject-proposal}]`), planner chat (`POST /api/chats`), and the
planner's inventories (`/api/catalog`, `/api/skills`, `/api/fleet` — each documented under
its own heading below).

## Model selection differs per CLI — read this

| harness | per-run model | how |
|---|---|---|
| coddy | ✅ | `coddy -p <prompt> --model <id> --cwd <dir>` (models in `~/.coddy/config.yaml`) |
| codex | ✅ | `codex exec -m <id> -C <dir>` (default in `~/.codex/config.toml`) |
| opencode | ✅ | `opencode run --model <id> "<prompt>"` — ids are `provider/model[#variant]` (`opencode models` lists yours); unknown ids fail loudly |
| zcode | ❌ | bundled CLI has **no `--model` flag**; model is per-session (`/model`). Set it out-of-band, or zori errors rather than ignoring your `model:` |

Codex runs with `-s danger-full-access`. Its `workspace-write` sandbox keeps `.git` read-only and the network off, so a headless agent can neither fetch nor commit. A task narrows it with its own `args`, e.g. a reviewer with `args: ["-s", "read-only"]`; the task's `-s` replaces the built-in one.

The experimental ZCode connector targets its bundled macOS CLI. Version 0.1 supports Coddy, Codex and OpenCode for headless model runs. Use `ZORI_CODDY_BIN`, `ZORI_CODEX_BIN` or `ZORI_OPENCODE_BIN` to select another executable.

For the harnesses that do honour `model:`, zori can show you the *real* list
instead of a hand-kept table: `zori models` (or the console's Model field,
which autocompletes from it) pulls each catalog live — from coddy's
`config.yaml`, from `codex debug models`, from `opencode models`. A custom
harness becomes listable by giving its override a `models_cmd`: an argv whose
stdout is one model id per line.

### Adding a harness: the personal store

Besides the built-ins and a flow's own `harnesses:` block there is a personal
layer: **`~/.zori/harnesses.yml`** — machine-personal state next to the skills
store and the fleet (D11). Precedence is tightest last: built-in → personal →
the flow's block, so a personal entry on a built-in id refines it everywhere
(a flow can still narrow it per flow), and a new id becomes a first-class
harness at once — it appears in `zori harnesses`, `zori models`, `zori
catalog`, `/api/harnesses`, the console's harness dropdown, the Fleet dialog
and the planner brief without touching any flow. The plumbing ids `prompt`
and `echo` cannot be redefined here; a malformed file is ignored with a
warning, never fatal.

The schema is the same map of overrides a flow accepts:

```yaml
# ~/.zori/harnesses.yml
devin:
  description: "Devin CLI (SWE-2 models)."
  transcript_chat: true
  command: ["devin"]
  model_flag: "--model"
  prompt_via: arg
  prompt_arg: "-p"                # the flag before the prompt (-p "<prompt>")
  prompt_file_flag: "--prompt-file"
  prompt_file_args: ["-p"]        # keep print mode when the prompt goes via file
  # Headless runs cannot answer approval prompts: under accept-edits every
  # shell command is rejected. Bypass approves all tools; a read-only
  # reviewer narrows it with args: ["--permission-mode", "auto"].
  extra_args: ["--permission-mode", "dangerous"]
  replaceable_flags: ["--permission-mode"]   # a task's args may swap it
  models_cmd: ["sh", "-c", "devin models list --format json | jq -r '.families[].variants[].model_uid'"]
  model_notes:
    swe-2-high: "Stronger SWE-2 — hard fixes."
```

`command` is the only required field for a new id. With `transcript_chat: true`, the console shows the agent's
`ASK:` or `ASK_INLINE:` message in a question card. Answer there to continue.
A follow-up starts the same CLI in the node directory with the saved prompt,
previous replies and your message. That prompt goes through `prompt_file_flag`
when the harness has one, preceded by `prompt_file_args`; a CLI whose print
mode rides on `prompt_arg` (Devin's `-p`) must list it there, or the follow-up
opens an interactive session that never replies. Each turn uses the task's configured model
and tool permissions. A finished node also accepts messages through **Send & steer**.
Older completed runs can recover the conversation from a matching saved attempt.
`zori harnesses` lists every resolvable harness with its source
(`builtin` / `personal`), whether per-run models work and whether the binary
was found on PATH.

### Planner catalog

`zori catalog` prints a JSON snapshot of the harnesses resolvable on this
machine — built-ins plus the personal store (`~/.zori/harnesses.yml`). The
console exposes the same inventory at `GET /api/catalog`. `zori catalog --flow
path/to/flow.yml` also resolves that flow's custom harnesses (its block wins
per id), listing commands and operator notes.

Each entry describes the harness, per-task model selection, session resume,
executable presence, discovery status and the default model when known.
`models` contains exact IDs advertised by the harness. Codex supplies catalog
descriptions, context windows, input modalities and reasoning levels. Coddy
reads limits from its configured model entries. OpenCode uses its CLI for IDs
and enriches exact matches from its local registry cache, including context
and output limits, tool calling and modalities. A model listing alone does
not verify that the account can execute an individual model.

Codex account quota comes from `account/rateLimits/read` in the installed
Codex app server. `quota` contains the reported usage windows, percent used and
Unix reset times, with separate buckets when reported. `ordinary_usage_allowed`
is the provider's own signal. Unknown fields and `quota: null` mean no data.
The snapshot carries `observed_at` in Unix seconds. Account quotas can be
shared across models; consult the bucket association before routing a task.

Add short, task-specific guidance in a flow using the exact model ID:

```yaml
harnesses:
  opencode:
    description: "Runs coding tasks through our configured providers."
    model_notes:
      "my-provider/my-model": "Passed our short SQL-review cases."
```

Notes appear as `operator_note` alongside source metadata for discovered
models. Unknown IDs in notes do not create candidates. An unavailable model
list or quota source leaves the other harness entries usable.

### The model fleet

The **⚓ Fleet** button in the console header opens a multi-select over every
installed agent harness: each harness is a collapsed group, expanding it
lists its models from the live catalog, and **Enter toggles** a pick — the
dialog stays open until **Save** (or Esc, which discards unsaved toggles).
The saved roster lands in `~/.zori/fleet.json` — machine-personal state next
to the global skills store (D11), never in a project checkout.

What the fleet *does*: planner chats receive it as a **Model fleet** section
of their brief, so a proposed graph draws `model:` values from the
operator's own roster instead of the whole catalog. It does not hard-constrain
anything — the task editor's Model field still accepts any id, and a saved
pick whose harness has vanished stays visible in the picker (marked, removable)
rather than being silently dropped. The server validates on save: an entry
must name an installed agent harness that honours per-run models, so the
runner never meets a fleet pair it would refuse at boot.

## Skills

A *skill* is a markdown file of standing instructions you hand to a node so it
knows something the prompt alone doesn't — a house style, an API cheat-sheet,
how to emit valid zori YAML. At run time each named skill is read, its YAML
frontmatter stripped, and injected into that node's starting prompt as a
`# Skill: <name>` section — placed after the dependency-fed `# <id>` sections
and before the `# Output` spec, so the skill reads as context and the thing you
actually want back still lands last.

Attach skills by name in the YAML — on the flow (every task gets them) and/or
on a task (appended to the flow's, de-duplicated):

```yaml
name: config-bot
skills: [zori-config]            # every task carries this cheat-sheet
skills_dirs: [./team-skills]     # extra search paths, earlier wins (tilde ok)
tasks:
  - id: draft
    harness: coddy
    model: neuraldeep/kimi-k2.6
    in: "Build a review flow of three independent agents and a build"
    skills: [russian-style]      # this node also carries team-skills/russian-style.md
```

A name resolves across a four-layer store, highest priority first: the flow's
`skills_dirs` → the per-project `zori-skills/` → the global `~/.zori/skills` →
the **built-ins** compiled into the binary. So you can shadow a built-in by
dropping your own `zori-skills/zori-config.md` — same name wins. An unresolvable
name is a **plan-time error** (like an unknown `${…}` reference), never a silent
skip, so a typo fails the boot rather than quietly sending a node off without
its context.

`zori skills` lists everything reachable right now — the built-ins plus every
`<name>.md` in the stores (pass `--flow path.yml` to include that flow's
`skills_dirs` too). The console exposes the same inventory at `GET /api/skills`.

**The built-in `zori-config` skill** is a cheat-sheet of the whole flow schema —
every `Flow`/`Task` field, the `${steps…}`/`${inputs…}` references, the harness
capability table, a worked example — written so a model can turn a plain-language
brief into *valid* zori YAML. Alongside it, **the built-in `zori-planner` skill**
carries the *discipline* rather than the syntax: which phases to walk (discuss →
agree → emit the graph), when to fan out versus keep a node single, how to pick a
harness, and the two hard boundaries the planner chat leans on — *never save a
file, never launch a pipeline* (see below). The console's planner chat
(**Chat with the model**) attaches **both** automatically: start the chat and the model
already speaks flow *and* keeps to the protocol, so "make me a three-reviewer flow"
comes back as a config the builder can render. Send an explicit `skills` list —
including an empty one — to override that default; the chat's skills toggle in the
dialog controls it, and it round-trips into the node so reopening the chat keeps them.

Both built-ins ship in the repo's own top-level **`skills/`** directory
(`skills/zori-config.md`, `skills/zori-planner.md`) and are baked into the binary
with `include_str!` (`src/skills.rs`), not read from disk at run time — which is why
a fresh clone needs no install step to speak flow. To add one, drop a markdown file
in `skills/` and wire it into `src/skills.rs`: an `include_str!` const, a resolver
arm, and its name in the `BUILTINS` list.

### Run a planner proposal

The planner receives the selected skills, the installed harness inventory and a server-supplied instruction to return a proposal in chat. Zori's proposal endpoint requires an explicit run request before saving and launching the graph. The underlying agent CLI still runs with its configured tool and filesystem permissions.

1. **Discuss.** The model thinks out loud in plain text — steps, who does each,
   what feeds what, where things run in parallel. No YAML yet.
2. **Propose.** Once you've agreed on the shape, the model emits the graph as a
   single block tagged ` ```zori-flow ` (triple backtick + `zori-flow`, YAML
   inside — a plain ` ```yaml ` block is left as prose). The last such block wins.
3. **Validate — on the server.** zori parses the block and runs it through the
   *same* check a real boot uses (`plan` + `connectors::resolve`): unknown
   harness, a model on a harness that can't take one, a dependency cycle. The
   verdict is stamped onto the chat node as a **proposal** and the card appears in
   the chat — not in the transcript as text you'd have to copy.
4. **Auto-repair, bounded.** If the block is invalid, the server doesn't hand you
   a broken card and shrug: it sends a **system turn** back into the *same* agent
   session naming the failures and demanding the full corrected graph, capped at
   two repair rounds so a looping model can't spin the run.
5. **Run — you click.** A valid proposal parks with a **▶ Run** button.
   Only your click launches it: the server re-parses the YAML it stored (never
   trusting what came back from the browser), re-checks the proposal's **hash**
   (so a graph validated a moment ago can't be swapped under your click for a
   different one — a fresh block carries a fresh hash and is refused), applies the
   missing-inputs gate, writes the flow to `--flows-root`, and boots it as an
   ordinary run. **Reject** drops the proposal.
