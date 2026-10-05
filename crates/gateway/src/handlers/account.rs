//! `account.*` and `activities.list`.

use axum::extract::State;
use axum::response::Response;
use axum::routing::{MethodRouter, get};
use st0x_alpaca::broker::AccountActivitiesQuery;
use st0x_alpaca_gateway_api::Operation;
use st0x_alpaca_gateway_api::dto::SymbolPath;
use st0x_alpaca_gateway_api::dto::account::{
    ActivitiesQuery, ActivitiesResponse, FundsResponse, InventoryResponse, PositionMarkResponse,
    WithdrawableCashResponse,
};

use crate::answer::{Sent, broker};
use crate::extract::{Params, Query};
use crate::state::{AppState, Call};

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
        .read(call, async {
            state
                .broker
                .account_funds()
                .await
                .map(FundsResponse::from)
                .map_err(|error| broker(&error, Sent::Read))
        })
        .await
}

async fn withdrawable_cash(State(state): State<AppState>, call: Call) -> Response {
    state
        .read(call, async {
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
        .read(call, async {
            state
                .broker
                .fetch_inventory()
                .await
                .map(InventoryResponse::from)
                .map_err(|error| broker(&error, Sent::Read))
        })
        .await
}

async fn position_mark(
    State(state): State<AppState>,
    call: Call,
    Params(path): Params<SymbolPath>,
) -> Response {
    state
        .read(call, async {
            state
                .broker
                .fetch_position_mark(&path.symbol)
                .await
                .map(|mark| PositionMarkResponse { mark })
                .map_err(|error| broker(&error, Sent::Read))
        })
        .await
}

async fn activities(
    State(state): State<AppState>,
    call: Call,
    Query(query): Query<ActivitiesQuery>,
) -> Response {
    let activity_types = query
        .types
        .split(',')
        .map(str::trim)
        .filter(|kind| !kind.is_empty())
        .map(str::to_string)
        .collect();

    state
        .read(call, async {
            // The account filter is the deployment's own: the context carries
            // the configured account id and no request field can change it.
            state
                .config
                .broker
                .fetch_account_activities(&AccountActivitiesQuery {
                    activity_types,
                    after: query.after,
                    until: query.until,
                })
                .await
                .map(|activities| ActivitiesResponse { activities })
                .map_err(|error| broker(&error, Sent::Read))
        })
        .await
}
