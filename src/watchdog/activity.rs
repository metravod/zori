//! Evidence local to one running attempt. Coddy saves tool turns even while
//! its print-mode stdout is quiet. Its small session header timestamps actual
//! transcript writes separately from lock, UI and stats updates.

use crate::state::SessionRef;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncRead, ReadBuf};

pub(crate) const EXTENSION: Duration = Duration::from_secs(600);

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Transcript {
    message_count: usize,
    messages_mod_time: u64, // Unix nanoseconds, from Coddy's session.json
}

fn transcript(path: &Path) -> Option<Transcript> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

#[derive(Default, Clone)]
pub(crate) struct OutputActivity(Arc<Mutex<Option<Instant>>>);

// Observe bytes before the line reader buffers them. An agent can be
// streaming a single paragraph when the deadline arrives.
pub(crate) struct ObservedOutput<R>(pub R, pub Option<OutputActivity>);

impl<R: AsyncRead + Unpin> AsyncRead for ObservedOutput<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let result = Pin::new(&mut self.0).poll_read(cx, buf);
        if let Some(activity) = &self.1 {
            if buf.filled()[before..]
                .iter()
                .any(|b| !b.is_ascii_whitespace())
            {
                *activity.0.lock().unwrap() = Some(Instant::now());
            }
        }
        result
    }
}

pub(crate) struct Activity {
    pub stdout: OutputActivity,
    header: Option<PathBuf>,
    previous_write: u64,
    // A resumed session can gain one user message just by starting.
    initial_messages: usize,
}

impl Activity {
    pub fn new(session: Option<&SessionRef>) -> Self {
        let header = match session {
            Some(SessionRef::Coddy {
                sessions_dir,
                session_id,
            }) => Some(sessions_dir.join(session_id).join("session.json")),
            _ => None,
        };
        let initial = header.as_deref().and_then(transcript);
        Self {
            stdout: OutputActivity::default(),
            previous_write: initial.as_ref().map_or(0, |t| t.messages_mod_time),
            initial_messages: initial.map_or(0, |t| t.message_count),
            header,
        }
    }

    // Consume both signals so the same evidence cannot buy a second extension.
    pub fn take_recent(&mut self, now: Instant, wall: SystemTime) -> Option<&'static str> {
        let output = self.stdout.0.lock().unwrap().take();
        let session = self
            .header
            .as_deref()
            .and_then(transcript)
            .is_some_and(|t| {
                let changed = t.messages_mod_time > self.previous_write;
                self.previous_write = self.previous_write.max(t.messages_mod_time);
                let only_prompt = t.message_count <= 1
                    || t.message_count == self.initial_messages.saturating_add(1);
                let written = UNIX_EPOCH + Duration::from_nanos(t.messages_mod_time);
                changed
                    && !only_prompt
                    && wall
                        .duration_since(written)
                        .is_ok_and(|age| age <= EXTENSION)
            });
        if session {
            Some("Coddy session advanced")
        } else if output.is_some_and(|t| now.saturating_duration_since(t) <= EXTENSION) {
            Some("agent produced output")
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn partial_output_earns_each_extension_once_and_expires() {
        let mut a = Activity::new(None);
        let wall = SystemTime::now();
        for bytes in [b"first".as_slice(), b" more tokens"] {
            ObservedOutput(bytes, Some(a.stdout.clone()))
                .read_to_end(&mut Vec::new())
                .await
                .unwrap();
            assert_eq!(
                a.take_recent(Instant::now(), wall),
                Some("agent produced output")
            );
            assert_eq!(a.take_recent(Instant::now(), wall), None);
        }
        ObservedOutput(b" \n".as_slice(), Some(a.stdout.clone()))
            .read_to_end(&mut Vec::new())
            .await
            .unwrap();
        assert_eq!(a.take_recent(Instant::now(), wall), None);
        *a.stdout.0.lock().unwrap() = Some(Instant::now());
        assert_eq!(
            a.take_recent(Instant::now() + EXTENSION + Duration::from_secs(1), wall),
            None
        );
    }

    #[test]
    fn resumed_coddy_requires_fresh_progress_after_its_prompt() {
        let dir = std::env::temp_dir().join(crate::artifacts::new_run_id("coddy-activity"));
        let session = SessionRef::Coddy {
            sessions_dir: dir.clone(),
            session_id: "sess_test".into(),
        };
        let folder = dir.join("sess_test");
        std::fs::create_dir_all(&folder).unwrap();
        let stamp = |count: usize, secs: u64| {
            let json =
                serde_json::json!({"messageCount": count, "messagesModTime": secs * 1_000_000_000});
            std::fs::write(folder.join("session.json"), json.to_string()).unwrap();
        };
        let now = Instant::now();
        let wall = UNIX_EPOCH + Duration::from_secs(2000);
        stamp(12, 1850);
        let mut a = Activity::new(Some(&session));
        assert_eq!(a.take_recent(now, wall), None); // earlier attempt
        stamp(13, 1890);
        assert_eq!(a.take_recent(now, wall), None); // resume prompt alone
        stamp(15, 1900);
        assert!(a.take_recent(now, wall).is_some());
        assert_eq!(a.take_recent(now, wall + EXTENSION), None);
        stamp(17, 1950);
        assert_eq!(a.take_recent(now, wall + EXTENSION), None); // too old
        stamp(19, 2590);
        assert!(a.take_recent(now, wall + EXTENSION).is_some());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
