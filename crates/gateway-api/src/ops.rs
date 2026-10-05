//! The operation catalog: every route the gateway serves, and which tiers may
//! call it in each profile. There is no other way to reach Alpaca through the
//! gateway.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::access::{Profile, Tier};

/// HTTP method of an operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
}

/// One gateway operation. Each one runs one bounded `st0x-alpaca` method.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Operation {
    #[serde(rename = "account.funds")]
    AccountFunds,
    #[serde(rename = "account.withdrawable_cash")]
    AccountWithdrawableCash,
    #[serde(rename = "account.inventory")]
    AccountInventory,
    #[serde(rename = "account.position_mark")]
    AccountPositionMark,
    #[serde(rename = "activities.list")]
    ActivitiesList,
    #[serde(rename = "market.is_open")]
    MarketIsOpen,
    #[serde(rename = "market.session")]
    MarketSession,
    #[serde(rename = "market.session_status")]
    MarketSessionStatus,
    #[serde(rename = "market.latest_trade")]
    MarketLatestTrade,
    #[serde(rename = "market.latest_quote")]
    MarketLatestQuote,
    #[serde(rename = "market.latest_overnight_quote")]
    MarketLatestOvernightQuote,
    #[serde(rename = "assets.get")]
    AssetsGet,
    #[serde(rename = "assets.counter_trade_shares")]
    AssetsCounterTradeShares,
    #[serde(rename = "orders.place_market")]
    OrdersPlaceMarket,
    #[serde(rename = "orders.place_limit")]
    OrdersPlaceLimit,
    #[serde(rename = "orders.place_exact_limit")]
    OrdersPlaceExactLimit,
    #[serde(rename = "orders.get")]
    OrdersGet,
    #[serde(rename = "orders.find")]
    OrdersFind,
    #[serde(rename = "orders.recover")]
    OrdersRecover,
    #[serde(rename = "orders.cancel")]
    OrdersCancel,
    #[serde(rename = "conversions.submit")]
    ConversionsSubmit,
    #[serde(rename = "conversions.get")]
    ConversionsGet,
    #[serde(rename = "conversions.find")]
    ConversionsFind,
    #[serde(rename = "journals.create")]
    JournalsCreate,
    #[serde(rename = "wallet.withdraw")]
    WalletWithdraw,
    #[serde(rename = "wallet.transfer")]
    WalletTransfer,
    #[serde(rename = "wallet.transfers")]
    WalletTransfers,
    #[serde(rename = "wallet.find_deposit")]
    WalletFindDeposit,
    #[serde(rename = "wallet.deposit_address")]
    WalletDepositAddress,
    #[serde(rename = "wallet.whitelist")]
    WalletWhitelist,
    #[serde(rename = "wallet.whitelist_create")]
    WalletWhitelistCreate,
    #[serde(rename = "wallet.whitelist_remove")]
    WalletWhitelistRemove,
    #[serde(rename = "wallet.whitelist_patch_travel_rule")]
    WalletWhitelistPatchTravelRule,
    #[serde(rename = "tokenization.mint")]
    TokenizationMint,
    #[serde(rename = "tokenization.requests")]
    TokenizationRequests,
    #[serde(rename = "tokenization.request")]
    TokenizationRequest,
    #[serde(rename = "tokenization.find_mint")]
    TokenizationFindMint,
    #[serde(rename = "tokenization.find_redemption")]
    TokenizationFindRedemption,
}

const BOT_READ_WRITE: &[Tier] = &[Tier::Bot, Tier::Read, Tier::Write];
const READ_WRITE: &[Tier] = &[Tier::Read, Tier::Write];
const BOT_WRITE: &[Tier] = &[Tier::Bot, Tier::Write];
const BOT: &[Tier] = &[Tier::Bot];
const WRITE: &[Tier] = &[Tier::Write];

const SINGLE_CALL: Duration = Duration::from_secs(45);
const ORDER_PLACEMENT: Duration = Duration::from_secs(125);
const MULTI_CALL: Duration = Duration::from_secs(90);
const ACTIVITY_PAGES: Duration = Duration::from_secs(120);
const WITHDRAWAL_ANSWER: Duration = Duration::from_secs(60);

