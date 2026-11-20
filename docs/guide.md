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
  tasks/<task>/<attempt>/      one attempt: r0 = first run, t<secs> = steer
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
**⤓ Шаблон** writes saved pipelines to and the **Пайплайны** dropdown reads back.

There is **one canvas** — you build the flow and watch/steer a run on the same
blocks, no separate "Run" vs "Build" view:

- **The blocks are the view.** Each task is a draggable block (id, harness ·
  model, prompt preview). Drag to place it; edges, execution levels and the
  YAML pane redraw as you go. There is **one layout**: blocks sit in columns,
  left to right, one column per execution level — the order the runner
  executes them, which is the Airflow-like shape a DAG is drawn in. There is
  no edit-mode layout to fall back to; the plainer vertical card view went away
  with the separate Run screen.
- **The canvas is an infinite plane under the UI** (Airflow-style): it fills
  the strip edge to edge and the right panel floats *on top* of it, so opening
  or resizing the panel never squeezes the graph. Drag the empty background to
  pan the plane (there are no scrollbars and no edges — it is a transform, not
  a scroll region); the **wheel zooms around the cursor** (trackpad pinch
  works too), and the floating pill at the bottom-left offers **− / % / +**
  and **⤢ fit** — a flow loads fitted into the visible strip, and a selected
  block is panned into it. Node drags divide by zoom, so blocks stick to the
  cursor at any level.
- **Attach a run, get live status on the blocks.** Pick a run in the header
  (or hit **▶ Run**) and its flow loads onto this canvas: each
  block grows a status dot and a progress/result line (running · done · failed
  · stale · skipped), fed live over SSE. *Detach* (choose "editing — no run")
  to keep editing without the overlay.
- **Click a block → its chat, with the config folded above it.** The
  floating right panel is pure **chat**: the block's *starting prompt* is the first
  card
  (editable, `out` tucked under it), and the dialogue with its model — plus
  *Send & steer* / *Rerun* — flows underneath. Above the chat sits a
  collapsible **config strip**: название (id), harness, модель (live catalog
  with ↻), зависимости — purely checkboxes — and a per-task **directory**
  (`cwd`, overriding the flow cwd and the isolated `tasks/<id>/work` default).
  The toggle doubles as a one-line summary of the block; edits commit live —
  no OK button; Esc or a click on empty canvas drops the selection. The flow's
  **YAML** (levels + the file that will be written) is a small button in the
  panel's top-right corner. A fresh page opens on the default graph with the
  blank block's prompt card ready — authoring starts in the chat.
- **A minimal top bar.** The header is the pixel `zori` logo plus exactly the
  flow-level essentials: **название** (name), **директория запуска** (default
  `cwd`), the **＋ Задача ▾** split button and **Save** / **▶ Run**. The caret
  opens the two block kinds — **Задача** (a task block with an agent harness)
  and **Промпт** (a starter `prompt` block, no process) — and the button
  remembers the last kind used. Secondary flow ops (**открыть…**, **Новый**,
  **Inputs**) live in the YAML pop-up, not the header; *Rerun stale* appears
  next to the run selector only while a run is attached.
- **Steer / rerun from the chat.** *Send & steer* resumes the agent's *own*
  session (`coddy -session-id`, `codex exec resume <thread_id>`,
  `opencode run -s <session_id>`) with your message — context intact, proven on
  all three harnesses. The corrected output replaces the node's output and its
  downstream tasks are marked **stale**; *Rerun* the block (or *Rerun stale* in
  the header) to re-derive them from the fix.
