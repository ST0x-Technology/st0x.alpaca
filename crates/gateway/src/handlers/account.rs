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
        .read(call, Intent::default(), async {
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
        .read(call, Intent::default(), async {
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
        .read(call, Intent::default(), async {
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
    let intent = Intent::default().key(&path.symbol);
    state
        .read(call, intent, async {
            state
                .broker
                .fetch_position_mark(&path.symbol)
                .await
                .map(|mark| PositionMarkResponse { mark })
                .map_err(|error| broker(&error, Sent::Read))
        })
        .await
}

/// Page cap of a bot `activities.list`, the direct context method's cap.
const BOT_ACTIVITY_PAGES: usize = 1000;

/// Pages `activities.list` may read for `call`. A human call reads at most
/// the units its admission charged, one per Alpaca request, so the shared
/// human budget counts every page it can send.
fn activity_pages(call: &Call) -> usize {
    if call.principal.tier.is_human() {
        usize::try_from(call.operation.human_budget_cost()).unwrap_or(0)
    } else {
        BOT_ACTIVITY_PAGES
    }
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
    let max_pages = activity_pages(&call);

    state
        .read(call, Intent::default(), async {
            state
                .broker
                .fetch_account_activities(&query, max_pages)
                .await
                .map(|activities| ActivitiesResponse {
                    activities: activities.into_iter().map(Into::into).collect(),
                })
                .map_err(|error| broker(&error, Sent::Read))
        })
        .await
}