impl Operation {
    pub const ALL: [Self; 38] = [
        Self::AccountFunds,
        Self::AccountWithdrawableCash,
        Self::AccountInventory,
        Self::AccountPositionMark,
        Self::ActivitiesList,
        Self::MarketIsOpen,
        Self::MarketSession,
        Self::MarketSessionStatus,
        Self::MarketLatestTrade,
        Self::MarketLatestQuote,
        Self::MarketLatestOvernightQuote,
        Self::AssetsGet,
        Self::AssetsCounterTradeShares,
        Self::OrdersPlaceMarket,
        Self::OrdersPlaceLimit,
        Self::OrdersPlaceExactLimit,
        Self::OrdersGet,
        Self::OrdersFind,
        Self::OrdersRecover,
        Self::OrdersCancel,
        Self::ConversionsSubmit,
        Self::ConversionsGet,
        Self::ConversionsFind,
        Self::JournalsCreate,
        Self::WalletWithdraw,
        Self::WalletTransfer,
        Self::WalletTransfers,
        Self::WalletFindDeposit,
        Self::WalletDepositAddress,
        Self::WalletWhitelist,
        Self::WalletWhitelistCreate,
        Self::WalletWhitelistRemove,
        Self::WalletWhitelistPatchTravelRule,
        Self::TokenizationMint,
        Self::TokenizationRequests,
        Self::TokenizationRequest,
        Self::TokenizationFindMint,
        Self::TokenizationFindRedemption,
    ];

