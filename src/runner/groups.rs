use super::*;

/// Process groups of currently-spawned children (each == that child's pid,
/// because we spawn with `process_group(0)`). The signal handler walks this to
/// kill the whole tree on Ctrl-C/SIGTERM; a 12-minute orphan burning tokens
/// after the runner died was the motivator.
static RUNNING_GROUPS: StdMutex<Vec<i32>> = StdMutex::new(Vec::new());

/// Register the current child group and unregister it on drop (which fires
/// even on the `?`/panic paths, so a reaped pid never lingers in the list and
/// a later `killpg` never targets a recycled pid).
pub(crate) struct GroupRegistration {
    pgid: Option<i32>,
}

impl GroupRegistration {
    pub(crate) fn register(pid: Option<u32>) -> Self {
        let pgid = pid.map(|p| p as i32);
        if let Some(pg) = pgid {
            RUNNING_GROUPS.lock().unwrap().push(pg);
        }
        Self { pgid }
    }

    /// SIGTERM the group, give it a moment to exit on its own, then SIGKILL.
    /// Reaching the whole group (not just the direct child) is the point: the
    /// agent CLIs spawn their own tool subprocesses.
    pub(super) async fn kill_group(&self) {
        let Some(pg) = self.pgid else { return };
        #[cfg(unix)]
        {
            // ESRCH (already gone) is fine to ignore here.
            unsafe { libc::killpg(pg, libc::SIGTERM) };
            tokio::time::sleep(Duration::from_millis(1500)).await;
            unsafe { libc::killpg(pg, libc::SIGKILL) };
        }
    }
}

impl Drop for GroupRegistration {
    fn drop(&mut self) {
        if let Some(pg) = self.pgid {
            let mut g = RUNNING_GROUPS.lock().unwrap();
            if let Some(pos) = g.iter().position(|&x| x == pg) {
                g.remove(pos);
            }
        }
    }
}

/// Install Ctrl-C / SIGTERM handling. Without this, tearing down the tokio
/// runtime does not run `Drop` on the in-flight `Command`s, so `kill_on_drop`
/// never fires and every agent CLI becomes an orphan. On a signal we kill all
/// live child groups and exit — we do not try to unwind the run.
pub fn install_death_handlers() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        tokio::spawn(async {
            #[cfg(unix)]
            let sig = {
                use tokio::signal::unix::{signal, SignalKind};
                let mut term = match signal(SignalKind::terminate()) {
                    Ok(s) => s,
                    Err(_) => return,
                };
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => "SIGINT",
                    _ = term.recv() => "SIGTERM",
                }
            };
            #[cfg(not(unix))]
            let sig = {
                tokio::signal::ctrl_c().await.ok();
                "SIGINT"
            };
            // die() exits the process, so there is nothing to re-arm: one
            // wait, then every live child group dies with us.
            die(sig);
        });
    });
}

/// Kill every registered child group and exit with a signal-appropriate code.
fn die(sig: &str) -> ! {
    let pgs: Vec<i32> = RUNNING_GROUPS.lock().unwrap().clone();
    if !pgs.is_empty() {
        eprintln!(
            "zori: {sig} received — killing {} running child group(s)",
            pgs.len()
        );
        #[cfg(unix)]
        {
            for &pg in &pgs {
                unsafe { libc::killpg(pg, libc::SIGTERM) };
            }
            std::thread::sleep(Duration::from_millis(1500));
            for &pg in &pgs {
                unsafe { libc::killpg(pg, libc::SIGKILL) };
            }
        }
    }
    // 128 + signal: 130 for SIGINT, 143 for SIGTERM (conventional).
    std::process::exit(if sig == "SIGTERM" { 143 } else { 130 });
}
