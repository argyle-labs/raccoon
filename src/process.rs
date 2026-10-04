//! Child processes with a deadline. A wedged child (a ludusavi scan stuck on a
//! dead network mount, a hung `tar`) must not hold the plugin's serial socket
//! forever, so every spawn the game-saves path makes goes through here.

use std::io::Read;
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// Run `cmd` to completion or kill it at `timeout`. Stdout/stderr are drained
/// on threads so a chatty child can't block on a full pipe; stdin is null.
pub fn run_bounded(mut cmd: Command, timeout: Duration) -> Result<Output, String> {
    let program = cmd.get_program().to_string_lossy().into_owned();
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn {program}: {e}"))?;
    let out = drain(child.stdout.take());
    let err = drain(child.stderr.take());
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if Instant::now() >= deadline => {
                kill(&mut child);
                return Err(format!("{program} timed out after {}s", timeout.as_secs()));
            }
            Ok(None) => thread::sleep(Duration::from_millis(50)),
            Err(e) => {
                kill(&mut child);
                return Err(format!("wait {program}: {e}"));
            }
        }
    };
    Ok(Output {
        status,
        stdout: out.join().unwrap_or_default(),
        stderr: err.join().unwrap_or_default(),
    })
}

fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut p) = pipe
            && let Err(e) = p.read_to_end(&mut buf)
        {
            plugin_toolkit::tracing::warn!("child pipe read failed: {e}");
        }
        buf
    })
}

/// Kill and reap, so a timed-out child never lingers as a zombie.
fn kill(child: &mut Child) {
    if let Err(e) = child.kill() {
        plugin_toolkit::tracing::warn!("kill pid {}: {e}", child.id());
    }
    if let Err(e) = child.wait() {
        plugin_toolkit::tracing::warn!("reap pid {}: {e}", child.id());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completes_and_captures_output() {
        let mut c = Command::new("sh");
        c.args(["-c", "echo out; echo err >&2"]);
        let o = run_bounded(c, Duration::from_secs(10)).unwrap();
        assert!(o.status.success());
        assert_eq!(String::from_utf8_lossy(&o.stdout), "out\n");
        assert_eq!(String::from_utf8_lossy(&o.stderr), "err\n");
    }

    #[test]
    fn kills_at_the_deadline() {
        let mut c = Command::new("sleep");
        c.arg("30");
        let start = Instant::now();
        let e = run_bounded(c, Duration::from_millis(200)).unwrap_err();
        assert!(e.contains("timed out"), "{e}");
        assert!(start.elapsed() < Duration::from_secs(5));
    }
}
