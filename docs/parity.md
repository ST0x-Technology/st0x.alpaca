# Alpaca parity matrix

This file maps every Alpaca integration item that st0x.liquidity and
st0x.issuance use to its st0x.alpaca counterpart. The bar is 1:1: the same
endpoints, wire encodings, auth modes, retry and polling rules, error
mapping, normalization, redaction, and regression tests. Each difference is
listed with its reason.

Sources:

- st0x.liquidity `5a9895b8` (`crates/execution`, `crates/tokenization`,
  `crates/dto` `Direction`).
- st0x.issuance `b3b955f` (`src/alpaca/{mod,itn,service,mock}.rs` and the
  corporate-action stream in `src/tokenized_asset/corporate_action_feed.rs`).

Status values:

- **Same**: ported with the same behavior and tests. Only import paths,
  lint-driven doc sections, `#[must_use]` attributes, single-letter variable
  renames, and ticket references in comments changed.
- **Moved**: the same behavior, exposed through a different public item.
- **Changed**: an intentional difference. The row gives the reason.
- **Stays in consumer**: consumer orchestration that is not Alpaca API
  behavior. It is not ported.

## Summary

| Surface | Distinct endpoints (method + path) | Ported | Intentional differences |
| --- | ---: | ---: | --- |
| Issuer (st0x.issuance) | 3 | 3 | `Fees` and response qty on Rain Float; 429 as `RateLimited`; typed `InvalidUrl`; percent-encoded path segments; no log events. |
| Corporate-action stream (st0x.issuance) | 1 | 1 | Errors split into endpoint and stream errors; JWT modes send a bearer token; no log events. Projection, cursor, and reconnect loop stay in issuance. |
| Broker and Market Data (st0x.liquidity) | 13 | 13 | Preflight policy fields and errors stay in the consumer; trait mappings became inherent methods; redirects disabled and mode URLs validated. |
| Wallet (st0x.liquidity) | 8 | 8 | Redirects disabled and base URL validated. |
| Tokenization (st0x.liquidity) | 2 | 2 | Non-generic service; onchain actions and their error variants stay in the consumer. |
| Mocks (st0x.liquidity e2e) | 2 servers | 2 | Inline ERC-20 interface instead of ABI environment variables. |
| Shared auth and transport | - | - | Public auth internals made private; request paths confined to the configured origin; token URLs validated. |

Tests: 647 pass with `--all-features` (plus 1 live-sandbox test ignored, as
in the source). Every source test is ported except the consumer-side tests
listed under Test parity.

## Shared transport and auth (`core`, `auth`, `endpoint`, `rate_limit`)

| Source item | st0x.alpaca item | Status |
| --- | --- | --- |
| liquidity `AlpacaBrokerAuth` (Basic, KmsJwt, PrivateKeyJwt; untagged, field-shaped) | `core::AlpacaAuth` | Same. Debug redacts with `[REDACTED]` (the issuer copy used `<redacted>`). |
| liquidity `kms_jwt.rs` (RFC 7523 assertions, KMS `asymmetricSign`, metadata token, local PEM, token cache with soft/hard deadlines, failed-mint backoff, stale-token fallback) | `auth` | Same, including the two log events (`warn` on riding a cached token, `info` on each mint). |
| `ALPACA_TOKEN_URL`, `ALPACA_SANDBOX_TOKEN_URL` | same names | Same. |
| `AuthRuntime` (pub), `KmsJwtAuth::new` / `with_urls` (pub, arbitrary token/KMS/metadata URLs and HTTP client) | crate-private `AuthRuntime`; private `KmsJwtAuth::with_urls` | Changed: every credential-bearing construction goes through `AuthRuntime::build`, which validates the token URL and uses a no-redirect client. No consumer uses these items outside the Alpaca crates. |
| `AuthRuntime::build(auth, token_url)` | same (crate-private) | Changed: JWT token URLs must be HTTPS (HTTP only on loopback) without embedded credentials, query, or fragment. New `KmsJwtError::InvalidTokenUrl`, classified deterministic. |
| `apply_apca` / `apply_wallet` / `broker_authorization`, sensitive header values | same (crate-private) | Same. |
| `KmsJwtError` and `is_deterministic` / `is_rate_limited` / `retry_after` | same | Same, plus `InvalidTokenUrl`. Changed: a 3xx from KMS or the token endpoint is deterministic, because the mint client does not follow redirects. |
| `rate_limit::parse_retry_after`, `retry_after_from_response_headers` | same | Same (the liquidity file and its 10 tests replace the shorter issuer copy). |
| `Backpressure`, `Permanence` | `core::{Backpressure, Permanence}` | Same. |
| liquidity `status_permanence(StatusCode)` | `core::response_status_permanence` | Changed: a 3xx is now Permanent. The source clients followed redirects, so a 3xx never surfaced; with redirects disabled the same request would be redirected again. Otherwise the same policy and test. The issuer keeps its existing `u16` classifier (3xx is already Permanent there). |
| issuer `AlpacaClient` public `get`/`post`/`delete`/`patch`/`market_data_get` taking any HTTPS URL; ids interpolated into the path with `format!` | crate-private `get`/`post` taking path segments | Changed: the URL is built on the configured base URL from percent-encoded segments, so credentials cannot reach another host and an id containing `/`, `?`, or `#` cannot address another endpoint. Empty and dot segments are rejected (`EndpointError::InvalidPathSegment`). `delete`, `patch`, and `market_data_get` had no issuer callers and are removed. |
| issuer `AlpacaError::InvalidUrl(String)` | `AlpacaError::InvalidUrl(EndpointError)` | Changed: typed error instead of an opaque string. |
| Broker HTTP client (reqwest default: follows up to 10 redirects; mode URLs unvalidated) | `broker::client` | Changed: redirects disabled; the mode's broker and market-data URLs are validated (`AlpacaBrokerApiError::InvalidEndpoint`). Sandbox and production URLs are unchanged. |
| Wallet HTTP client (`reqwest::Client::new()`: follows redirects; base URL unvalidated) | `wallet::client` | Changed: redirects disabled; base URL validated (`AlpacaWalletError::InvalidBaseUrl`). Still no connect or request timeout, as in the source: a timeout on the non-idempotent withdrawal POST would turn a hang into an ambiguous failure the consumer could retry into a second withdrawal. Adding one needs a consumer-side recovery path first. |
| liquidity `rate_limit::retry_after_from_response_headers` re-exported from the crate root | crate-private | Changed: only the Alpaca clients used it, and they now live here. |
| (none) | root `pub use st0x_finance` | New: every public amount, quantity, and symbol type is from `st0x-finance` `v0.3.0`; consumers must use that release (liquidity uses its in-repo copy today, see the `NotPositive` row below). |

