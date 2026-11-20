# Zori

Run a coding task through several local CLI agents in parallel, then give their findings to another agent to reconcile. A YAML flow defines who does what and which outputs each task receives. Run it in the terminal or open the same flow in the browser to inspect a reply, answer a question, or rerun a branch.

## Install

```sh
brew install metravod/tap/zori
zori --version
```

Prebuilt binaries for macOS and Linux on arm64 and x86_64 are available on [GitHub Releases](https://github.com/metravod/zori/releases). To install manually, download the archive for your machine and `SHA256SUMS` from the same release. Check the archive with `shasum -a 256`, compare its digest with the matching entry, then extract `zori` into a directory on your PATH. To build from a checkout, run `cargo install --locked --path .`.

## Try a flow

Save this as `hello.yml`. Both steps run locally without a model or an account.

```yaml
name: hello
tasks:
  - id: brief
    harness: prompt
    in: "Review the parser for empty input."
  - id: reviewer
    harness: echo
    depends_on: [brief]
    in: "Check this requirement."
```

```sh
zori run hello.yml --dry-run
zori run hello.yml
cat zori-runs/hello-*/tasks/reviewer/r0/out.txt
```

On a first run the file contains the reviewer's instruction followed by a `# brief` section with the requirement. To watch the same flow in the browser, run `zori ui hello.yml` and open the printed local address. Run `zori ui` by itself to build a flow visually or reopen a previous run.

## Use your agents

Install and sign in to the CLI you want to use. Zori can run **Codex**, **Coddy** and **OpenCode**, each with its own provider configuration. To try one, change the `reviewer` harness in the example from `echo` to `codex`. Set `workspace: flow` on a task that needs the current checkout. By default it starts in its own empty working directory. Real agent calls use the account configured in that CLI.

The [parallel review example](examples/fanout-review.yml) sends one brief to Codex and Coddy, then passes both reports to a third task. From the repository root:

```sh
zori models codex
zori run examples/fanout-review.yml --input prompt="Review the uncommitted diff"
```

## Work with a running flow

- Set `max_parallel` to cap concurrent tasks. A failed task skips its dependents. Mark a branch `optional: true` to let a join proceed with the other reports.
- Open a task on the console canvas to inspect its transcript or continue its agent session. A changed reply marks downstream results stale so you can rerun them.
- The planner can propose a flow in chat. Zori validates the proposal and shows its graph for you to accept or reject before launch.
- Each run records prompts, outputs and attempt metadata under `zori-runs/`. Keep these files local when they contain private project material. The console binds to loopback and has no authentication. Run trusted flows and skills with the permissions you intend to give their commands.

See the [usage guide](docs/guide.md) for the console, watchdog, model discovery and skills. The [examples](examples/) include an offline fan-out and individual agent smoke tests.

## License

[MIT](LICENSE). Zori's level-based scheduler is adapted from zymi-core.
