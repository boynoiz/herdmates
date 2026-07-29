//! Getting a command into a pane that is still starting up.
//!
//! `herdr pane run` writes bytes straight to the pty, so anything submitted
//! before the pane's shell — or a just-launched agent's TUI — is reading input
//! gets discarded, and its own redraw wipes the text off screen. Nothing in the
//! herdr API reports that: `pane run` returns Ok for bytes it merely wrote, and
//! the pane's own text shows the command whether or not anything read it,
//! because herdr renders those bytes on arrival.
//!
//! So submission is bracketed: wait for the pane to stop drawing before typing
//! ([`wait_until_quiet`]), and where a process is expected afterwards, confirm
//! one exists ([`job_started`]) rather than trusting the submit.

use crate::herdr::HerdrApi;
use std::thread;
use std::time::{Duration, Instant};

/// How long `respawn-pane` waits for a freshly split pane's shell to finish
/// starting before it types into it.
#[derive(Debug, Clone, Copy)]
pub struct SubmitPolicy {
    /// Upper bound on the wait. A shell that has not settled by now is not
    /// going to; the command goes out anyway rather than failing a spawn that
    /// might still work.
    pub settle_timeout: Duration,
    pub poll_interval: Duration,
    /// Consecutive identical reads that count as "the terminal has gone
    /// quiet". Two is enough to distinguish a finished prompt from a pane
    /// caught mid-redraw; a login greeting draws in several bursts.
    pub quiet_reads: usize,
    /// How long the submitted command gets to become a running process. The
    /// shell forks as soon as it reads the line, so this covers scheduling
    /// and herdr's polling interval, not the agent's own startup.
    pub job_timeout: Duration,
}

impl Default for SubmitPolicy {
    fn default() -> Self {
        Self {
            settle_timeout: Duration::from_secs(10),
            poll_interval: Duration::from_millis(200),
            quiet_reads: 2,
            job_timeout: Duration::from_secs(10),
        }
    }
}

/// Block until `pane_id` stops drawing, or the policy's patience runs out.
///
/// Read failures are not fatal: this is a courtesy wait in front of a submit
/// that will be attempted regardless, and a spawn should not fail because a
/// readiness probe did.
pub fn wait_until_quiet<H: HerdrApi>(herdr: &H, pane_id: &str, policy: SubmitPolicy) {
    let deadline = Instant::now() + policy.settle_timeout;
    let mut previous: Option<String> = None;
    let mut quiet = 0;
    loop {
        if let Ok(output) = herdr.pane_read_unwrapped(pane_id) {
            if !output.is_empty() && previous.as_deref() == Some(output.as_str()) {
                quiet += 1;
                if quiet >= policy.quiet_reads {
                    return;
                }
            } else {
                quiet = 0;
            }
            previous = Some(output);
        }
        let now = Instant::now();
        if now >= deadline {
            return;
        }
        thread::sleep(policy.poll_interval.min(deadline - now));
    }
}

/// Poll until something is running in the pane, or the policy gives up.
pub fn job_started<H: HerdrApi>(
    herdr: &H,
    pane_id: &str,
    policy: SubmitPolicy,
) -> Result<bool, crate::herdr::HerdrError> {
    let deadline = Instant::now() + policy.job_timeout;
    loop {
        match herdr.pane_process_info(pane_id) {
            Ok(info) if info.job_running() => return Ok(true),
            Ok(_) => {}
            Err(error) => return Err(error),
        }
        let now = Instant::now();
        if now >= deadline {
            return Ok(false);
        }
        thread::sleep(policy.poll_interval.min(deadline - now));
    }
}
