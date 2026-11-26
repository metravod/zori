#![cfg(unix)]

use std::path::PathBuf;
use std::process::{Output, Stdio};
use std::time::Duration;

use zori::artifacts::{self, Manifest};

struct RunFixture {
    root: PathBuf,
}

impl RunFixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(artifacts::new_run_id("zori-cli-test"));
        std::fs::create_dir(&root).unwrap();
        Self { root }
    }

    async fn run(&self, yaml: &str, args: &[&str]) -> Output {
        let flow = self.root.join("flow.yml");
        std::fs::write(&flow, yaml).unwrap();
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_zori"));
        command
            .current_dir(&self.root)
            .env(artifacts::RUNS_ROOT_ENV, self.root.join("runs"))
            .args(["run", flow.to_str().unwrap()])
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().unwrap();
        // Keep a redirected stdin open throughout the run. A blocking prompt
        // would hang even though no answer will ever arrive.
        let _stdin = child.stdin.take().unwrap();
        tokio::time::timeout(Duration::from_secs(10), child.wait_with_output())
            .await
            .expect("CLI should finish without reading piped stdin")
            .unwrap()
    }

    fn run_dir(&self) -> PathBuf {
        let entries = std::fs::read_dir(self.root.join("runs")).unwrap();
        let dirs: Vec<_> = entries.map(|entry| entry.unwrap().path()).collect();
        assert_eq!(dirs.len(), 1);
        dirs.into_iter().next().unwrap()
    }
}

impl Drop for RunFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[tokio::test]
async fn run_retries_a_required_failure_and_exits_one() {
    let fixture = RunFixture::new();
    let output = fixture
        .run(
            r#"
name: required-failure
inputs: {prompt: default}
harnesses:
  failing: {command: ["sh", "-c", "cat && echo diagnostic >&2 && exit 7"], prompt_via: stdin}
tasks:
  - {id: required, harness: failing, in: "${inputs.prompt}"}
  - {id: dependent, harness: echo, in: waiting, depends_on: [required]}
"#,
            &["--input", "prompt=overridden"],
        )
        .await;

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert_eq!(
        output.status.code(),
        Some(1),
        "{stdout}\n{:?}",
        output.stderr
    );
    for line in [
        "[required] started",
        "[required] [watchdog] attempt failed",
        "[required] failed exited with code 7",
        "[dependent] skipped",
        "0 done, 1 failed, 1 skipped",
    ] {
        assert!(stdout.contains(line), "missing {line:?} in {stdout}");
    }
    let run_dir = fixture.run_dir();
    let manifest: Manifest =
        serde_json::from_slice(&std::fs::read(run_dir.join("manifest.json")).unwrap()).unwrap();
    let attempts = &manifest.tasks["required"].attempts;
    assert_eq!(attempts.len(), 2, "the tracked watchdog retries by default");
    for nonce in ["r0", "r1"] {
        assert_eq!(attempts[nonce].exit_code, Some(7));
        let dir = run_dir.join("tasks/required").join(nonce);
        assert_eq!(
            std::fs::read_to_string(dir.join("out.txt")).unwrap(),
            "overridden\n"
        );
        assert!(std::fs::read_to_string(dir.join("err.txt"))
            .unwrap()
            .contains("diagnostic"));
    }
    assert!(manifest.tasks["dependent"].attempts.contains_key("skipped"));
}

#[tokio::test]
async fn run_reports_optional_failure_as_degraded() {
    let fixture = RunFixture::new();
    let output = fixture
        .run(
            r#"
name: optional-failure
harnesses:
  failing: {command: ["sh", "-c", "exit 7"], prompt_via: stdin}
tasks:
  - {id: optional, harness: failing, in: test, optional: true}
  - {id: dependent, harness: echo, in: "${steps.optional.output}", depends_on: [optional]}
"#,
            &[],
        )
        .await;

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(output.status.success(), "{stdout}\n{:?}", output.stderr);
    assert!(stdout.contains("[dependent] done"), "{stdout}");
    assert!(
        stdout.contains("1 done, 0 failed, 0 skipped, 1 degraded (best-effort)"),
        "{stdout}"
    );
    let out =
        std::fs::read_to_string(fixture.run_dir().join("tasks/dependent/r0/out.txt")).unwrap();
    assert!(out.contains("did not complete"), "{out}");
}

#[tokio::test]
async fn redirected_stdin_uses_unattended_answers() {
    let fixture = RunFixture::new();
    let yaml = r#"
name: question
watchdog: {unattended: false}
tasks:
  - {id: question, harness: echo, in: "ASK: which format?", watchdog: {unattended: false}}
"#;
    let preview = fixture.run(yaml, &["--dry-run"]).await;
    assert!(preview.status.success());
    assert!(!fixture.root.join("runs").exists());

    let output = fixture.run(yaml, &[]).await;
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(output.status.success(), "{stdout}\n{:?}", output.stderr);
    assert!(stdout.contains("auto-answering"), "{stdout}");
    assert!(stdout.contains("1 done, 0 failed, 0 skipped"), "{stdout}");
    assert!(output.stderr.is_empty(), "{:?}", output.stderr);
}