- **A failed node is rerunnable — with the fix.** Rerun works on `failed` and
  `skipped` nodes too (not just done/stale), and it is a *fix-and-retry* loop:
  correct the block (e.g. a typo'd model name) and hit **⟳ Rerun** right in its
  chat — the re-run executes the corrected config, not the
  boot-time one; the corrected flow file is written back, so the fix survives a
  server restart. Ids and dependencies are frozen on a live run (change those
  by saving a new flow); everything else — model, prompt, out, cwd, timeout,
  args, env, optional — may be corrected. A rerun that dies at the driver
  level records `failed` honestly instead of hanging in `running`.
- **Agent answers render as Markdown.** The CLIs answer in markdown, so the
  dialogue renders headings, lists, task lists, tables, fenced code, quotes and
  links instead of printing the asterisks. Text is HTML-escaped before any
  markup is introduced, so a model (or a log line) cannot inject markup; only
  `http(s)`/`mailto` links become clickable. Your own turns and the **MD**
  toggle show the raw text exactly as the harness printed it (the choice is
  remembered). Renderer: `MD` in `web/index.html`, covered by
  `node web/md.test.mjs`. The layout's level computation is covered the same
  way by `node web/layout.test.mjs`.

Runs survive closing the tab and restart the server (`state.json` per run).
Steering works after a task finishes, not mid-turn (mid-turn needs per-harness
ACP/app-server — deferred). The console has no auth: bind locally.

### The watchdog: nudges, retries, and questions to you

The console supervises first
runs, reruns and their automatic follow-ups. A manual chat steer runs once. Classic `zori run` executes each task once.

- **An unfinished reply** (the `<response clipped` marker, empty assistant text,
  or an entire reply consisting of placeholders such as «Продолжаю…») is **nudged**: the *same* session is resumed with a message that
  orders the model to stop working and produce the final answer now — a naive
  "continue" would feed the very loop that killed the node.
  If the last nudge also yields an unfinished reply, the node fails.
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
  run's **Вопросы** menu. The tab title flags pending questions.
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
  retry: 1              # from-scratch retries for failed attempts (default 1)
  ask_timeout_secs: 0   # how long a parked question waits (0 = forever)
tasks:
  - id: fixes
    watchdog: {nudges: 3, retry: 0}   # per-task override, rest inherited
```

Absent block = the defaults shown (the supervisor is on; `ask: false` +
`nudges: 0` + `retry: 0` switches it off). The protocol preamble is mixed
only into the built-in agent harnesses (coddy/codex/opencode) — a custom
`harnesses:` override often takes a path or a one-shot script as its prompt,
where an appended section would corrupt the value; those still get
retries, but their questions stay in the output (they have no resumable
session to park). The console posts `{node, question_id, text}` to
`/api/runs/:id/answer`. Older clients can omit the ID for a single
unanswered question on a waiting node. Each ID accepts one answer.

### Start a prompt or a model chat

In the starting input of a detached canvas, type a message and press **Enter**.
**Shift+Enter** inserts a newline. Choose **Стартовый промпт** to make the
selected block a static input for downstream tasks, or **Чат с моделью** to
open a separate conversation. Harness groups start collapsed. Use the arrow
keys or Tab to focus a harness, Enter to expand it, then Enter on a model
to send the first message. Esc dismisses the chooser and retains the draft.

Selecting a model closes the picker and focuses the message input in the right
panel. Follow-ups, Markdown replies and operator questions use the same panel
as task conversations. The canvas stays interactive. Select a block to open
its inspector, or press Esc to return to the previous selection. To resume a
chat, choose its first message in the run selector. Reload restores the open
conversation.

### Build a flow without writing YAML

The console is a Visio-style canvas for the same YAML you would hand-write:
you start from one empty block and grow the graph, then **▶ Run** — on the
very same blocks a run later paints its live status (see the console section
above; building and running share one view).

- **Grow the graph by clicking.** Hover a block and hit **▸** to add a block that
  *depends on* this one (drawn to its right), or **∥** to add a *parallel*
  sibling (below, same stage, same dependencies). The header's **＋ Задача ▾**
  drops a free block anywhere — **Промпт** in the menu makes it a starter
  `prompt` block. Drag a block to place it — positions are stored on the task
  (`pos: {x, y}` in the YAML, ignored by the runner) so the layout survives a
  save/reopen. A flow with no stored positions lays itself out by execution
  level — from this page's own Kahn BFS, so it is horizontal the moment it
  loads, before the server has answered anything — until you drag something.
- **Configure each block in the folded config strip.** Click a block (a click,
  not a drag) and the right panel opens on it: the prompt card is the chat's
  first card, and the collapsible config above carries id, harness (a dropdown
  that greys `model` out for CLIs that don't honour it), per-block
  *Зависимости* checkboxes, the task **directory** (`cwd`), and an *optional*
  (best-effort branch) toggle. The **Model** field autocompletes from
  the harness's *live* catalog — pulled from the CLI itself (coddy's config,
  `codex debug models`, `opencode models`), never a hardcoded table — with its
  own default shown and a ↻ to refetch; you can always type any id instead.
  The **Inputs** button opens
  a window for the flow's `key=value` inputs.
- **Wire the DAG** two ways: tick *Зависимости* in the config strip, or just
  type `${steps.<id>.output}` in a prompt — references auto-wire the edge, and
  the edges, execution levels and the YAML (small button, top right) update as
  you go.
- **Watch it validate live.** The server runs the *same* check a real run uses
  (`plan` + `connectors::resolve`) on every edit — unknown harness, a model on a
  harness that can't set one, a dependency cycle — and shows the error inline
  before you can save. The YAML view shows exactly what will be written.
- **⤓ Шаблон** (Save) writes the file to `--flows-root` (`zori-flows` by
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
planner's inventories (`/api/catalog`, `/api/skills` — each documented under
its own heading below).

## Model selection differs per CLI — read this

| harness | per-run model | how |
|---|---|---|
| coddy | ✅ | `coddy -p <prompt> --model <id> --cwd <dir>` (models in `~/.coddy/config.yaml`) |
| codex | ✅ | `codex exec -m <id> -C <dir>` (default in `~/.codex/config.toml`) |
| opencode | ✅ | `opencode run --model <id> "<prompt>"` — ids are `provider/model[#variant]` (`opencode models` lists yours); unknown ids fail loudly |
| zcode | ❌ | bundled CLI has **no `--model` flag**; model is per-session (`/model`). Set it out-of-band, or zori errors rather than ignoring your `model:` |

The experimental ZCode connector targets its bundled macOS CLI. Version 0.1 supports Coddy, Codex and OpenCode for headless model runs. Use `ZORI_CODDY_BIN`, `ZORI_CODEX_BIN` or `ZORI_OPENCODE_BIN` to select another executable.

For the harnesses that do honour `model:`, zori can show you the *real* list
instead of a hand-kept table: `zori models` (or the console's Model field,
which autocompletes from it) pulls each catalog live — from coddy's
`config.yaml`, from `codex debug models`, from `opencode models`. A custom
harness becomes listable by giving its override a `models_cmd`: an argv whose
stdout is one model id per line.

### Planner catalog

`zori catalog` prints a JSON snapshot of built-in harnesses. The console exposes
the same inventory at `GET /api/catalog`. `zori catalog --flow path/to/flow.yml`
also resolves that flow's custom harnesses, listing commands and operator notes.

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
    in: "Собери флоу для ревью из трёх независимых агентов и сборки"
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
(**Чат с моделью**) attaches **both** automatically: start the chat and the model
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

So a planner turn hands the model two *kinds* of text, and the line between them is
the whole safety story. The two skills ride in as **context** (`zori-config` for
syntax, `zori-planner` for discipline) — yours to override with an explicit `skills`
list. But the model is also held to a **server-authored** response protocol plus a
live harness inventory, injected as the closing `# Output` block it is re-armed with
on every attempt; that text is *not* the model's to change, because a model left to
its own words in your repo will try to save a file and boot its own pipeline. The
protocol and the inventory are spelled out under *Propose → confirm → run*.

### Propose → confirm → run

The planner chat is fenced so a supermodel left to itself in your repo cannot
become a free agent. The dialogue runs in phases, and the model may **only
propose** — it never writes a flow file and never boots a pipeline; saving and
launching belong to you. The enforcement is server-side, not a request the model
might ignore:

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
5. **Run — you click.** A valid proposal parks with a **▶ Запустить** button.
   Only your click launches it: the server re-parses the YAML it stored (never
   trusting what came back from the browser), re-checks the proposal's **hash**
   (so a graph validated a moment ago can't be swapped under your click for a
   different one — a fresh block carries a fresh hash and is refused), applies the
   missing-inputs gate, writes the flow to `--flows-root`, and boots it as an
   ordinary run. **Отклонить** drops the proposal.

The hard response protocol — "emit one ` ```zori-flow ` block, save nothing, launch
nothing" — lives in the chat node's `out:` field, so the runner re-arms it as the
closing `# Output` block on *every* attempt (a steered continuation is held to it
as firmly as the first reply), and it is server-authored, never the model's own
words. The same field carries a cheap **harness inventory** of what's actually
installed on this machine (PATH-only, spawns nothing), so the model plans against
reality instead of hallucinating a model no CLI has ever seen. The proposal card
lives in the chat (rendered alongside the watchdog's question cards) and stays
honest across SSE refreshes; `POST /api/runs/:id/run-proposal` and
`POST /api/runs/:id/reject-proposal` are the two buttons' endpoints. `src/proposal.rs`
holds the parse/validate/repair logic; its tests, and the runner's gate tests
(valid parks · invalid earns one bounded repair turn · budget stops · no block is
a plain turn · a non-chat flow is never gated), cover the loop on the echo probe.
