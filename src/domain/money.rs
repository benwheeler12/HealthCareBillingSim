//! Money is integer cents. Never floats (DESIGN.md Decisions); reconciliation is
//! exact equality, so the representation must make rounding impossible.

use std::fmt;
use std::iter::Sum;
use std::ops::{Add, AddAssign, Mul, Sub};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct Money(i64);

impl Money {
    pub const ZERO: Money = Money(0);

    pub const fn from_cents(cents: i64) -> Money {
        Money(cents)
    }

    pub const fn cents(self) -> i64 {
        self.0
    }

    /// Convert a JSON dollar amount to cents. Amounts that are not a whole
    /// number of cents are a schema violation, not something to round away.
    pub fn try_from_dollars(dollars: f64) -> Option<Money> {
        if !dollars.is_finite() {
            return None;
        }
        let cents = (dollars * 100.0).round();
        if (dollars * 100.0 - cents).abs() > 1e-6 || cents.abs() >= i64::MAX as f64 {
            return None;
        }
        Some(Money(cents as i64))
    }

    /// `bps` basis points of this amount, floored. Used by honest adjudication
    /// to carve exact integer splits (the remainder goes to the payer).
    pub const fn bps(self, bps: u32) -> Money {
        Money(self.0 * bps as i64 / 10_000)
    }

    pub fn min(self, other: Money) -> Money {
        Money(self.0.min(other.0))
    }
}

impl Add for Money {
    type Output = Money;
    fn add(self, rhs: Money) -> Money {
        Money(self.0 + rhs.0)
    }
}

impl AddAssign for Money {
    fn add_assign(&mut self, rhs: Money) {
        self.0 += rhs.0;
    }
}

impl Sub for Money {
    type Output = Money;
    fn sub(self, rhs: Money) -> Money {
        Money(self.0 - rhs.0)
    }
}

impl Mul<u32> for Money {
    type Output = Money;
    fn mul(self, rhs: u32) -> Money {
        Money(self.0 * rhs as i64)
    }
}

impl Sum for Money {
    fn sum<I: Iterator<Item = Money>>(iter: I) -> Money {
        iter.fold(Money::ZERO, Add::add)
    }
}

impl fmt::Display for Money {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sign = if self.0 < 0 { "-" } else { "" };
        let abs = self.0.unsigned_abs();
        write!(f, "{sign}${}.{:02}", abs / 100, abs % 100)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dollars_convert_exactly_or_not_at_all() {
        assert_eq!(
            Money::try_from_dollars(123.45),
            Some(Money::from_cents(12345))
        );
        assert_eq!(Money::try_from_dollars(0.1), Some(Money::from_cents(10)));
        assert_eq!(Money::try_from_dollars(0.0), Some(Money::ZERO));
        assert_eq!(Money::try_from_dollars(1.005), None); // fractional cent
        assert_eq!(Money::try_from_dollars(f64::NAN), None);
        assert_eq!(Money::try_from_dollars(f64::INFINITY), None);
    }

    #[test]
    fn display_formats_cents() {
        assert_eq!(Money::from_cents(12345).to_string(), "$123.45");
        assert_eq!(Money::from_cents(5).to_string(), "$0.05");
        assert_eq!(Money::from_cents(-250).to_string(), "-$2.50");
    }

    #[test]
    fn bps_floors_exactly() {
        assert_eq!(
            Money::from_cents(10_000).bps(2_500),
            Money::from_cents(2_500)
        );
        assert_eq!(Money::from_cents(3).bps(3_333), Money::ZERO);
    }
}
