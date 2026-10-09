//! One handler per catalog operation, grouped by surface.

mod account;
mod market;
mod orders;
mod tokenization;
mod wallet;

use axum::routing::MethodRouter;
use st0x_alpaca_gateway_api::Operation;

use crate::state::AppState;

/// The handler serving `operation`. Every catalog operation has one.
pub(crate) fn route(operation: Operation) -> Option<MethodRouter<AppState>> {
    account::route(operation)
        .or_else(|| market::route(operation))
        .or_else(|| orders::route(operation))
        .or_else(|| wallet::route(operation))
        .or_else(|| tokenization::route(operation))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_operation_has_a_handler() {
        for operation in Operation::ALL {
            assert!(route(operation).is_some(), "{operation} has no handler");
        }
    }
}