## Issuer surface (`issuer`, st0x.issuance)

| Source item (issuance) | st0x.alpaca item | Status |
| --- | --- | --- |
| `POST /v1/accounts/{account_id}/tokenization/callback/mint` (`send_mint_callback`) | `IssuerApi::send_mint_callback` | Same, under the shared retry policy. A 429 is `RateLimited` with its `Retry-After` hint; issuance returned `Api { 429 }`. Both retry. |
| `POST /v1/accounts/{account_id}/tokenization/callback/redeem` (`call_redeem_endpoint`) | `IssuerApi::call_redeem_endpoint` | Same, with the retry inside the method. |
| ITN preflight `itn::accepts_network_wire_string` before the redeem call; `UnsupportedTokenizationNetwork { network, reference }` | `issuer::itn::{TOKENIZATION_NETWORK_WIRE_STRINGS, REDEEM_CALLBACK_OPENAPI_REFERENCE, accepts_network_wire_string}`; `AlpacaError::UnsupportedTokenizationNetwork` | Same. The check runs before the retry and before any HTTP call, and the error is not retryable. As in issuance, every `Network` value is on the list, so the check cannot fail today; it guards a future variant. |
| `GET /v1/accounts/{account_id}/tokenization/requests/{id}` (`poll_request_status`), 404 as `RequestNotFound`, id mismatch as `ResponseIdMismatch` | `IssuerApi::poll_request_status` | Same. |
| Request qty from `Decimal` keeping lexical scale (`100.50`) | `RedeemQty` (validated through `FractionalShares`, serializes the caller's exact string) | Moved: wire spelling is preserved without `rust_decimal`. |
| Response qty `Decimal` | `Qty(FractionalShares)` | Changed: Rain Float for arithmetic. Response quantities are not sent back. |
| `Fees(Decimal)` | `Fees(Usd)` | Changed: Rain Float-backed `st0x_finance::Usd`. Every production encoding (`"0"`, `"0.0"`, `"0.01"`, `"0.5"`, `"0.001"`), a JSON number, absent, and `null` are covered by tests. `rust_decimal` is no longer a dependency. |
| `tx_hash` absent / null / empty on a pending redeem poll | `deserialize_optional_b256` with `serde(default)` | Same. |
| `issuer_request_id` typed enum | `IssuerRequestId(String)` | Changed: kept as the wire string; issuance parses its own id format. |
| Network (issuance dto enum) | `core::Network` (closed enum, same wire names) | Moved. |
| `MockAlpacaService` (`new_success`, `new_failure`, `get_call_count`) | `issuer::mock::MockIssuerApi` (behind `test-support`) | Moved: the redeem echo returns the request quantity as `Qty(FractionalShares)` (numeric), following the response type change above. The mock now needs the `test-support` feature, like the other test doubles. |
| Issuance ITN list comment (`robinhood` as the only unpublished entry) and `UnsupportedTokenizationNetwork` message ("not a published ... value") | same list | Changed wording only: `hyperevm` is not in the published enum either, so the message now says the network is not on the ITN list. |
| Issuance log events (`Calling Alpaca redeem endpoint`, etc.) | none | Changed: the issuer surface stays telemetry-free, as reviewed earlier. Consumers log around the calls. |

## Corporate-action stream (`corporate-actions`, st0x.issuance)

Source: st0x.issuance `b3b955f` `src/tokenized_asset/corporate_action_feed.rs`,
`src/tokenized_asset/mod.rs` (id types), and `src/alpaca/service.rs` (stream
URL, bootstrap instant). `src/tokenized_asset/corporate_actions.rs` is not
compiled at that commit (the module is not declared) and is not ported.

| Source item | st0x.alpaca item | Status |
| --- | --- | --- |
| `validate_corporate_action_endpoint(endpoint, Environment)` -> `CorporateActionStreamTransport` | `CorporateActionStreamEndpoint::parse(endpoint, DevelopmentLoopback)`, `.transport()` | Moved: `Environment::Development` becomes `DevelopmentLoopback::Allow`. Credentials go only to `stream.data.alpaca.markets` over HTTPS; a plain-HTTP loopback IP is credential-free and allowed in development only. Changed: URL userinfo (which reqwest would send as an `Authorization` header) and fragments are rejected (`EmbeddedCredentials`, `Fragment`), and `until_id` is reserved with `since`, `since_id`, and `until`. |
| `CorporateActionStreamTransport {AuthenticatedAlpaca, CredentialFreeDevelopment}` | same | Same. |
| `CorporateActionFeedBuildError {InvalidEndpoint, InsecureEndpointScheme, UnexpectedEndpointHost, ReservedReplayQueryParameter, Client}` | `CorporateActionEndpointError` (first four) and `CorporateActionStreamBuildError {Client, Auth}` | Changed: split into endpoint and client errors with the same messages. `Auth` is new (credential or token-URL failure). |
| Struct-literal `AuthenticatedAlpaca` endpoint on a loopback URL (issuance tests) | `CorporateActionStreamEndpoint::authenticated_loopback` (`test-support`) | Moved: consumer tests get an authenticated loopback endpoint without bypassing validation. |
| `DEFAULT_CORPORATE_ACTIONS_STREAM_URL` | same | Same. |
| Stream client: connect timeout, read timeout, no redirects | `CorporateActionStreamClient::new(.., connect_timeout, read_timeout)` | Same. The timeouts are parameters; issuance keeps its 10 s and 90 s defaults in its config. |
| Raw `APCA-API-KEY-ID` / `APCA-API-SECRET-KEY` headers | `AuthRuntime::apply_apca` through `AlpacaAuth` | Same headers for Basic, now marked sensitive, with no `Authorization`. Changed: the KMS and private-key JWT modes send the bearer token (issuance had Basic only). Credential-free development never builds credentials. |
| Replay query: `since_id`, `since` + `until`, `since`, or none | `CorporateActionReplay {SinceId, Window, Since, Live}` | Moved: same parameters and order. The rule that a live `since` applies only to the authenticated transport stays in the consumer. |
| Status and content-type checks; `CorporateActionFeedError::{Http, HttpStatus, InvalidContentType}` | `CorporateActionStreamClient::connect`; `CorporateActionStreamError {Http, HttpStatus, InvalidContentType, Auth}` | Same messages; `Auth` is new. |
| `response.bytes_stream()` feeding `CorporateActionSseDecoder::push` | `CorporateActionStream::next_batch`, `has_pending_frame` | Changed shape: `Response::chunk()` instead of `bytes_stream()` (no `futures` dependency); the same chunks, errors, and decoding. |
| `CorporateActionSseDecoder` (64 KiB frame cap, 4-byte separator allowance, CR/LF/CRLF, poison releases the buffer) | same | Same, with one fix: when a chunk ends between the CR and LF of a frame's final CRLF, the source left the LF in the buffer, so `has_pending_frame` reported a partial frame at a clean end of stream. The decoder now consumes that LF (`crlf_separator_split_across_chunks_leaves_no_pending_byte`). `has_pending_frame` is public. |
| `CorporateActionDecodeBatch`, `CorporateActionStreamDecodeError` (with `event_id`), `CorporateActionDecodeError` | same | Same variants and messages. `event_id()` is public (issuance reached it through `CorporateActionFeedError::event_id`). |
| `decode_sse_frame`, envelope and payload types, SSE line/field/frame helpers | private in `corporate_actions::sse` | Same. |
| `CorporateActionMutationKind`, `CorporateActionMutation`, `DividendCorporateAction` | same | Changed: `underlying` is `CorporateActionSymbol` instead of issuance's `UnderlyingSymbol` (the same trim and non-empty rule). |
| `CorporateActionEventId` (canonical ULID), `CorporateActionId` (1 to 128 bytes), validating `Deserialize` | same | Moved: the stream's wire identities are owned here; `Hash` is added. |
| `CorporateActionBootstrapSince` (with its error, `FromStr`, `try_from_instant`, `query_value`), `CorporateActionReplayUntil` | same | Same. |
| `info!` "Connected to Alpaca corporate-action stream" | none | Changed: this surface is telemetry-free like the issuer surface; the consumer logs after `connect` returns. |
| `CorporateActionFeed` and its run loop, baselines, replay-anchor check, reconnect backoff and alerts, notifications, projection, cursor, blocked boundary, reconciliation, holds, admission guard, spawn and shutdown | none | Stays in consumer. |

## Broker API surface (`broker`, st0x.liquidity)

### Endpoints

| Endpoint | Source | st0x.alpaca | Status |
| --- | --- | --- | --- |
| `GET /v1/trading/accounts/{id}/account` (verify, `ACTIVE` check) | `client.verify_account`, `Executor::try_from_ctx` | `AlpacaBrokerApi::try_from_ctx` | Same. |
| `GET /v1/trading/accounts/{id}/account` (cash, buying power, withdrawable in cents) | `positions::get_account_funds` | `AlpacaBrokerApi::{account_funds, withdrawable_cash_cents}` | Same; `account_funds` is newly public so the consumer preflight can read it. |
| `GET /v1/assets/{symbol}` (status, tradable, fractionable, attributes) with TTL cache | `client.get_asset`, `get_asset_cached`, `validate_asset` | `AlpacaBrokerApi::get_asset_details` and every placement path | Same. |
| `POST /v1/trading/accounts/{id}/orders` market (`qty` string, configured TIF `day`/`cls`, `extended_hours: false`, `client_order_id`) | `order::place_market_order` | `AlpacaBrokerApi::place_market_order` | Same, including 422 duplicate `client_order_id` adoption and placement-timestamp fallback through `GET .../orders/{id}`. |
| `POST .../orders` limit (`limit_price` precision, `extended_hours`, `day`) | `order::place_limit_order` | `AlpacaBrokerApi::{place_limit_order, place_alpaca_limit_order}` | Same. |
| `GET .../orders/{id}` and status mapping (16 statuses, terminality, timestamp fallbacks, completeness checks) | `order::get_order_status`, `Executor::get_order_status` | `AlpacaBrokerApi::get_order_status` -> `OrderState` | Moved: the trait-impl mapping (IncompleteOrder, FilledQuantityMismatch) is now the inherent method. |
| `GET .../orders:by_client_order_id?client_order_id=` (404 as `None`) | `client.get_order_by_client_order_id`, `recover_order_by_client_id` | `AlpacaBrokerApi::{get_order_by_client_order_id, recover_order_by_client_id}` | Same. |
| `DELETE .../orders/{id}` (204 `Requested`, 404 `OrderNotFound`, 422 error) | `client.cancel_order` | `AlpacaBrokerApi::cancel_order` | Same. |
| `POST .../orders` crypto `USDCUSD`: sell by `qty`, buy by whole-cent `notional`, `gtc`; 403 `40310000` as `UsdConversionInsufficientBalance` | `order::convert_usdc_usd` | `AlpacaBrokerApi::convert_usdc_usd` | Same. |
| Conversion polling: 300 s deadline, cancel, 30 s settle window, 500 ms reads, cancel/fill race, partial fills, `DoneForDay` waited on | `poll_crypto_order_until_filled`, `poll_crypto_order_to_terminal`, `cancel_and_settle` | `AlpacaBrokerApi::{convert_usdc_usd, poll_conversion_to_terminal}` | Same. |
| `GET .../orders:by_client_order_id` for crypto | `get_crypto_order_by_client_order_id` | `AlpacaBrokerApi::find_conversion_order` | Same. |
| `GET /v1/trading/accounts/{id}/positions` (equities, `USDCUSD` qty floored to 6 decimals) | `positions::fetch_inventory` | `AlpacaBrokerApi::fetch_inventory` -> `Inventory` | Moved: returns `Inventory` directly instead of `InventoryResult::Fetched`. |
| `GET .../positions/{symbol}` mark (404 and missing/non-positive mark as `None`, returned-symbol check, encoded symbol) | `positions::fetch_position_mark` | `AlpacaBrokerApi::fetch_position_mark` | Same. |
| `POST /v1/journals` (JNLS, `qty` string) | `client.create_journal` | `AlpacaBrokerApi::create_journal` | Same. |
| `GET /v1/accounts/activities` (form-encoded query, 100 per page, 1000-page cap, repeated token check) | `activity::get_account_activities`, `AlpacaBrokerApiCtx::fetch_account_activities` | same | Same. |
| `GET /v1/calendar?start&end` and session classification (regular, extended, overnight 20:00-04:00 ET, holidays, early closes, DST, next-session lookahead of 14 days, date mismatch) | `market_hours` | `AlpacaBrokerApi::{is_market_open, market_session, market_session_status}` | Same. `session_and_close_at` moved its bound-drift logging into `log_session_bound_drift` to meet the 100-line lint here (the source threshold is 200); behavior is unchanged. |
| Market Data `GET /v2/stocks/{symbol}/trades/latest` | `alpaca_market_data::fetch_latest_trade_price` | `AlpacaBrokerApi::fetch_latest_trade_price` | Moved: newly public so the consumer buy preflight can read the reference price. |
| Market Data `GET /v2/stocks/{symbol}/quotes/latest?feed=delayed_sip` (symbol match, bid/ask positivity, crossed check) | `fetch_latest_quote` | `AlpacaBrokerApi::fetch_latest_quote` -> `LatestQuote` | Moved: returns `LatestQuote` instead of `Option` (the source always returned `Some`). |
| Market Data `...quotes/latest?feed=overnight` with required timestamp | `fetch_latest_overnight_quote` | same | Same. |
| Hosts per mode: broker, data, and authx for sandbox and production; `Mock { base_url }` under `mock` | `AlpacaBrokerApiMode` | same | Same, including the custom deserializer. |

### Types and errors

| Source item | st0x.alpaca item | Status |
| --- | --- | --- |
| `AlpacaBrokerApiCtx { auth, account_id, mode, asset_cache_ttl, time_in_force, counter_trade_slippage_bps, hedge_floor }` | `AlpacaBrokerApiCtx { auth, account_id, mode, asset_cache_ttl, time_in_force }` | Changed: slippage and hedge floor are consumer preflight policy. |
| `AlpacaBrokerApiError` | same | Same, minus `BuyingPowerReservationOutOfRange`, `BuyingPowerReservationOverflow`, and `CounterTradeCost` (raised only by consumer preflight), plus `InvalidEndpoint`. |
| `AlpacaMarketDataError` (public only under `test-support`) | public | Changed: it is reachable through the public `LatestTrade`/`LatestQuote` variants. |
| `AlpacaAmount` (raw 9-decimal value for cash valuation, 6-decimal floored value for transfers) | `broker::AlpacaAmount` | Same. `Usdc::floor_to_6_decimals` is not in st0x-finance `v0.3.0`, so the same floor is a private function here with the source tests (4 unit tests and 1 proptest). |
| `ClientOrderId`, `OrderState`, `OrderStatus`, `OrderUpdate`, `OrderPlacement`, `RecoveredOrderPlacement`, `CancellationOutcome`, `OrderFailureTerminality`, `MarketOrder`, `LimitOrder`, `ExecutorOrderId` | `broker::*` | Same. `OrderStatus` drops its `sqlx::Type` derive (persistence stays in the consumer). |
| `st0x_dto::Direction` | `broker::Direction` | Same wire behavior (snake_case, case-insensitive parse, `BUY`/`SELL` display) without the `ts-rs` derive. |
| `MarketSession`, `PostCloseGap`, `MarketSessionStatus`, `LatestQuote`, `IndicativeQuote`, `LatestQuoteError`, `ALPACA_MAX_DECIMAL_PLACES` | `broker::*` | Same. |
| `truncate_to_decimal_places` (crate-private) | `broker::truncate_to_decimal_places` | Changed: public so the consumer preflight uses the same quantity grid instead of a copy. Same behavior and tests. |
| `prepare_counter_trade_shares` and `PreparedCounterTradeShares` (private) | `AlpacaBrokerApi::prepare_counter_trade_shares` -> `PreparedShares` | Moved: public so the consumer preflight gets the asset's fractional eligibility and quantity precision. Same rule (9 decimals when fractionable, and for extended hours also fractional-extended-hours enabled; whole shares otherwise; missing metadata is whole shares) and warnings. |
| `Executor::parse_order_id` | `AlpacaBrokerApi::parse_order_id` | Moved (UUID check). |
| `Inventory`, `EquityPosition` | `broker::{Inventory, EquityPosition}` | Same. |
| `TimeInForce`, `AccountStatus`, `AssetStatus`, `AssetDetails`, `JournalResponse`, `JournalStatus`, `AccountActivity`, `AccountActivitiesQuery`, `AlpacaLimitOrder`, `AlpacaLimitPrice`, `ConversionOrder`, `ConversionDirection`, `CryptoOrderResponse`, `CryptoOrderOutcome`, `CryptoOrderFailureReason`, `DeadlineCancel`, `MissingOrderField`, `HTTP_REQUEST_TIMEOUT` | same | Same. |
| st0x-finance `NotPositive { value }` (comparison failure treated as positive) | st0x-finance `v0.3.0` `NotPositive::{Constraint, Comparison}` | Changed by the dependency: a failed zero comparison now rejects the value instead of accepting it. `broker::rejected_value` reads the value from either variant where the source read `.value`. |

### Stays in the consumer (st0x.liquidity)

- The `Executor` and `TryIntoExecutor` impls, `SupportedExecutor`, and the
  maintenance hooks.
- Counter-trade preflight: `preflight_counter_trade*`,
  `preflight_sell_inventory`, `preflight_buy_cash`, `resolve_buy_preflight`,
  `resolve_sell_preflight`, buying-power reservations, slippage, and the
  hedge floor. They read Alpaca through `prepare_counter_trade_shares`,
  `fetch_inventory`, `account_funds`, and `fetch_latest_trade_price`.
- `InventoryResult`, `CounterTradePreflight`, `CounterTradeSkipReason`, and
  `MockExecutor`.

## Wallet surface (`wallet`, st0x.liquidity)

| Endpoint or item | Source | st0x.alpaca | Status |
| --- | --- | --- | --- |
| `GET /v1/accounts/{id}/wallets?asset=&network=` | `asset::get_wallet_address` | `AlpacaWalletService::get_wallet_address` | Same. |
| `GET/POST /v1/accounts/{id}/wallets/whitelists`, `DELETE .../{wl}`, `PATCH .../{wl}/travel-rule-info` | `client.rs`, `whitelist.rs` | `AlpacaWalletService::{get_whitelisted_addresses, create_whitelist_entry, remove_whitelist_entries, patch_all_whitelist_travel_rules}` | Same (EIP-55 addresses, required travel-rule info, approved-status check). |
| `POST /v1/accounts/{id}/wallets/transfers` (`Positive<Usdc>` amount as a string) | `transfer::request_withdrawal` | `AlpacaWalletService::initiate_withdrawal` | Same, including the whitelist check before the request. |
| `GET .../wallets/transfers/{id}` (404 as `TransferNotFound`) and `GET .../wallets/transfers` (chain-neutral filter before strict parse) | `transfer.rs` | `AlpacaWalletService::{poll_transfer_until_complete, find_deposit_by_tx_hash, poll_deposit_by_tx_hash, list_all_transfers}` | Same. |
| Polling: 10 s interval, 30 min deadline, 5xx exponential retry (10 attempts, 1-60 s), backwards-status detection | `status.rs`, `PollingConfig` | same | Same. |
| Beneficiary redaction in logs and `ApiError` messages, fail-closed | `client.rs` | same | Same. |
| `AlpacaWalletClient` (public under `test-support`), `AlpacaWalletService::new_with_client` | `client.rs`, `mod.rs` | same | Same (`test-support` feature). |
| `TransferDirection` (type of the public `Transfer.direction`, not re-exported) | `transfer.rs` | `wallet::TransferDirection` | Changed: re-exported so consumers can name it. |
| `alpaca_wallet/serde.rs` | `serde.rs` | none | Not ported: the module was never declared in the source, so its code and 2 tests never compiled. |

## Tokenization surface (`tokenization`, st0x.liquidity)

| Endpoint or item | Source | st0x.alpaca | Status |
| --- | --- | --- | --- |
| `POST /v1/accounts/{id}/tokenization/mint` (`qty` string, lowercase `network`, `issuer: "st0x"`, the issuer request id as both `Idempotency-Key` and `client_request_id`) | `AlpacaTokenizationClient::request_mint` | `AlpacaTokenizationService::request_mint` | Same. |
| Mint error classification: only a 403/422 with a known rejection phrase is definitive; every other failure must be reconciled by issuer request id | `map_mint_error`, `is_definitive_mint_rejection` | same | Same. |
| `GET /v1/accounts/{id}/tokenization/requests` with optional `type`/`status` filters, no pagination | `list_requests`, `fetch_requests_body` | `AlpacaTokenizationService::{list_requests, list_pending_requests}` | Moved: `list_pending_requests` was in the `Tokenizer` impl; the query, the non-pending filter, and its warning are unchanged. |
| Keyed lookups by scanning the list: `get_request`, mint recovery by issuer request id (duplicate detection), redemption detection by tx hash | `get_request`, `find_mint_by_issuer_request_id`, `find_redemption_by_tx` | same names, now `pub` | Same. |
| Network confirmation (a request on another network, or with no network, is refused on the mint response, `get_request`, and redemption lookups) | `confirm_network` | same | Changed: the bound network is `core::Network` instead of `st0x_evm::Chain`. The four shared wire names are identical (`base`, `ethereum`, `hyperevm`, `robinhood`); `core::Network` also has `BnbSmartChain` (`binance`), which `Chain` does not, so the client can be bound to it. As in the source, mint recovery by issuer request id (`find_mint_by_issuer_request_id`) does not confirm the network. |
| Polling: `PollingConfig` interval (10 s) and timeout (30 min), `PollTimeout` | `poll_until_terminal`, `poll_for_redemption_detection` | `poll_mint_until_complete`, `poll_for_redemption`, `poll_redemption_until_complete` | Same. |
| 429 `Retry-After` backpressure, `status_code()` | `AlpacaTokenizationError::backpressure` | same | Same. |
| Credentialed base URL must be HTTPS or HTTP on a loopback IP; no redirects | `validate_credentialed_base_url`; `InvalidBaseUrl(url::ParseError)`, `InsecureBaseUrl` | `endpoint::validate_origin`; `InvalidBaseUrl(EndpointError)` | Changed: one validator for every client. It now also rejects embedded credentials, a query, or a fragment, and accepts `http://localhost` (a loopback host the source refused). The two error variants became one typed variant. |
| `TokenizationRequest`, `TokenizationRequestStatus`, `TokenizationRequestType`, `ClientRequestId`, validated `TokenizationRequestId`, `IssuerRequestId`, `AlpacaApiErrorMessage` | `alpaca.rs`, `lib.rs` | `tokenization::*` | Same. `InvalidTokenizationParameters` is now re-exported (it is a public error field type). |
| Generic `AlpacaTokenizationService<W: Wallet>` with `redemption_wallet` | same | non-generic `AlpacaTokenizationService` | Changed: no Alpaca method reads the wallet or the redemption wallet. |
| `AlpacaTokenizationError::{Evm, MissingRedemptionWallet}` | same | removed | Changed: only the onchain `send_for_redemption` produced them; the consumer error carries them. |
| `send_for_redemption`, `wait_for_block`, `verify_mint_tx`, `redemption_wallet`, `Tokenizer`, `TokenizerError`, `MintVerificationError`, `MockTokenizer` | `alpaca.rs`, `lib.rs`, `mock.rs` | none | Stays in consumer (onchain actions and the trait). |

## Mock servers (`mock`, st0x.liquidity e2e)

| Source item | st0x.alpaca item | Status |
| --- | --- | --- |
| `AlpacaBrokerMock` and `MockMode`, `MockOrderSnapshot`, `MockPosition`, `MockPositionSnapshot`, `MockWalletTransferSnapshot`, `OrderSide`, `OrderStatus`, `TransferDirection`, `TransferFlow`, `TransferStatus`, `WhitelistStatus`, `TEST_ACCOUNT_ID`, `TEST_API_KEY`, `TEST_API_SECRET` (re-exported from `alpaca_broker_api`) | `broker::mock::*` | Moved: a separate module because the mock's wire `OrderStatus`, `TransferStatus`, and `WhitelistStatus` clash with the broker and wallet types of the same names. Endpoints, state model, chaos knobs, and the deposit watcher are the same. |
| `AlpacaTokenizationMock`, `TokenizationStatus`, `TokenizationRequestType`, `RedemptionOutcome`, `MockTokenizationRequestSnapshot`, `REDEMPTION_WALLET` | `tokenization_mock::*` | Same endpoints, idempotency replay, redemption watcher, and mint executor. |
| `DeployableERC20` / `TestERC20` bindings from `ST0X_*_ABI` environment variables | inline `alloy::sol!` ERC-20 interface (`transfer`, `Transfer`) | Changed: this repository has no ABI artifacts; the mock calls only `transfer` and reads `Transfer` logs, so any deployed ERC-20 works. |
| Request-parsing blocks inside the mock responders | extracted helpers | Changed shape only, to meet `too_many_lines`; the responses and status codes are the same and the 23 source tests pass unchanged. |

## Same names, different types

Each surface keeps its source types, so a few names exist more than once.
They are not interchangeable:

| Name | Where | Meaning |
| --- | --- | --- |
| `TokenizationRequestId` | `core` (issuer) | Unvalidated `String`, public `.0`, from issuance. |
| `TokenizationRequestId` | `tokenization` | Validated non-empty id, from liquidity. |
| `IssuerRequestId` | `issuer` | The issuer's wire string (a tx hash). |
| `IssuerRequestId` | `tokenization` | Liquidity's UUID mint tracking id. |
| `Network` | `core` | Closed enum of issued networks (issuer and tokenization). |
| `Network` | `wallet` | Lowercased string newtype for wallet endpoints. |
| `TokenSymbol` | `issuer` / `wallet` | Issuer token ticker / wallet asset symbol. |
| `OrderStatus`, `TransferStatus`, `WhitelistStatus` | `broker` and `wallet` / `broker::mock` | Domain status / mock wire status. |
| `TokenizationRequestType` | `issuer` / `tokenization` / `tokenization_mock` | Issuer response type / liquidity request type / mock wire type. |

## Telemetry

The broker, wallet, and tokenization surfaces emit the same `tracing`
events as the source, with the same targets (`broker`, `wallet`,
`tokenization`) and fields. `tracing` is a facade: the crate installs no
subscriber and no exporter, so consumers keep full control of
instrumentation. The issuer surface emits no events.

## Test parity

Counted per source file at liquidity `5a9895b8` (unit tests, `tokio` tests,
traced tests, and proptests). "Ported" includes tests renamed when their
subject moved.

| Source file | Source tests | Ported | Not ported |
| --- | ---: | ---: | --- |
| `execution/src/alpaca_amount.rs` | 5 | 5 | - |
| `execution/src/alpaca_broker_api/activity.rs` | 6 | 6 | - |
| `execution/src/alpaca_broker_api/auth.rs` | 16 | 16 | - |
| `execution/src/alpaca_broker_api/client.rs` | 18 | 18 | - |
| `execution/src/alpaca_broker_api/executor.rs` | 66 | 56 | 10 preflight/trait tests (consumer): `test_preflight_counter_trade_*` (4), `non_fractionable_buy_preflight_*`, `non_fractionable_sell_preflight_*` (3), `test_to_supported_executor`, `test_maintenance_interval_returns_none`. Renamed: `test_get_inventory_returns_fetched`, `extended_hours_sell_preflight_uses_extended_hours_precision`, `non_fractionable_quantity_below_one_skips_preflight_without_broker_order` now assert the Alpaca-side result (`fetch_inventory`, `prepare_counter_trade_shares`). |
| `execution/src/alpaca_broker_api/journal.rs` | 8 | 8 | - |
| `execution/src/alpaca_broker_api/kms_jwt.rs` | 15 | 15 | - |
| `execution/src/alpaca_broker_api/market_hours.rs` | 52 | 52 | - |
| `execution/src/alpaca_broker_api/mock_api.rs` | 20 | 20 | - |
| `execution/src/alpaca_broker_api/mod.rs` | 13 | 13 | - |
| `execution/src/alpaca_broker_api/order.rs` | 79 | 79 | - |
| `execution/src/alpaca_broker_api/positions.rs` | 28 | 28 | - |
| `execution/src/alpaca_market_data.rs` | 22 | 22 | - |
| `execution/src/alpaca_wallet/{asset,client,mod,status,transfer,whitelist}.rs` | 85 | 85 | - (1 live-sandbox test stays `#[ignore]`, as in the source) |
| `execution/src/alpaca_wallet/serde.rs` | 2 | 0 | Never compiled in the source (module not declared). |
| `execution/src/order/{mod,state,status}.rs` | 13 | 13 | - |
| `execution/src/rate_limit.rs` | 10 | 10 | - |
| `execution/src/lib.rs` | 46 | 11 | 35 consumer tests: preflight sizing (`resolve_*`, `estimate_buffered_cost_cents_*`), `Shares`, `SupportedExecutor`, and st0x-finance arithmetic/symbol tests that belong to st0x.finance. Ported: decimal truncation (8), session status (2), status permanence (1). |
| `execution/src/hedge_floor.rs`, `execution/src/mock.rs` | 34 | 1 | Consumer (hedge floor, `MockExecutor`). |
| `tokenization/src/alpaca.rs` | 62 | 51 | 11 onchain tests (Anvil/ERC-20): `test_send_tokens_for_redemption_*` (2), `test_wait_for_block_*`, `test_verify_mint_tx_*` (8). |
| `tokenization/src/lib.rs` | 9 | 9 | - |
| `tokenization/src/mock_api.rs` | 3 | 3 | - |
| `tokenization/src/mock.rs` | 2 | 0 | Consumer (`MockTokenizer`). |
| `dto/src/trade.rs` (`Direction`) | 2 | 2 | - |

Issuance (`b3b955f`): every Alpaca test in `src/alpaca/{mod,itn,service,mock}.rs`
has a counterpart in `src/issuer/`, including the ITN list test and the
three Ethereum-network tests. Not ported: the eight log-assertion tests,
because the issuer surface emits no events.

Corporate-action stream: 23 source tests are ported (4 identity, 4 bootstrap
instant through `FromStr`, 1 endpoint, 14 decoder;
`invalid_payload_error_logs_the_valid_sse_event_id` is renamed
`invalid_payload_error_retains_the_valid_sse_event_id` without its log
assertion). The request-side assertions of 6 feed tests are kept as new
client tests (cursor replay, first install, bounded window, EOF inside a
frame, truncated body, wrong content type). Not ported: 29 projection,
database, reconnect, notification, and shutdown tests, which stay in
issuance.

New tests (not in either source): origin validation, path-segment encoding,
and redirect refusal for the issuer, broker, wallet, and tokenization clients
and the token mint; token-URL validation for both JWT modes; fees encodings;
unpublished ITN network strings; ITN error classification;
`fetch_latest_trade_price`; `Direction` serde. Corporate-action stream:
symbol rule, replay query map, default URL, the test-support loopback
constructor, non-US and blank symbols, credential-free live request, status
and refused redirect, keyless bearer token, credentials built only for
authenticated endpoints, and userinfo/fragment rejection.
