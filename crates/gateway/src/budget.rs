//! The shared budget of the human tiers, so operator scripts cannot use up
//! the credential's Alpaca rate limit that the bot needs.

use std::sync::Mutex;
use std::time::{Duration, Instant};

const WINDOW: Duration = Duration::from_secs(60);

/// A fixed one minute window of cost units.
pub struct HumanBudget {
    per_minute: u32,
    /// Start of the current window and the units used in it.
    window: Mutex<Option<(Instant, u32)>>,
}

/// Units taken from one window, so they can be given back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Charge {
    cost: u32,
    window: Option<Instant>,
}

impl HumanBudget {
    #[must_use]
    pub fn new(per_minute: u32) -> Self {
        Self {
            per_minute,
            window: Mutex::new(None),
        }
    }

    /// Takes `cost` units, or returns how long until the window resets.
    ///
    /// # Errors
    ///
    /// Returns the wait until the next window when the budget is spent.
    pub fn take(&self, cost: u32) -> Result<Charge, Duration> {
        self.take_at(cost, Instant::now())
    }

    /// Keeps `used` units of `charge`, at most its cost, and gives the rest
    /// back: an operation reserves the most Alpaca requests it can send and
    /// pays for the ones it sent. Units of a window that already ended stay
    /// spent: the new window never grows past its limit.
    pub fn settle(&self, charge: Charge, used: u32) {
        self.settle_at(charge, used, Instant::now());
    }

    fn take_at(&self, cost: u32, now: Instant) -> Result<Charge, Duration> {
        if cost == 0 {
            return Ok(Charge { cost, window: None });
        }
        let Ok(mut window) = self.window.lock() else {
            return Err(WINDOW);
        };
        let (started, used) = window.get_or_insert((now, 0));
        if now.duration_since(*started) >= WINDOW {
            *started = now;
            *used = 0;
        }
        if used.saturating_add(cost) > self.per_minute {
            return Err(WINDOW.saturating_sub(now.duration_since(*started)));
        }
        *used += cost;
        Ok(Charge {
            cost,
            window: Some(*started),
        })
    }

    fn settle_at(&self, charge: Charge, used: u32, now: Instant) {
        let unused = charge.cost.saturating_sub(used);
        let Some(charged_window) = charge.window.filter(|_| unused > 0) else {
            return;
        };
        let Ok(mut window) = self.window.lock() else {
            return;
        };
        if let Some((started, spent)) = window.as_mut()
            && *started == charged_window
            && now.duration_since(*started) < WINDOW
        {
            *spent = spent.saturating_sub(unused);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spends_up_to_the_limit_then_waits_for_the_window() {
        let budget = HumanBudget::new(3);
        let start = Instant::now();
        budget.take_at(2, start).unwrap();
        budget.take_at(1, start).unwrap();
        let wait = budget
            .take_at(1, start + Duration::from_secs(20))
            .unwrap_err();
        assert_eq!(wait, Duration::from_secs(40));
        budget.take_at(3, start + WINDOW).unwrap();
    }

    #[test]
    fn free_operations_never_wait() {
        let budget = HumanBudget::new(1);
        let start = Instant::now();
        budget.take_at(1, start).unwrap();
        budget.take_at(0, start).unwrap();
    }

    #[test]
    fn settling_gives_back_only_the_units_not_used_in_the_same_window() {
        let budget = HumanBudget::new(10);
        let start = Instant::now();
        let charge = budget.take_at(10, start).unwrap();
        budget.take_at(1, start).unwrap_err();

        budget.settle_at(charge, 3, start + Duration::from_secs(1));

        let later = start + Duration::from_secs(2);
        budget.take_at(7, later).unwrap();
        budget.take_at(1, later).unwrap_err();
    }

    #[test]
    fn settling_with_every_unit_used_gives_nothing_back() {
        let budget = HumanBudget::new(2);
        let start = Instant::now();
        let charge = budget.take_at(2, start).unwrap();

        budget.settle_at(charge, 5, start + Duration::from_secs(1));

        budget
            .take_at(1, start + Duration::from_secs(2))
            .unwrap_err();
    }

    #[test]
    fn settling_a_charge_from_an_ended_window_does_not_grow_the_next_one() {
        let budget = HumanBudget::new(2);
        let start = Instant::now();
        let old = budget.take_at(2, start).unwrap();
        let later = start + WINDOW;
        budget.take_at(2, later).unwrap();

        budget.settle_at(old, 0, later + Duration::from_secs(1));

        budget
            .take_at(1, later + Duration::from_secs(2))
            .unwrap_err();
    }
}
