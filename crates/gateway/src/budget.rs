//! The shared budget of the human tiers, so operator scripts cannot use up
//! the credential's Alpaca rate limit that the bot needs.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use st0x_alpaca_gateway_api::Operation;

/// How long one window of the budget lasts.
const WINDOW: Duration = Duration::from_secs(60);

/// The units a human call of `operation` takes: none for the keyed reads a
/// caller polls, one for every other call.
#[must_use]
pub const fn cost(operation: Operation) -> u32 {
    match operation {
        Operation::OrdersGet
        | Operation::ConversionsGet
        | Operation::WalletTransfer
        | Operation::WalletFindDeposit
        | Operation::TokenizationRequest
        | Operation::TokenizationFindRedemption => 0,
        _ => 1,
    }
}

/// A fixed one minute window of calls.
pub struct HumanBudget {
    per_minute: u32,
    /// Start of the current window and the units used in it.
    window: Mutex<Option<(Instant, u32)>>,
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
    pub fn take(&self, cost: u32) -> Result<(), Duration> {
        self.take_at(cost, Instant::now())
    }

    fn take_at(&self, cost: u32, now: Instant) -> Result<(), Duration> {
        if cost == 0 {
            return Ok(());
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
        Ok(())
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
        budget.take_at(cost(Operation::OrdersGet), start).unwrap();
    }
}
