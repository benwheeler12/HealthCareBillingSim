//! Virtual time. All timestamps are durations since simulation start on the
//! paused tokio clock — never wall-clock (Decisions #12).

use std::fmt;
use std::ops::Add;
use std::time::Duration;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
pub struct VirtualTime(Duration);

impl VirtualTime {
    pub const fn as_duration(self) -> Duration {
        self.0
    }

    pub fn saturating_since(self, earlier: VirtualTime) -> Duration {
        self.0.saturating_sub(earlier.0)
    }
}

impl Add<Duration> for VirtualTime {
    type Output = VirtualTime;
    fn add(self, rhs: Duration) -> VirtualTime {
        VirtualTime(self.0 + rhs)
    }
}

impl fmt::Display for VirtualTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "t+{:.3}s", self.0.as_secs_f64())
    }
}

/// Handle on the simulation's start instant; cloned into every component that
/// stamps events. Reads the paused tokio clock, so `now()` is virtual.
#[derive(Clone, Debug)]
pub struct Clock {
    start: tokio::time::Instant,
}

impl Clock {
    /// Must be called inside the (paused) runtime, at simulation start.
    pub fn start() -> Clock {
        Clock { start: tokio::time::Instant::now() }
    }

    pub fn now(&self) -> VirtualTime {
        VirtualTime(self.start.elapsed())
    }
}
