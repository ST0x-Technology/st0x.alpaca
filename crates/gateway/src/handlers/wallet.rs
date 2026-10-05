//! `wallet.*` and `journals.create`.
//!
//! Every destination a mutation here can move value to is pinned by the
//! deployment's config and checked before anything is sent to Alpaca.

use alloy_primitives::Address;
use axum::extract::State;
use axum::response::Response;
use axum::routing::{MethodRouter, get, post};
use st0x_alpaca::broker::AlpacaAccountId;
use st0x_alpaca::wallet::{
    ReportedFeesError, TokenSymbol, TravelRuleInfo, WhitelistEntry, WhitelistStatus,
};
use st0x_alpaca_gateway_api::dto::wallet::{
    AddressPath, DepositAddressQuery, DepositAddressResponse, DepositResponse,
    JournalCreateRequest, JournalCreateResponse, Transfer, TransferIdPath, TransferResponse,
    TransfersResponse, TravelRulePatchRequest, TxHashPath, WhitelistCreateRequest,
    WhitelistRemoveRequest, WhitelistResponse, WithdrawRequest,
};
use st0x_alpaca_gateway_api::{ErrorCode, Operation, RejectionReason, Tier};

use crate::answer::{Failure, Sent, broker, wallet};
use crate::config::{JournalConfig, WalletConfig};
use crate::extract::{Body, Params, Query};
use crate::state::{AppState, Call, Done, Intent};

pub(super) fn route(operation: Operation) -> Option<MethodRouter<AppState>> {
    Some(match operation {
        Operation::WalletWithdraw => post(withdraw),
        Operation::WalletTransfer => get(transfer),
        Operation::WalletTransfers => get(transfers),
        Operation::WalletFindDeposit => get(find_deposit),
        Operation::WalletDepositAddress => get(deposit_address),
        Operation::WalletWhitelist => get(whitelist),
        Operation::WalletWhitelistCreate => post(whitelist_create),
        Operation::WalletWhitelistRemove => post(whitelist_remove),
        Operation::WalletWhitelistPatchTravelRule => post(whitelist_patch_travel_rule),
        Operation::JournalsCreate => post(journal_create),
        _ => return None,
    })
}

/// Refuses a withdrawal destination outside the caller's lane: the bot may
/// only withdraw to the pinned destinations, and a human never to one of
/// them, so a human withdrawal can never pass for a bot one.
fn check_withdrawal_destination(
    tier: Tier,
    config: &WalletConfig,
    address: &Address,
) -> Result<(), Failure> {
    let pinned = config.bot_withdrawal_destinations.contains(address);
    match (tier, pinned) {
        (Tier::Bot, false) => Err(Failure::destination_not_allowed(format!(
            "{address} is not a withdrawal destination of the bot"
        ))),
        (Tier::Read | Tier::Write, true) => Err(Failure::destination_not_allowed(format!(
            "{address} is reserved for bot withdrawals"
        ))),
        _ => Ok(()),
    }
}

/// Whether `entries` hold an approved entry for `address` and `asset`. The
/// chain is not compared, as in the library: Alpaca answers `ETH` for a
/// request made with `ethereum`.
fn has_approved_entry(entries: &[WhitelistEntry], address: &Address, asset: &TokenSymbol) -> bool {
    entries.iter().any(|entry| {
        entry.address == *address
            && entry.asset == *asset
            && entry.status == WhitelistStatus::Approved
    })
}

/// The configured Travel Rule beneficiary, or `capability_disabled` when the
/// deployment has none.
fn travel_rule(config: &WalletConfig) -> Result<TravelRuleInfo, Failure> {
    config
        .travel_rule_beneficiary
        .clone()
        .map(TravelRuleInfo::new)
        .ok_or_else(|| {
            Failure::new(
                ErrorCode::CapabilityDisabled,
                "no Travel Rule beneficiary is configured on this deployment",
            )
        })
}

/// The account behind a configured counterparty name.
fn counterparty_account(config: &JournalConfig, name: &str) -> Result<AlpacaAccountId, Failure> {
    if config.counterparties.is_empty() {
        return Err(Failure::new(
            ErrorCode::CapabilityDisabled,
            "no journal counterparty is configured on this deployment",
        ));
    }
    config.counterparties.get(name).copied().ok_or_else(|| {
        Failure::destination_not_allowed(format!("{name} is not a configured journal counterparty"))
    })
}