    /// Catalog name, as written in audit records and config.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::AccountFunds => "account.funds",
            Self::AccountWithdrawableCash => "account.withdrawable_cash",
            Self::AccountInventory => "account.inventory",
            Self::AccountPositionMark => "account.position_mark",
            Self::ActivitiesList => "activities.list",
            Self::MarketIsOpen => "market.is_open",
            Self::MarketSession => "market.session",
            Self::MarketSessionStatus => "market.session_status",
            Self::MarketLatestTrade => "market.latest_trade",
            Self::MarketLatestQuote => "market.latest_quote",
            Self::MarketLatestOvernightQuote => "market.latest_overnight_quote",
            Self::AssetsGet => "assets.get",
            Self::AssetsCounterTradeShares => "assets.counter_trade_shares",
            Self::OrdersPlaceMarket => "orders.place_market",
            Self::OrdersPlaceLimit => "orders.place_limit",
            Self::OrdersPlaceExactLimit => "orders.place_exact_limit",
            Self::OrdersGet => "orders.get",
            Self::OrdersFind => "orders.find",
            Self::OrdersRecover => "orders.recover",
            Self::OrdersCancel => "orders.cancel",
            Self::ConversionsSubmit => "conversions.submit",
            Self::ConversionsGet => "conversions.get",
            Self::ConversionsFind => "conversions.find",
            Self::JournalsCreate => "journals.create",
            Self::WalletWithdraw => "wallet.withdraw",
            Self::WalletTransfer => "wallet.transfer",
            Self::WalletTransfers => "wallet.transfers",
            Self::WalletFindDeposit => "wallet.find_deposit",
            Self::WalletDepositAddress => "wallet.deposit_address",
            Self::WalletWhitelist => "wallet.whitelist",
            Self::WalletWhitelistCreate => "wallet.whitelist_create",
            Self::WalletWhitelistRemove => "wallet.whitelist_remove",
            Self::WalletWhitelistPatchTravelRule => "wallet.whitelist_patch_travel_rule",
            Self::TokenizationMint => "tokenization.mint",
            Self::TokenizationRequests => "tokenization.requests",
            Self::TokenizationRequest => "tokenization.request",
            Self::TokenizationFindMint => "tokenization.find_mint",
            Self::TokenizationFindRedemption => "tokenization.find_redemption",
        }
    }

    /// Path below the tier prefix, in axum's `{param}` syntax.
    #[must_use]
    pub const fn path(self) -> &'static str {
        match self {
            Self::AccountFunds => "/account/funds",
            Self::AccountWithdrawableCash => "/account/withdrawable-cash",
            Self::AccountInventory => "/account/inventory",
            Self::AccountPositionMark => "/account/positions/{symbol}/mark",
            Self::ActivitiesList => "/activities",
            Self::MarketIsOpen => "/market/open",
            Self::MarketSession => "/market/session",
            Self::MarketSessionStatus => "/market/session-status",
            Self::MarketLatestTrade => "/market/stocks/{symbol}/latest-trade",
            Self::MarketLatestQuote => "/market/stocks/{symbol}/latest-quote",
            Self::MarketLatestOvernightQuote => "/market/stocks/{symbol}/latest-overnight-quote",
            Self::AssetsGet => "/assets/{symbol}",
            Self::AssetsCounterTradeShares => "/assets/{symbol}/counter-trade-shares",
            Self::OrdersPlaceMarket => "/orders/market",
            Self::OrdersPlaceLimit => "/orders/limit",
            Self::OrdersPlaceExactLimit => "/orders/exact-limit",
            Self::OrdersGet => "/orders/{order_id}",
            Self::OrdersFind => "/orders/by-client-order-id/{client_order_id}",
            Self::OrdersRecover => "/orders/recover",
            Self::OrdersCancel => "/orders/{order_id}/cancel",
            Self::ConversionsSubmit => "/conversions",
            Self::ConversionsGet => "/conversions/{order_id}",
            Self::ConversionsFind => "/conversions/by-client-order-id/{client_order_id}",
            Self::JournalsCreate => "/journals",
            Self::WalletWithdraw => "/wallet/withdrawals",
            Self::WalletTransfer => "/wallet/transfers/{transfer_id}",
            Self::WalletTransfers => "/wallet/transfers",
            Self::WalletFindDeposit => "/wallet/deposits/by-tx/{tx_hash}",
            Self::WalletDepositAddress => "/wallet/deposit-address",
            Self::WalletWhitelist => "/wallet/whitelist",
            Self::WalletWhitelistCreate => "/wallet/whitelist/entries",
            Self::WalletWhitelistRemove => "/wallet/whitelist/{address}/remove",
            Self::WalletWhitelistPatchTravelRule => "/wallet/whitelist/travel-rule",
            Self::TokenizationMint => "/tokenization/mints",
            Self::TokenizationRequests => "/tokenization/requests",
            Self::TokenizationRequest => "/tokenization/requests/{tokenization_request_id}",
            Self::TokenizationFindMint => {
                "/tokenization/mints/by-issuer-request-id/{issuer_request_id}"
            }
            Self::TokenizationFindRedemption => "/tokenization/redemptions/by-tx/{tx_hash}",
        }
    }

    #[must_use]
    pub const fn method(self) -> Method {
        if self.uses_body() {
            Method::Post
        } else {
            Method::Get
        }
    }

    /// Whether the operation carries a JSON body (and is a POST).
    const fn uses_body(self) -> bool {
        self.mutates() || matches!(self, Self::OrdersRecover | Self::AssetsCounterTradeShares)
    }

    /// Whether the operation can change state at Alpaca.
    #[must_use]
    pub const fn mutates(self) -> bool {
        matches!(
            self,
            Self::OrdersPlaceMarket
                | Self::OrdersPlaceLimit
                | Self::OrdersPlaceExactLimit
                | Self::OrdersCancel
                | Self::ConversionsSubmit
                | Self::JournalsCreate
                | Self::WalletWithdraw
                | Self::WalletWhitelistCreate
                | Self::WalletWhitelistRemove
                | Self::WalletWhitelistPatchTravelRule
                | Self::TokenizationMint
        )
    }

    /// Whether a mutation may be sent again with the same key after
    /// `outcome_unknown`. Alpaca dedupes these by key, or they are idempotent
    /// by nature. Never true for a read.
    #[must_use]
    pub const fn resendable_with_same_key(self) -> bool {
        matches!(
            self,
            Self::OrdersPlaceMarket
                | Self::OrdersPlaceLimit
                | Self::OrdersPlaceExactLimit
                | Self::OrdersCancel
                | Self::WalletWhitelistRemove
                | Self::WalletWhitelistPatchTravelRule
                | Self::TokenizationMint
        )
    }

    /// How long the gateway may take before it answers. A mutation already
    /// sent to Alpaca keeps running past this; the answer is then
    /// `outcome_unknown`.
    #[must_use]
    pub const fn deadline(self) -> Duration {
        match self {
            Self::OrdersPlaceMarket | Self::OrdersPlaceLimit | Self::OrdersPlaceExactLimit => {
                ORDER_PLACEMENT
            }
            Self::OrdersRecover
            | Self::WalletWhitelistRemove
            | Self::WalletWhitelistPatchTravelRule => MULTI_CALL,
            Self::ActivitiesList => ACTIVITY_PAGES,
            Self::WalletWithdraw => WITHDRAWAL_ANSWER,
            _ => SINGLE_CALL,
        }
    }

    /// Units this operation takes from the shared human budget. Keyed single
    /// reads that client loops depend on cost nothing: a `429` there would
    /// end a crate poll loop before its deadline cancel.
    #[must_use]
    pub const fn human_budget_cost(self) -> u32 {
        match self {
            Self::OrdersGet
            | Self::ConversionsGet
            | Self::WalletTransfer
            | Self::TokenizationRequest
            | Self::TokenizationFindRedemption => 0,
            Self::ActivitiesList => 10,
            _ => 1,
        }
    }

    /// Tiers allowed to call this operation in `profile`. Empty when the
    /// profile does not serve it.
    #[must_use]
    pub const fn tiers(self, profile: Profile) -> &'static [Tier] {
        match profile {
            Profile::T0 => self.t0_tiers(),
        }
    }

    const fn t0_tiers(self) -> &'static [Tier] {
        match self {
            Self::AccountFunds
            | Self::AccountWithdrawableCash
            | Self::AccountInventory
            | Self::AccountPositionMark
            | Self::ActivitiesList
            | Self::MarketIsOpen
            | Self::MarketSession
            | Self::MarketSessionStatus
            | Self::MarketLatestTrade
            | Self::MarketLatestQuote
            | Self::AssetsGet
            | Self::OrdersGet
            | Self::OrdersFind
            | Self::ConversionsGet
            | Self::ConversionsFind
            | Self::WalletTransfer
            | Self::WalletTransfers
            | Self::WalletFindDeposit
            | Self::WalletDepositAddress
            | Self::TokenizationRequests
            | Self::TokenizationRequest
            | Self::TokenizationFindMint
            | Self::TokenizationFindRedemption => BOT_READ_WRITE,
            Self::MarketLatestOvernightQuote | Self::WalletWhitelist => READ_WRITE,
            Self::OrdersPlaceMarket
            | Self::OrdersCancel
            | Self::ConversionsSubmit
            | Self::WalletWithdraw
            | Self::TokenizationMint => BOT_WRITE,
            Self::AssetsCounterTradeShares | Self::OrdersPlaceLimit | Self::OrdersRecover => BOT,
            Self::OrdersPlaceExactLimit
            | Self::JournalsCreate
            | Self::WalletWhitelistCreate
            | Self::WalletWhitelistRemove
            | Self::WalletWhitelistPatchTravelRule => WRITE,
        }
    }

    /// Whether `tier` may call this operation in `profile`.
    #[must_use]
    pub fn allows(self, profile: Profile, tier: Tier) -> bool {
        self.tiers(profile).contains(&tier)
    }

    /// Looks an operation up by its catalog name.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|operation| operation.name() == name)
    }
}

