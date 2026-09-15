//! Redraw pacing shared by the inline applets.
//!
//! Every applet runs `loop { draw; poll; handle }`. Layout, wrapping, and
//! highlighting only need to run again after something visible changed, so
//! the loops draw when a [`Redraw`] request is pending and otherwise wait
//! [`IDLE_POLL`] between checks of their termination flags. Terminal input
//! still wakes `crossterm::event::poll` immediately; the timeout only bounds
//! how long a signal or a non-terminal event source (bridge frames, LSP
//! replies) waits for the next iteration.
use std::time::{Duration, Instant};

/// Poll timeout while nothing animates and no frames are expected.
pub const IDLE_POLL: Duration = Duration::from_millis(250);
/// Poll timeout while related updates are likely to follow soon.
pub const ACTIVE_POLL: Duration = Duration::from_millis(80);
/// How long after the last activity the short poll stays in effect.
pub const ACTIVE_WINDOW: Duration = Duration::from_secs(2);

/// Pending-frame flag with an activity window for the poll cadence.
#[derive(Debug)]
pub struct Redraw {
    pending: bool,
    active_until: Option<Instant>,
}

impl Default for Redraw {
    /// The first frame is always due.
    fn default() -> Self {
        Self {
            pending: true,
            active_until: None,
        }
    }
}

impl Redraw {
    pub fn new() -> Self {
        Self::default()
    }

    /// Something visible changed; draw before waiting again.
    pub fn request(&mut self) {
        self.pending = true;
    }

    /// Like [`Redraw::request`], and keep polling at [`ACTIVE_POLL`] for
    /// [`ACTIVE_WINDOW`] because more updates usually follow (a bridge sends
    /// snapshots once a second while an agent streams).
    pub fn activity(&mut self, now: Instant) {
        self.pending = true;
        self.active_until = Some(now + ACTIVE_WINDOW);
    }

    /// Whether a frame is due, clearing the request.
    pub fn take(&mut self) -> bool {
        std::mem::replace(&mut self.pending, false)
    }

    /// Poll timeout for the next wait.
    pub fn poll_timeout(&self, now: Instant) -> Duration {
        if self.active_until.is_some_and(|until| now < until) {
            ACTIVE_POLL
        } else {
            IDLE_POLL
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_frame_is_due_then_only_after_requests() {
        let mut redraw = Redraw::new();
        assert!(redraw.take());
        assert!(!redraw.take(), "idle timeouts must not draw");
        redraw.request();
        assert!(redraw.take());
        assert!(!redraw.take());
    }

    #[test]
    fn activity_shortens_the_poll_only_for_its_window() {
        let now = Instant::now();
        let mut redraw = Redraw::new();
        assert_eq!(redraw.poll_timeout(now), IDLE_POLL);
        redraw.activity(now);
        assert!(redraw.take());
        assert_eq!(redraw.poll_timeout(now), ACTIVE_POLL);
        assert_eq!(redraw.poll_timeout(now + ACTIVE_WINDOW / 2), ACTIVE_POLL);
        assert_eq!(redraw.poll_timeout(now + ACTIVE_WINDOW), IDLE_POLL);
    }
}