/// Refuses a value that would not stay one query parameter: the library
/// writes it into the URL unescaped.
fn check_query_value(field: &str, value: &str) -> Result<(), Failure> {
    if !value.is_empty() && value.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        Ok(())
    } else {
        Err(Failure::invalid(format!(
            "{field} must be letters, digits or underscores"
        )))
    }
}

/// Alpaca ids of the entries a whitelist mutation touched, for the audit.
fn entry_ids(entries: &[WhitelistEntry]) -> Option<String> {
    (!entries.is_empty()).then(|| {
        entries
            .iter()
            .map(|entry| entry.id.as_str())
            .collect::<Vec<_>>()
            .join(",")
    })
}

async fn withdraw(
    State(state): State<AppState>,
    call: Call,
    Body(request): Body<WithdrawRequest>,
) -> Response {
    let intent = Intent::of(&request)
        .key(&request.operation_id)
        .reason(request.reason.as_deref())
        .note("amount", &request.amount)
        .note("asset", &request.asset)
        .note("destination", &request.address);
    let tier = call.principal.tier;
    let work = state.clone();

    state
        .mutate(call, intent, async move {
            check_withdrawal_destination(tier, &work.config.wallet, &request.address)?;

            // Read the whitelist here so a failed read answers not_applied:
            // the library reads it again inside the withdrawal, where any
            // failure is indistinguishable from a failed send.
            let entries = work
                .wallet
                .get_whitelisted_addresses()
                .await
                .map_err(|error| wallet(&error, Sent::Read))?;
            if !has_approved_entry(&entries, &request.address, &request.asset) {
                return Err(Failure::rejected(
                    RejectionReason::AddressNotWhitelisted,
                    format!(
                        "{} has no approved whitelist entry for {}",
                        request.address, request.asset
                    ),
                ));
            }

            let transfer = work
                .wallet
                .initiate_withdrawal(request.amount, &request.asset, &request.address)
                .await
                .map_err(|error| wallet(&error, Sent::Mutation))?;
            let id = transfer.id;
            Ok(Done::new(Transfer::from(transfer), &id))
        })
        .await
}

async fn transfer(
    State(state): State<AppState>,
    call: Call,
    Params(path): Params<TransferIdPath>,
) -> Response {
    state
        .read(call, async {
            let transfer = state
                .wallet
                .get_transfer(&path.transfer_id.into())
                .await
                .map_err(|error| wallet(&error, Sent::Read))?;
            let reported_fees = match transfer.reported_fees() {
                Ok(fees) => fees,
                Err(ReportedFeesError::NonUsdcAsset { .. }) => None,
                Err(error @ ReportedFeesError::Float(_)) => {
                    return Err(Failure {
                        retryable: false,
                        ..Failure::new(
                            ErrorCode::UpstreamTransient,
                            format!("cannot sum the fees Alpaca reports: {error}"),
                        )
                    });
                }
            };
            Ok(TransferResponse {
                transfer: transfer.transfer.into(),
                reported_fees,
            })
        })
        .await
}

async fn transfers(State(state): State<AppState>, call: Call) -> Response {
    state
        .read(call, async {
            state
                .wallet
                .list_all_transfers()
                .await
                .map(|transfers| TransfersResponse {
                    transfers: transfers.into_iter().map(Transfer::from).collect(),
                })
                .map_err(|error| wallet(&error, Sent::Read))
        })
        .await
}

async fn find_deposit(
    State(state): State<AppState>,
    call: Call,
    Params(path): Params<TxHashPath>,
) -> Response {
    state
        .read(call, async {
            state
                .wallet
                .find_deposit_by_tx_hash(&path.tx_hash)
                .await
                .map(|deposit| DepositResponse {
                    deposit: deposit.map(Transfer::from),
                })
                .map_err(|error| wallet(&error, Sent::Read))
        })
        .await
}

async fn deposit_address(
    State(state): State<AppState>,
    call: Call,
    Query(query): Query<DepositAddressQuery>,
) -> Response {
    state
        .read(call, async {
            check_query_value("asset", query.asset.as_ref())?;
            check_query_value("network", query.network.as_ref())?;
            state
                .wallet
                .get_wallet_address(&query.asset, &query.network)
                .await
                .map(|address| DepositAddressResponse { address })
                .map_err(|error| wallet(&error, Sent::Read))
        })
        .await
}