impl std::fmt::Display for Operation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.name())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn every_operation_has_a_unique_name_and_route() {
        let names: HashSet<_> = Operation::ALL.iter().map(|op| op.name()).collect();
        assert_eq!(names.len(), Operation::ALL.len());

        let routes: HashSet<_> = Operation::ALL
            .iter()
            .map(|op| (op.path(), op.method() == Method::Post))
            .collect();
        assert_eq!(routes.len(), Operation::ALL.len());
    }

    #[test]
    fn serde_name_matches_catalog_name() {
        for operation in Operation::ALL {
            let json = serde_json::to_value(operation).unwrap();
            assert_eq!(json, serde_json::Value::from(operation.name()));
            assert_eq!(Operation::from_name(operation.name()), Some(operation));
        }
    }

    #[test]
    fn no_path_takes_an_account() {
        for operation in Operation::ALL {
            assert!(
                !operation.path().contains("account_id"),
                "{operation} takes an account id"
            );
        }
    }

    #[test]
    fn mutations_are_posts_and_never_offered_to_readers() {
        for operation in Operation::ALL.into_iter().filter(|op| op.mutates()) {
            assert_eq!(operation.method(), Method::Post, "{operation}");
            assert!(!operation.allows(Profile::T0, Tier::Read), "{operation}");
        }
    }

    #[test]
    fn every_t0_operation_is_served_on_some_tier() {
        for operation in Operation::ALL {
            assert_ne!(operation.tiers(Profile::T0), &[] as &[Tier], "{operation}");
        }
    }

    #[test]
    fn manual_only_operations_are_closed_to_the_bot() {
        for operation in [
            Operation::OrdersPlaceExactLimit,
            Operation::JournalsCreate,
            Operation::WalletWhitelistCreate,
            Operation::WalletWhitelistRemove,
            Operation::WalletWhitelistPatchTravelRule,
        ] {
            assert!(!operation.allows(Profile::T0, Tier::Bot), "{operation}");
        }
    }
}
