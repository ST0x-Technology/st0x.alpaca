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
    AlpacaWalletError, AlpacaWalletService, Network, ReportedFeesError, TravelRuleInfo,
    WhitelistEntry,
};
use st0x_alpaca_gateway_api::dto::wallet::{
    AddressPath, DepositAddressQuery, DepositAddressResponse, DepositResponse,
    JournalCreateRequest, JournalCreateResponse, Transfer, TransferIdPath, TransferLookupResponse,
    TransferResponse, TransfersResponse, TravelRulePatchRequest, TxHashPath,
    WhitelistCreateRequest, WhitelistEntryResponse, WhitelistRemoveRequest, WhitelistResponse,
    WithdrawRequest,
};
use st0x_alpaca_gateway_api::{ErrorCode, Operation, Tier};

use crate::answer::{Failure, Sent, broker, wallet};
use crate::budget::Charge;
use crate::config::{JournalConfig, WalletConfig};
use crate::extract::{Body, Params, Query};
use crate::state::{AppState, Call, Done, Intent};

pub(super) fn route(operation: Operation) -> Option<MethodRouter<AppState>> {
    Some(match operation {
        Operation::WalletWithdraw => post(withdraw),
        Operation::WalletTransfer => get(transfer),
        Operation::WalletTransfers => get(transfers),
        Operation::WalletFindDeposit => get(find_deposit),
        Operation::WalletFindTransfer => get(find_transfer),
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

/// The write a whitelist mutation sends to each entry.
enum EntryWrite<'info> {
    Delete,
    PatchTravelRule(&'info TravelRuleInfo),
}

impl EntryWrite<'_> {
    async fn send(
        &self,
        wallet: &AlpacaWalletService,
        entry: &WhitelistEntry,
    ) -> Result<(), AlpacaWalletError> {
        match self {
            Self::Delete => wallet.delete_whitelist_entry(&entry.id).await,
            Self::PatchTravelRule(info) => {
                wallet.patch_whitelist_travel_rule(&entry.id, info).await
            }
        }
    }
}

/// Takes the human budget for all `writes` of a whitelist loop before the
/// first one is sent, so a loop the budget cannot finish is refused with
/// nothing written. Bots are never charged.
///
/// A loop longer than one minute of budget beside its own read can never
/// be admitted, so its refusal is not retryable and names the limit.
fn reserve_writes(
    state: &AppState,
    tier: Tier,
    operation: Operation,
    writes: usize,
) -> Result<Option<Charge>, Failure> {
    if !tier.is_human() {
        return Ok(None);
    }
    let cost = u32::try_from(writes).unwrap_or(u32::MAX);
    state.budget.take(cost).map(Some).map_err(|wait| {
        let limit = state.config.human_budget_per_minute;
        if cost.saturating_add(operation.human_budget_cost()) > limit {
            Failure {
                retryable: false,
                ..Failure::new(
                    ErrorCode::Backpressure,
                    format!(
                        "{operation} needs {writes} whitelist writes, more than the human budget \
                         of {limit} requests per minute can cover; nothing was written"
                    ),
                )
            }
        } else {
            Failure {
                retry_after: Some(wait),
                ..Failure::new(
                    ErrorCode::Backpressure,
                    format!(
                        "the human request budget left this minute cannot cover {writes} \
                         whitelist writes; nothing was written"
                    ),
                )
            }
        }
    })
}

/// Sends `write` to each of `entries` once the budget for all of them is
/// reserved. Until the first write went through nothing has changed, so its
/// own failure maps as one mutation. Any later failure is `outcome_unknown`
/// naming the entries already changed. Writes never sent give their budget
/// units back.
async fn write_entries(
    state: &AppState,
    tier: Tier,
    operation: Operation,
    entries: &[WhitelistEntry],
    write: &EntryWrite<'_>,
) -> Result<(), Failure> {
    let charge = reserve_writes(state, tier, operation, entries.len())?;
    for (done, entry) in entries.iter().enumerate() {
        let Err(error) = write.send(&state.wallet, entry).await else {
            continue;
        };
        if let Some(charge) = charge {
            state
                .budget
                .settle(charge, u32::try_from(done + 1).unwrap_or(u32::MAX));
        }
        let failure = wallet(&error, Sent::Mutation);
        if done == 0 {
            return Err(failure);
        }

        let written: Vec<String> = entries[..done]
            .iter()
            .map(|written| written.id.clone())
            .collect();
        let message = format!(
            "whitelist entries {} were already changed when entry {} failed: {}",
            written.join(","),
            entry.id,
            failure.message
        );
        return Err(Failure {
            retry_after: failure.retry_after,
            alpaca_status: failure.alpaca_status,
            alpaca_object_ids: written,
            ..Failure::new(ErrorCode::OutcomeUnknown, message)
        });
    }
    Ok(())
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

            work.wallet
                .check_withdrawal_whitelist(&request.asset, &request.address)
                .await
                .map_err(|error| wallet(&error, Sent::Read))?;

            let transfer = work
                .wallet
                .submit_withdrawal(request.amount, &request.asset, &request.address)
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
    let intent = Intent::default().key(&path.transfer_id);
    state
        .read(call, intent, async {
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

/// The account's whole transfer list. A row the library cannot represent
/// fails the read, as the direct `list_all_transfers` does: a reconciliation
/// must never conclude from a list that silently lost rows.
async fn transfers(State(state): State<AppState>, call: Call) -> Response {
    state
        .read(call, Intent::default(), async {
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

/// Looks a transfer up by its tx hash with the library's own scan, so a row
/// that does not parse fails only the lookup of its own hash, exactly as the
/// direct `find_transfer_by_tx_hash` does.
async fn find_transfer(
    State(state): State<AppState>,
    call: Call,
    Params(path): Params<TxHashPath>,
) -> Response {
    let intent = Intent::default().key(&path.tx_hash);
    state
        .read(call, intent, async {
            state
                .wallet
                .find_transfer_by_tx_hash(&path.tx_hash)
                .await
                .map(|transfer| TransferLookupResponse {
                    transfer: transfer.map(Transfer::from),
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
    let intent = Intent::default().key(&path.tx_hash);
    state
        .read(call, intent, async {
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
    let intent = Intent::default()
        .key(&query.asset)
        .note("network", &query.network);
    state
        .read(call, intent, async {
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
        .read(call, Intent::default(), async {
            state
                .wallet
                .get_whitelisted_addresses()
                .await
                .map(|entries| WhitelistResponse {
                    entries: entries.into_iter().map(Into::into).collect(),
                })
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
        .note("asset", &request.asset);
    let work = state.clone();

    state
        .mutate(call, intent, async move {
            let travel_rule = travel_rule(&work.config.wallet)?;
            let entry = work
                .wallet
                .create_whitelist_entry(
                    &request.address,
                    &request.asset,
                    &Network::new("ethereum"),
                    &travel_rule,
                )
                .await
                .map_err(|error| wallet(&error, Sent::Mutation))?;
            let id = entry.id.clone();
            Ok(Done::new(WhitelistEntryResponse::from(entry), &id))
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
    let tier = call.principal.tier;
    let operation = call.operation;
    let work = state.clone();

    state
        .mutate(call, intent, async move {
            let entries: Vec<WhitelistEntry> = work
                .wallet
                .get_whitelisted_addresses()
                .await
                .map_err(|error| wallet(&error, Sent::Read))?
                .into_iter()
                .filter(|entry| entry.address == path.address)
                .collect();
            if entries.is_empty() {
                let error = AlpacaWalletError::NoWhitelistEntries {
                    address: path.address,
                };
                return Err(wallet(&error, Sent::Read));
            }

            write_entries(&work, tier, operation, &entries, &EntryWrite::Delete).await?;
            Ok(Done {
                alpaca_object_id: entry_ids(&entries),
                body: WhitelistResponse {
                    entries: entries.into_iter().map(Into::into).collect(),
                },
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
    let tier = call.principal.tier;
    let operation = call.operation;
    let work = state.clone();

    state
        .mutate(call, intent, async move {
            let travel_rule = travel_rule(&work.config.wallet)?;
            let entries = work
                .wallet
                .get_whitelisted_addresses()
                .await
                .map_err(|error| wallet(&error, Sent::Read))?;

            write_entries(
                &work,
                tier,
                operation,
                &entries,
                &EntryWrite::PatchTravelRule(&travel_rule),
            )
            .await?;
            Ok(Done {
                alpaca_object_id: entry_ids(&entries),
                body: WhitelistResponse {
                    entries: entries.into_iter().map(Into::into).collect(),
                },
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