async fn whitelist(State(state): State<AppState>, call: Call) -> Response {
    state
        .read(call, async {
            state
                .wallet
                .get_whitelisted_addresses()
                .await
                .map(|entries| WhitelistResponse { entries })
                .map_err(|error| wallet(&error, Sent::Read))
        })
        .await
}

async fn whitelist_create(
    State(state): State<AppState>,
    call: Call,
    Body(request): Body<WhitelistCreateRequest>,
) -> Response {
    let intent = Intent::of(&request)
        .key(&request.operation_id)
        .reason(request.reason.as_deref())
        .note("destination", &request.address)
        .note("asset", &request.asset)
        .note("network", &request.network);
    let work = state.clone();

    state
        .mutate(call, intent, async move {
            let travel_rule = travel_rule(&work.config.wallet)?;
            let entry = work
                .wallet
                .create_whitelist_entry(
                    &request.address,
                    &request.asset,
                    &request.network,
                    &travel_rule,
                )
                .await
                .map_err(|error| wallet(&error, Sent::Mutation))?;
            let id = entry.id.clone();
            Ok(Done::new(entry, &id))
        })
        .await
}

async fn whitelist_remove(
    State(state): State<AppState>,
    call: Call,
    Params(path): Params<AddressPath>,
    Body(request): Body<WhitelistRemoveRequest>,
) -> Response {
    let intent = Intent::of(&request)
        .key(&path.address)
        .reason(request.reason.as_deref())
        .note("destination", &path.address);
    let work = state.clone();

    state
        .mutate(call, intent, async move {
            let entries = work
                .wallet
                .remove_whitelist_entries(&path.address)
                .await
                .map_err(|error| wallet(&error, Sent::Mutation))?;
            Ok(Done {
                alpaca_object_id: entry_ids(&entries),
                body: WhitelistResponse { entries },
            })
        })
        .await
}

async fn whitelist_patch_travel_rule(
    State(state): State<AppState>,
    call: Call,
    Body(request): Body<TravelRulePatchRequest>,
) -> Response {
    let intent = Intent::of(&request)
        .key(&request.operation_id)
        .reason(request.reason.as_deref());
    let work = state.clone();

    state
        .mutate(call, intent, async move {
            let travel_rule = travel_rule(&work.config.wallet)?;
            let entries = work
                .wallet
                .patch_all_whitelist_travel_rules(&travel_rule)
                .await
                .map_err(|error| wallet(&error, Sent::Mutation))?;
            Ok(Done {
                alpaca_object_id: entry_ids(&entries),
                body: WhitelistResponse { entries },
            })
        })
        .await
}

async fn journal_create(
    State(state): State<AppState>,
    call: Call,
    Body(request): Body<JournalCreateRequest>,
) -> Response {
    let intent = Intent::of(&request)
        .key(&request.operation_id)
        .reason(request.reason.as_deref())
        .note("counterparty", &request.counterparty)
        .note("symbol", &request.symbol)
        .note("quantity", &request.qty);
    let work = state.clone();

    state
        .mutate(call, intent, async move {
            let destination = counterparty_account(&work.config.journal, &request.counterparty)?;
            let journal = work
                .broker
                .create_journal(destination, &request.symbol, request.qty)
                .await
                .map_err(|error| broker(&error, Sent::Mutation))?;
            let id = journal.id;
            Ok(Done::new(JournalCreateResponse::from(journal), &id))
        })
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn journals_are_switched_off_without_counterparties() {
        let failure = counterparty_account(&JournalConfig::default(), "issuer").unwrap_err();

        assert_eq!(failure.code, ErrorCode::CapabilityDisabled);
    }

    #[test]
    fn whitelist_writes_are_switched_off_without_a_beneficiary() {
        let failure = travel_rule(&WalletConfig::default()).unwrap_err();

        assert_eq!(failure.code, ErrorCode::CapabilityDisabled);
    }

    #[test]
    fn a_deposit_address_query_value_cannot_add_parameters() {
        assert!(check_query_value("asset", "USDC").is_ok());
        assert!(check_query_value("network", "ethereum").is_ok());
        for value in ["", "USDC&account_id=x", "eth#", "a b", "a/b"] {
            let failure = check_query_value("asset", value).unwrap_err();
            assert_eq!(failure.code, ErrorCode::InvalidRequest, "{value}");
        }
    }
}
