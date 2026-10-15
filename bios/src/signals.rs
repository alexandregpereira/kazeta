// Termination signal handling for the launcher.
// Instead of dying on SIGTERM/SIGINT/SIGHUP, the signal is recorded and
// forwarded to the running child so the normal cleanup path still runs.

use std::process::{Child, ExitStatus};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
use signal_hook::iterator::Signals;

const POLL_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Clone)]
pub struct Terminated(Arc<AtomicBool>);

impl Terminated {
    pub fn get(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// Start catching termination signals; must be called before spawning children
pub fn install() -> Terminated {
    let flag = Arc::new(AtomicBool::new(false));
    match Signals::new([SIGTERM, SIGINT, SIGHUP]) {
        Ok(mut signals) => {
            let thread_flag = flag.clone();
            thread::spawn(move || {
                for sig in signals.forever() {
                    eprintln!("Received signal {}, shutting down", sig);
                    thread_flag.store(true, Ordering::Relaxed);
                }
            });
        }
        Err(e) => eprintln!("Failed to install signal handlers: {}", e),
    }
    Terminated(flag)
}

/// Send `signal` to the process group led by `child` (see `Command::process_group(0)`),
/// or to the child alone if it does not lead a group
fn signal_group(child: &Child, signal: libc::c_int) {
    let pid = child.id() as libc::pid_t;
    if unsafe { libc::kill(-pid, signal) } != 0 {
        unsafe { libc::kill(pid, signal) };
    }
}

/// Wait for `child` to exit, passing on SIGTERM once `stop` returns true (e.g. after a
/// termination signal) and killing it if it is still running after `kill_timeout`.
/// The child should lead its own process group: the signals then reach everything it started,
/// and whatever is left of the group once it has exited is killed, so no leftover process keeps
/// the cart mounted or its output pipe open.
pub fn wait_child(child: &mut Child, stop: &dyn Fn() -> bool, kill_timeout: Duration) -> std::io::Result<ExitStatus> {
    let mut forwarded_at: Option<Instant> = None;
    loop {
        if let Some(status) = child.try_wait()? {
            // a group outlives its leader while any member is left, so its id is not reused yet
            unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL) };
            return Ok(status);
        }

        // the child has not been reaped yet, so its pid cannot have been reused
        if stop() {
            match forwarded_at {
                None => {
                    signal_group(child, libc::SIGTERM);
                    forwarded_at = Some(Instant::now());
                }
                Some(at) if at.elapsed() > kill_timeout => {
                    eprintln!("Child {} did not exit, killing it", child.id());
                    signal_group(child, libc::SIGKILL);
                }
                _ => {}
            }
        }

        thread::sleep(POLL_INTERVAL);
    }
}
