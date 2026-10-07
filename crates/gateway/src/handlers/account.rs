//! `account.*` and `activities.list`.

use std::num::NonZeroUsize;

use axum::extract::State;
use axum::response::Response;
use axum::routing::{MethodRouter, get};
use st0x_alpaca::broker::AccountActivitiesQuery;
use st0x_alpaca_gateway_api::dto::SymbolPath;
use st0x_alpaca_gateway_api::dto::account::{
    ActivitiesQuery, ActivitiesResponse, PositionMarkResponse, WithdrawableCashResponse,
};
use st0x_alpaca_gateway_api::{Operation, Tier};

use crate::answer::{Sent, broker};
use crate::extract::{Params, Query};
use crate::state::{AppState, Call, Intent};

pub(super) fn route(operation: Operation) -> Option<MethodRouter<AppState>> {
    Some(match operation {
        Operation::AccountFunds => get(funds),
        Operation::AccountWithdrawableCash => get(withdrawable_cash),
        Operation::AccountInventory => get(inventory),
        Operation::AccountPositionMark => get(position_mark),
        Operation::ActivitiesList => get(activities),
        _ => return None,
    })
}

async fn funds(State(state): State<AppState>, call: Call) -> Response {
    state
        .read(call, Intent::default(), |state| async move {
            state
                .broker
                .account_funds()
                .await
                .map_err(|error| broker(&error, Sent::Read))
        })
        .await
}

async fn withdrawable_cash(State(state): State<AppState>, call: Call) -> Response {
    state
        .read(call, Intent::default(), |state| async move {
            state
                .broker
                .withdrawable_cash_cents()
                .await
                .map(|withdrawable_cents| WithdrawableCashResponse { withdrawable_cents })
                .map_err(|error| broker(&error, Sent::Read))
        })
        .await
}

async fn inventory(State(state): State<AppState>, call: Call) -> Response {
    state
        .read(call, Intent::default(), |state| async move {
            state
                .broker
                .fetch_inventory()
                .await
                .map_err(|error| broker(&error, Sent::Read))
        })
        .await
}

async fn position_mark(
    State(state): State<AppState>,
    call: Call,
    Params(path): Params<SymbolPath>,
) -> Response {
    let intent = Intent::default().key(&path.symbol);
    state
        .read(call, intent, |state| async move {
            state
                .broker
                .fetch_position_mark(&path.symbol)
                .await
                .map(|mark| PositionMarkResponse { mark })
                .map_err(|error| broker(&error, Sent::Read))
        })
        .await
}

/// Pages `activities.list` may read: the direct context method's cap for
/// the bot, and a small one for a human call, which takes one unit of the
/// human budget.
fn activity_pages(tier: Tier) -> NonZeroUsize {
    // 10 and 1000, built without an unwrap.
    const HUMAN: NonZeroUsize = NonZeroUsize::MIN.saturating_add(9);
    const BOT: NonZeroUsize = NonZeroUsize::MIN.saturating_add(999);
    if tier.is_human() { HUMAN } else { BOT }
}

async fn activities(
    State(state): State<AppState>,
    call: Call,
    Query(query): Query<ActivitiesQuery>,
) -> Response {
    // The account filter is the deployment's own: the broker carries the
    // configured account id and no request field can change it.
    let query = AccountActivitiesQuery {
        activity_types: query
            .types
            .split(',')
            .map(str::trim)
            .filter(|kind| !kind.is_empty())
            .map(str::to_string)
            .collect(),
        after: query.after,
        until: query.until,
    };
    let max_pages = activity_pages(call.principal.tier);

    state
        .read(call, Intent::default(), move |state| async move {
            state
                .broker
                .fetch_account_activities(&query, max_pages)
                .await
                .map(|activities| ActivitiesResponse { activities })
                .map_err(|error| broker(&error, Sent::Read))
        })
        .await
}
