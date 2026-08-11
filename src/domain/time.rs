//! Virtual time. All timestamps are durations since simulation start,
//! computed — never read from any clock, wall or paused (DESIGN.md 'Virtual time').

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
        let secs = self.0.as_secs_f64();
        if secs >= 60.0 {
            write!(f, "t+{}", human_virtual(secs))
        } else {
            write!(f, "t+{secs:.3}s")
        }
    }
}

/// Render a virtual duration in the units a practice owner thinks in.
/// Wall-clock durations (program runtime) should NOT use this — they stay in
/// real seconds; this is for simulated time only.
pub fn human_virtual(secs: f64) -> String {
    match secs {
        s if s >= 86_400.0 => format!("{:.1} virtual days", s / 86_400.0),
        s if s >= 3_600.0 => format!("{:.1} virtual hours", s / 3_600.0),
        s if s >= 60.0 => format!("{:.1} virtual minutes", s / 60.0),
        s => format!("{s:.0} virtual seconds"),
    }
}
