//! A single deadline, never a queue of overdue animation frames.
use std::time::Duration;
use tokio::time::Instant;

const FRAME_PERIOD: Duration = Duration::from_nanos(1_000_000_000 / 30);

pub(super) struct Frames {
    deadline: Instant,
    dirty: bool,
}

impl Frames {
    pub fn new(now: Instant) -> Self {
        Self {
            deadline: now,
            dirty: true,
        }
    }
    pub fn invalidate(&mut self) {
        self.dirty = true;
    }
    pub fn pending(&self, animated: bool) -> bool {
        self.dirty || animated
    }
    pub fn deadline(&self) -> Instant {
        self.deadline
    }
    pub fn due(&self, now: Instant, animated: bool) -> bool {
        self.pending(animated) && now >= self.deadline
    }
    pub fn drawn(&mut self, now: Instant) {
        self.dirty = false;
        self.deadline = now + FRAME_PERIOD;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_resume_and_stalls_do_not_accumulate_frames() {
        let start = Instant::now();
        let mut frames = Frames::new(start);
        assert!(frames.due(start, false));
        frames.drawn(start);
        let later = start + Duration::from_secs(300);
        assert!(!frames.due(later, false));
        frames.invalidate();
        assert!(frames.due(later, false));
        frames.drawn(later);
        assert!(!frames.due(later, true));
        assert!(frames.due(later + FRAME_PERIOD, true));
    }

    #[test]
    fn event_flood_cannot_speed_up_or_postpone_frames() {
        let start = Instant::now();
        let mut frames = Frames::new(start);
        frames.drawn(start);
        for millis in 0..33 {
            frames.invalidate();
            assert!(!frames.due(start + Duration::from_millis(millis), false));
        }
        assert!(frames.due(start + FRAME_PERIOD, false));
    }
}
