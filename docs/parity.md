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
| Broker and Market Data (st0x.liquidity) | 13 | 13 | Preflight policy fields and errors stay in the consumer; trait mappings became inherent methods; redirects disabled and mode URLs validated; a USDC conversion 403 `40310000` is insufficient balance only when its message says so; the conversion poll starts no read at or after its deadline and no read or repeated cancel at or after its settle window; a market data request that never built is Permanent. |
| Wallet (st0x.liquidity) | 8 | 8 | Redirects disabled and base URL validated; the direct client has a 10 s connect timeout and no total request timeout, while the gateway builds it with a 30 s total request timeout to bound detached work; the deposit poll matches incoming transfers only; the polls retry every transient read error after the longer of the exponential backoff and its `Retry-After`, and start no read at or after their deadline. |
| Tokenization (st0x.liquidity) | 2 | 2 | Service not generic over a wallet; onchain actions and their error variants stay in the consumer; the polls retry every transient lookup error, as the wallet polls do, and neither start a lookup nor wait for an interval past their deadline. |
| Mocks (st0x.liquidity e2e) | 2 servers | 2 | Inline ERC-20 interface instead of ABI environment variables. |
| Shared auth and transport | - | - | Public auth internals made private; request paths confined to the configured origin; token URLs validated; a throttled token mint holds further mints off for its capped `Retry-After`; new `request_id` scope that records the Alpaca request ids of a future, and a send gate that holds its requests back. |

CI runs every test with `cargo test --locked --workspace --all-features`
(the `test` job in `.github/workflows/ci.yaml`); the one live sandbox test
stays ignored, as in the source. Every source test is ported except the
consumer side tests listed under Test parity.

## Shared transport and auth (`core`, `auth`, `endpoint`, `rate_limit`, `request_id`)

| Source item | st0x.alpaca item | Status |
| --- | --- | --- |
| liquidity `AlpacaBrokerAuth` (Basic, KmsJwt, PrivateKeyJwt; untagged, field-shaped) | `core::AlpacaAuth` | Same. Debug redacts with `[REDACTED]` (the issuer copy used `<redacted>`). |
| liquidity `kms_jwt.rs` (RFC 7523 assertions, KMS `asymmetricSign`, metadata token, local PEM, token cache with soft/hard deadlines, failed mint backoff, stale token fallback) | `auth` | Same, including the two log events (`warn` on riding a cached token, `info` on each mint). Changed: after a rate limited mint no mint is attempted until its `Retry-After` passes, counted as at least the 15 s failed mint backoff and at most `MAX_RETRY_AFTER_HOLD` (5 min). A call inside that hold rides a still valid cached token or fails with a 429 `TokenStatus` carrying the remaining wait, and a cached token's next refresh also waits out a `Retry-After` longer than 15 s. The source deferred only the cached token's refresh, by 15 s, and minted again on the next call that found no usable token, so every caller of a throttled credential reached the token endpoint again. |
| `ALPACA_TOKEN_URL`, `ALPACA_SANDBOX_TOKEN_URL` | same names | Same. |
| `AuthRuntime` (pub), `KmsJwtAuth::new` / `with_urls` (pub, arbitrary token/KMS/metadata URLs and HTTP client) | crate-private `AuthRuntime`; private `KmsJwtAuth::with_urls` | Changed: every credential-bearing construction goes through `AuthRuntime::build`, which validates the token URL and uses a no-redirect client. No consumer uses these items outside the Alpaca crates. |
| `AuthRuntime::build(auth, token_url)` | same (crate-private) | Changed: JWT token URLs must be HTTPS (HTTP only on loopback) without embedded credentials, query, or fragment. New `KmsJwtError::InvalidTokenUrl`, classified deterministic. |
| `apply_apca` / `apply_wallet` / `broker_authorization`, sensitive header values | same (crate-private) | Same. |
| `KmsJwtError` and `is_deterministic` / `is_rate_limited` / `retry_after` | same | Same, plus `InvalidTokenUrl` and `NotSent(GateClosed)` (a mint a closed send gate held back, see the send gate row below; deterministic). Changed: a 3xx from KMS or the token endpoint is deterministic, because the mint client does not follow redirects, and the `retry_after` of `KmsStatus` and `TokenStatus` is capped at `MAX_RETRY_AFTER_HOLD` (5 min), the first throttled mint's included, so no caller (and no gateway relaying it) is told to hold off longer than the mint hold lasts. The source carried the endpoint's hint unbounded. |
| `rate_limit::parse_retry_after`, `retry_after_from_response_headers` | same | Same (the liquidity file and its 10 tests replace the shorter issuer copy). |
| (none) | crate private `rate_limit::{next_poll_delay, poll_deadline, read_before}` | New: the rules every poll shares. `poll_deadline` fixes a poll's absolute deadline, `next_poll_delay` cuts the wait before the next read at it, and `read_before` runs a wallet or tokenization read only before it and drops one still running there, ending as the poll's own timeout; a tokenization poll also waits for its next interval only until the deadline. The conversion poll bounds its status reads the same way with `tokio::time::timeout_at`, but never drops a cancel it sent. |
| `Backpressure`, `Permanence` | `core::{Backpressure, Permanence}` | Same. |
| liquidity `status_permanence(StatusCode)` | `core::response_status_permanence` | Changed: a 3xx is now Permanent. The source clients followed redirects, so a 3xx never surfaced; with redirects disabled the same request would be redirected again. Otherwise the same policy and test. The issuer keeps its existing `u16` classifier (3xx is already Permanent there). |
| issuer `AlpacaClient` public `get`/`post`/`delete`/`patch`/`market_data_get` taking any HTTPS URL; ids interpolated into the path with `format!` | crate-private `get`/`post` taking path segments | Changed: the URL is built on the configured base URL from percent-encoded segments, so credentials cannot reach another host and an id containing `/`, `?`, or `#` cannot address another endpoint. Empty and dot segments are rejected (`EndpointError::InvalidPathSegment`). `delete`, `patch`, and `market_data_get` had no issuer callers and are removed. |
| issuer `AlpacaError::InvalidUrl(String)` | `AlpacaError::InvalidUrl(EndpointError)` | Changed: typed error instead of an opaque string. |
| Broker HTTP client (reqwest default: follows up to 10 redirects; mode URLs unvalidated) | `broker::client` | Changed: redirects disabled; the mode's broker and market-data URLs are validated (`AlpacaBrokerApiError::InvalidEndpoint`). Sandbox and production URLs are unchanged. |
| Wallet HTTP client (`reqwest::Client::new()`: follows redirects; base URL unvalidated; no timeouts) | `wallet::client` | Changed: redirects disabled and base URL validated (`AlpacaWalletError::InvalidBaseUrl`). The direct path has a 10 s connect timeout and no total request timeout, so a slow keyless write does not become an error that invites a duplicate. The gateway builds the same client with a 30 s total request timeout to bound detached work, but expiry remains ambiguous: recovery waits for the result carrying `answered` or `settled` audit record and reconciles repeatedly over a longer window before any replacement. |
| liquidity `rate_limit::retry_after_from_response_headers` re-exported from the crate root | crate-private | Changed: only the Alpaca clients used it, and they now live here. |
| (none) | root `pub use st0x_finance` | New: every public amount, quantity, and symbol type is from `st0x-finance` `v0.3.0`; consumers must use that release (liquidity uses its in-repo copy today, see the `NotPositive` row below). |
| (none) | `endpoint::validate_credential_origin` | New: the crate private `validate_origin` rule (HTTPS, HTTP only on a loopback host, a host, no embedded credentials, query, or fragment) made public with role `EndpointRole::CredentialOrigin`, so the gateway client and the gateway's key URL config check their URLs with the same rule as every library client. |
| (none) | `core::GatewayHopError`, the `Gateway` variant of `AlpacaBrokerApiError`, `AlpacaWalletError`, and `AlpacaTokenizationError` | New: a failure on the hop to the Alpaca gateway (no answer, or a refusal the gateway decided itself). Only gateway clients raise it; the direct clients never do. It is Transient when the gateway marked it retryable or safe to resend with the same idempotency key, Permanent otherwise, and it is backpressure only when it carries the gateway's `retryAfterSecs` (`retry_after`). |
| (none) | `request_id::{Traffic, collect, ALPACA_REQUEST_ID_HEADER}` | New: records the Alpaca answers of one future for a caller that audits them (the gateway). `collect(future)` returns the output with its `Traffic`: `request_ids`, the `X-Request-ID` of every answer that carried one, in order, credential mint answers included, and `last_status`, the status of the last Alpaca API answer, which a credential mint answer never sets. The broker, market data, wallet, tokenization, issuer and corporate action stream clients record each answer right after its send returns, before reading its body. Outside a scope nothing is recorded, so a consumer that opens no scope sees no behavior change. |
| (none) | `request_id::{SendGate, gated, GateClosed}`, the `NotSent(GateClosed)` variant of `AlpacaBrokerApiError`, `AlpacaMarketDataError`, `AlpacaWalletError`, `AlpacaTokenizationError`, `AlpacaError` and `CorporateActionStreamError`, and `KmsJwtError::NotSent` | New: lets a caller stop the requests of one future from leaving after some point (the gateway's answer deadline and shutdown). `gated(gate, future)` runs the future under `gate`, a `SendGate::new(is_open)` closure. Every broker, market data, wallet, tokenization, issuer and corporate action stream request asks it right before it leaves, once its credential is in hand, and holds its lock until reqwest has the request; a credential mint asks it before it starts. `SendGate::close()` takes the same lock, so once it returns no request or mint of the future starts. A held back request fails with its error type's `NotSent`, a held back mint with `KmsJwtError::NotSent` inside the client's `KmsJwt` or `Auth` variant. Every classifier treats both as a request that never left: `permanence()` Permanent (a gate only ever closes), not retryable by `AlpacaError::is_retryable()`, no backpressure, not a definitive mint rejection, and `PlacementError::written` and `IssuerCallError::written` false. A consumer's exhaustive match on `AlpacaError` (issuance's `classify_journal_poll_error`) or `CorporateActionStreamError` needs an arm for `NotSent`. Outside a `gated` scope every request leaves, so a consumer that opens none sees no behavior change. |

## Issuer surface (`issuer`, st0x.issuance)

| Source item (issuance) | st0x.alpaca item | Status |
| --- | --- | --- |
| `POST /v1/accounts/{account_id}/tokenization/callback/mint` (`send_mint_callback`) | `IssuerApi::send_mint_callback` | Same, under the shared retry policy. A 429 is `RateLimited` with its `Retry-After` hint; issuance returned `Api { 429 }`. Both retry. |
| `POST /v1/accounts/{account_id}/tokenization/callback/redeem` (`call_redeem_endpoint`) | `IssuerApi::call_redeem_endpoint` | Changed: the retry is inside the method, and every attempt sends `issuer_request_id` as its `Idempotency-Key`, so Alpaca replays the first answer to a resend instead of refusing the reused id with 422. |
| ITN preflight `itn::accepts_network_wire_string` before the redeem call; `UnsupportedTokenizationNetwork { network, reference }` | `issuer::itn::{TOKENIZATION_NETWORK_WIRE_STRINGS, REDEEM_CALLBACK_OPENAPI_REFERENCE, accepts_network_wire_string}`; `AlpacaError::UnsupportedTokenizationNetwork` | Same. The check runs before the retry and before any HTTP call, and the error is not retryable. As in issuance, every `Network` value is on the list, so the check cannot fail today; it guards a future variant. |
| `GET /v1/accounts/{account_id}/tokenization/requests/{id}` (`poll_request_status`), 404 as `RequestNotFound`, id mismatch as `ResponseIdMismatch` | `IssuerApi::poll_request_status` | Same. |
| (none) | `IssuerCallError { written, alpaca_status, error }`, `AlpacaClient::{send_mint_callback_reporting, call_redeem_endpoint_reporting, poll_request_status_reporting}` | New: the two issuer POSTs report whether any attempt of their `with_retry` may have reached Alpaca, as `PlacementError` does for an order POST. `written` is false only when no attempt left (a closed send gate, a credential, URL, network, or redeem `Idempotency-Key` preflight failure, a request that never built or connected) or Alpaca answered each with a definite rejection (a 4xx other than 408, so 401, 403 and 429 too, but not a mint callback 400 or a redeem 422, which Alpaca also gives once the request may have been applied); a 500 answered later with a 400 stays written. The keyed read reports the same error with `written` always false. `alpaca_status` is the status of the last answer Alpaca gave any attempt of the call, so a 500 followed by a local `Retry-After` hold keeps 500, and a call no attempt of which got an answer (a hold, a connect failure, a failed mint) has none. `IssuerApi::{send_mint_callback, call_redeem_endpoint, poll_request_status}` delegate to them and drop the report, so valid issuance calls see no change. |
| (none) | `Serialize` on `RedeemResponse`, `Fees` and `issuer::TokenizationRequest` | New: the gateway answers with these types under Alpaca's field names, and its client reads them back with the same `Deserialize`. A round trip keeps the values, not Alpaca's spelling: an empty `tx_hash` writes `null`, and `qty` writes its canonical decimal (`"50.00"` as `"50"`). |
| Request qty from `Decimal` keeping lexical scale (`100.50`) | `RedeemQty` (validated through `FractionalShares`, serializes the caller's exact string) | Moved: wire spelling is preserved without `rust_decimal`. The gateway accepts only a positive plain decimal with at most 9 fractional digits, refusing exponent notation or a finer scale before Alpaca is called. |
| Response qty `Decimal` | `Qty(FractionalShares)` | Changed: Rain Float for arithmetic. Response quantities are not sent back. |
| `Fees(Decimal)` | `Fees(Usd)` | Changed: Rain Float-backed `st0x_finance::Usd`. Every production encoding (`"0"`, `"0.0"`, `"0.01"`, `"0.5"`, `"0.001"`), a JSON number, absent, and `null` are covered by tests. `rust_decimal` is no longer a dependency. |
| `tx_hash` absent / null / empty on a pending redeem poll | `deserialize_optional_b256` with `serde(default)` | Same. |
| `issuer_request_id` typed enum | `IssuerRequestId(String)` | Changed: kept as the wire string; issuance parses its own id format. The gateway refuses control characters, and the direct issuer client builds the redeem `Idempotency-Key` once before retrying so an invalid header value is a permanent unsent error. |
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
| Status and content-type checks; `CorporateActionFeedError::{Http, HttpStatus, InvalidContentType}` | `CorporateActionStreamClient::connect`, over the new `connect_raw`; `CorporateActionStreamError {Http, HttpStatus, RateLimited, InvalidContentType, Auth, NotSent}` | Same messages; `RateLimited`, `Auth` and `NotSent` are new, and `InvalidContentType` is a struct variant that also carries the status Alpaca answered. A 429 is `RateLimited { retry_after }` with Alpaca's `Retry-After` hint, read by the crate's shared parser, instead of `HttpStatus(429)`; every other status stays `HttpStatus`. Issuance's exhaustive match over the error needs the new arm. `connect_raw` makes the same request and checks and returns the response with its body unread, so the gateway relays the stream's bytes unchanged. |
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
| `POST .../orders` crypto `USDCUSD`: sell by `qty`, buy by whole-cent `notional`, `gtc`; a buy rejected with 403 `40310000` and an "insufficient balance" message as `UsdConversionInsufficientBalance`; any other rejection (including "no available quote") passes through as `ApiError` | `order::convert_usdc_usd` | `AlpacaBrokerApi::convert_usdc_usd` | Changed: the source classified every buy 403 `40310000` as `UsdConversionInsufficientBalance`. Alpaca reuses that code for "no available quote for symbol", which resizing cannot fix, so the message must also say "insufficient balance" (any case). |
| Conversion polling: 300 s deadline, cancel, 30 s settle window, 500 ms reads, cancel/fill race, partial fills, `DoneForDay` waited on | `poll_crypto_order_until_filled`, `poll_crypto_order_to_terminal`, `cancel_and_settle` | `AlpacaBrokerApi::{convert_usdc_usd, poll_conversion_to_terminal}` | Changed in the deadlines and the reported fill. No status read starts at or after the 300 s deadline, and a read still running there is dropped, so the remainder is cancelled on time whatever the last read answered; the source checked the deadline only after a read answered, so a slow read held the cancel back. The 30 s settle window starts when the first deadline cancel answers, as in the source; no status read or repeated cancel starts at or after its end, a status read still running there is dropped, and the poll then returns `ConversionCancelNotSettled`; the source bounded neither call and checked the window only after a read answered. `ConversionCancelNotSettled.filled_quantity` is the last fill any read reported, before the cancel or after it, and a later read that omits the fill keeps it; the source took only the last settle read's value. A read before the deadline cancel that fails with an error `permanence()` calls transient (a 5xx, 408 or 429, a transport failure, a gateway hop marked `retryable`) logs a `warn` event and is read again at the next interval, or after the `Retry-After` it relayed when that is longer; the source ended the poll on any read error, skipping the deadline cancel. Any other read error still ends the poll. Otherwise the same: a failed settle read is read again inside the window, and a failed cancel is `Failed` and sent again every 500 ms inside the window. |
| `GET .../orders:by_client_order_id` for crypto | `get_crypto_order_by_client_order_id` | `AlpacaBrokerApi::find_conversion_order` | Same. |
| `GET /v1/trading/accounts/{id}/positions` (equities, `USDCUSD` qty floored to 6 decimals) | `positions::fetch_inventory` | `AlpacaBrokerApi::fetch_inventory` -> `Inventory` | Moved: returns `Inventory` directly instead of `InventoryResult::Fetched`. |
| `GET .../positions/{symbol}` mark (404 and missing/non-positive mark as `None`, a failed zero comparison as `FloatConversion`, returned-symbol check, encoded symbol) | `positions::fetch_position_mark` | `AlpacaBrokerApi::fetch_position_mark` | Same (comparison error per liquidity `07970f828`). |
| `POST /v1/journals` (JNLS, `qty` string) | `client.create_journal` | `AlpacaBrokerApi::create_journal` | Same. |
| `GET /v1/accounts/activities` (form-encoded query, 100 per page, 1000-page cap, repeated token check) | `activity::get_account_activities`, `AlpacaBrokerApiCtx::fetch_account_activities` | same | Same. |
| `GET /v1/calendar?start&end` and session classification (regular, extended, overnight 20:00-04:00 ET, holidays, early closes, DST, next-session lookahead of 14 days, date mismatch) | `market_hours` | `AlpacaBrokerApi::{is_market_open, market_session, market_session_status}` | Same. `session_and_close_at` moved its bound-drift logging into `log_session_bound_drift` to meet the 100-line lint here (the source threshold is 200); behavior is unchanged. |
| Market Data `GET /v2/stocks/{symbol}/trades/latest` | `alpaca_market_data::fetch_latest_trade_price` | `AlpacaBrokerApi::fetch_latest_trade_price` | Moved: newly public so the consumer buy preflight can read the reference price. |
| Market Data `GET /v2/stocks/{symbol}/quotes/latest?feed=delayed_sip` (symbol match, bid/ask positivity, crossed check) | `fetch_latest_quote` | `AlpacaBrokerApi::fetch_latest_quote` -> `LatestQuote` | Moved: returns `LatestQuote` instead of `Option` (the source always returned `Some`). |
| Market Data `...quotes/latest?feed=overnight` with required timestamp | `fetch_latest_overnight_quote` | same | Same. |
| Hosts per mode: broker, data, and authx for sandbox and production; `Mock { base_url }` under `mock` | `AlpacaBrokerApiMode` | same | Same, including the custom deserializer. `token_url()` is public, so the gateway builds its issuer and stream clients for the mode's authx host. |

### Types and errors

| Source item | st0x.alpaca item | Status |
| --- | --- | --- |
| `AlpacaBrokerApiCtx { auth, account_id, mode, asset_cache_ttl, time_in_force, counter_trade_slippage_bps, hedge_floor }` | `AlpacaBrokerApiCtx { auth, account_id, mode, asset_cache_ttl, time_in_force }` | Changed: slippage and hedge floor are consumer preflight policy. |
| `AlpacaBrokerApiError` | same | Same, minus `BuyingPowerReservationOutOfRange`, `BuyingPowerReservationOverflow`, and `CounterTradeCost` (raised only by consumer preflight), plus `InvalidEndpoint` and `Gateway` (see `GatewayHopError` above). `permanence()` keeps the source rule for an `HttpClient` failure: Permanent only for a request that never built, Transient for every other, a body cut short or timed out while it streams included (reqwest 0.13 `Response::bytes()` reports that as a decode error); a body that arrived whole but does not parse is `JsonParse`, Permanent. |
| `AlpacaMarketDataError` (public only under `test-support`) | public | Changed: it is reachable through the public `LatestTrade`/`LatestQuote` variants. Its crate private `permanence()`, which `AlpacaBrokerApiError::permanence()` delegates to for those variants, classifies an `Http` request that never built Permanent and every other `Http` failure Transient, as the broker error does; the source classified every `Http` failure Transient. |
| `AlpacaAmount` (raw 9-decimal value for cash valuation, 6-decimal floored value for transfers) | `broker::AlpacaAmount` | Same. `Usdc::floor_to_6_decimals` is not in st0x-finance `v0.3.0`, so the same floor is a private function here with the source tests (4 unit tests and 1 proptest). |
| `ClientOrderId`, `OrderState`, `OrderStatus`, `OrderUpdate`, `OrderPlacement`, `RecoveredOrderPlacement`, `CancellationOutcome`, `OrderFailureTerminality`, `MarketOrder`, `LimitOrder`, `ExecutorOrderId` | `broker::*` | Same. `OrderStatus` drops its `sqlx::Type` derive (persistence stays in the consumer). |
| `st0x_dto::Direction` | `broker::Direction` | Same wire behavior (snake_case, case-insensitive parse, `BUY`/`SELL` display) without the `ts-rs` derive. |
| `MarketSession`, `PostCloseGap`, `MarketSessionStatus`, `LatestQuote`, `IndicativeQuote`, `LatestQuoteError`, `ALPACA_MAX_DECIMAL_PLACES` | `broker::*` | Same. |
| `truncate_to_decimal_places` (crate-private) | `broker::truncate_to_decimal_places` | Changed: public so the consumer preflight uses the same quantity grid instead of a copy. Same behavior and tests. |
| `prepare_counter_trade_shares` and `PreparedCounterTradeShares` (private) | `AlpacaBrokerApi::prepare_counter_trade_shares` -> `PreparedShares` | Moved: public so the consumer preflight gets the asset's fractional eligibility and quantity precision. Same rule (9 decimals when fractionable, and for extended hours also fractional-extended-hours enabled; whole shares otherwise; missing metadata is whole shares) and warnings. |
| `Executor::parse_order_id` | `AlpacaBrokerApi::parse_order_id` | Moved (UUID check). |
| `Inventory`, `EquityPosition` | `broker::{Inventory, EquityPosition}` | Same. |
| `TimeInForce`, `AccountStatus`, `AssetStatus`, `AssetDetails`, `JournalResponse`, `JournalStatus`, `AccountActivity`, `AccountActivitiesQuery`, `AlpacaLimitOrder`, `AlpacaLimitPrice`, `ConversionOrder`, `ConversionDirection`, `CryptoOrderResponse`, `CryptoOrderOutcome`, `CryptoOrderFailureReason`, `DeadlineCancel`, `MissingOrderField`, `HTTP_REQUEST_TIMEOUT` | same | Same. |
| (none) | `Serialize` and `Deserialize` on `OrderState`, `OrderPlacement`, `RecoveredOrderPlacement`, `CancellationOutcome`, `MarketSessionStatus`, `PostCloseGap`, `AssetDetails`, `AssetStatus`, `PreparedShares`, `AccountFunds`, `Inventory`, `EquityPosition`, `JournalStatus`, `CryptoOrderResponse`, and `wallet::Transfer` and `tokenization::TokenizationRequest` | New: the gateway answers with these types. A type read from Alpaca keeps Alpaca's field names; the others are camelCase, `OrderState` tagged by `status`. |
| st0x-finance `NotPositive { value }` (comparison failure treated as positive) | st0x-finance `v0.3.0` `NotPositive::{Constraint, Comparison}` | Changed by the dependency: a failed zero comparison now rejects the value instead of accepting it. As in liquidity `07970f828`, `Constraint` keeps its domain behavior (`NonPositive*` errors, or `None` for a position mark), and `Comparison` surfaces as `AlpacaMarketDataError::Float` (Permanent, trades and quotes) or `AlpacaBrokerApiError::FloatConversion` (position mark). |

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
| `GET/POST /v1/accounts/{id}/wallets/whitelists`, `DELETE .../{wl}`, `PATCH .../{wl}/travel-rule-info` | `client.rs`, `whitelist.rs` | `AlpacaWalletService::{get_whitelisted_addresses, create_whitelist_entry, remove_whitelist_entries, patch_all_whitelist_travel_rules}` | Same (EIP-55 addresses, required travel-rule info, approved-status check). Changed: a whitelist DELETE or PATCH completes as soon as Alpaca answers a success status, without reading the response body, which both discard. The source read the whole body first, so a success body cut short, not UTF-8, or never ending turned a write Alpaca had confirmed into a failure or held it back. An error status still reads the body for the `ApiError` message. |
| `POST /v1/accounts/{id}/wallets/transfers` (`Positive<Usdc>` amount as a string) | `transfer::request_withdrawal` | `AlpacaWalletService::initiate_withdrawal` | Same, including the whitelist check before the request. |
| `GET .../wallets/transfers/{id}` (404 as `TransferNotFound`) and `GET .../wallets/transfers` (chain neutral filter before strict parse) | `transfer.rs` | `AlpacaWalletService::{poll_transfer_until_complete, find_deposit_by_tx_hash, list_all_transfers}` | Same. |
| Deposit poll by tx hash | `poll_deposit_by_tx_hash` over `find_transfer_by_tx_hash` (either direction) | `AlpacaWalletService::poll_deposit_by_tx_hash` and `poll_deposit_by_tx_hash_with` over `WalletTransfers::find_deposit_by_tx_hash` | Changed: the poll matches incoming transfers only, through the required trait method `WalletTransfers::find_deposit_by_tx_hash` (the incoming only scan of `find_deposit_by_tx_hash` for the direct client, the `wallet.find_deposit` operation for a gateway client). The source matched a transfer in either direction, so an outgoing transfer carrying the same hash could be returned as the deposit or shadow the deposit behind it. |
| Polling: 10 s interval, 30 min deadline, 5xx exponential retry (10 attempts, 1 to 60 s), backwards status detection | `status.rs`, `PollingConfig` | Same interval, deadline, retry schedule, and backwards status detection. Changed: both polls retry by the one rule all three wallet polls share, a read error that `AlpacaWalletError::permanence()` classifies Transient (a 5xx, 408, or 429, a transport failure, a token mint failure that is not deterministic, a `Gateway` hop the gateway classified Transient), so a 408, a 429, a transport failure, or a throttled mint is retried where the source returned it from the first read. Each retry waits for the longer of its exponential backoff and its `Retry-After` hint, without spending another retry during that wait. No read starts at or after the deadline, and a read still running there is dropped, ending as `TransferTimeout` or `DepositTimeout`; the source checked the deadline only between reads and let a read run past it. |
| One shot `GET .../wallets/transfers/{id}` with optional `network_fee` and `fees`, `reported_fees()` checked USDC sum | liquidity `1ad78737c` `transfer.rs` | `AlpacaWalletService::get_transfer` -> `TransferWithFees` | Same, plus `ReportedFeesError` for an asset other than USDC instead of labelling its fees as USDC. `Transfer` keeps its shape. |
| Completed transfer tx hash poll | liquidity `1ad78737c` `status::poll_transfer_tx_hash` | `AlpacaWalletService::poll_transfer_tx_hash`, `poll_transfer_tx_hash_with` | Changed: no read starts at or after the deadline and a read still running there is dropped (`TransferTimeout`), a hash is returned only once the transfer is `Complete`, a `Failed` transfer returns `TransferFailed` or `FailedTransferHasTx`, and a read error that `AlpacaWalletError::permanence()` classifies Permanent returns at once. A Transient read error is retried at the next 10 s interval, or after the wait it relayed (`Retry-After`, a throttled token mint's, or a gateway hop's) when that is longer, never past the deadline. The source retried every read error until the deadline and returned any hash. `CompletedTransferMissingTx` is part of the error contract but is raised by the consumer. |
| (none) | none | `AlpacaWalletError::permanence()` | New, mirroring `AlpacaBrokerApiError::permanence()`: an HTTP status by the shared status rule (`response_status_permanence`), a request that never built Permanent, any other transport failure (a body cut short or timed out while it streams included) and a token mint failure that is not deterministic Transient, a `Gateway` hop by `GatewayHopError::permanence()`, and every other variant (a body that arrived whole but does not parse, configuration, `TransferNotFound`, whitelist refusals, poll outcomes) Permanent. The three wallet polls retry by it, and the gateway's answer mapping uses it. |
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
| Polling: `PollingConfig` interval (10 s) and timeout (30 min), `PollTimeout` | `poll_until_terminal`, `poll_for_redemption_detection` | `poll_mint_until_complete`, `poll_for_redemption`, `poll_redemption_until_complete`, and over any `TokenizationLookups` `poll_request_until_complete_with` and `poll_for_redemption_with` | Changed: the polls retry by the rule of the wallet polls, a lookup error that `AlpacaTokenizationError::permanence()` classifies Transient (a 5xx, 408, or 429, a transport failure, a token mint failure that is not deterministic, a `Gateway` hop the gateway classified Transient). Such an error logs a `warn` event and the poll looks up again at the next interval, after any longer `Retry-After` hint. Any other lookup error ends the poll; the source ended the poll on the first lookup error of any kind. No lookup starts at or after the timeout, a lookup still running there is dropped, and neither an interval nor a `Retry-After` wait runs past the timeout, each ending as `PollTimeout`, so a poll whose interval is longer than its timeout returns at the timeout after one lookup; the source checked the timeout after each interval wait, let a lookup run past it, and waited for an interval tick that fell past it. |
| 429 `Retry-After` backpressure, `status_code()` | `AlpacaTokenizationError::backpressure` | same | Same, plus the relayed wait of a `Gateway` hop failure. |
| (none) | none | `AlpacaTokenizationError::permanence()` | New, mirroring `AlpacaBrokerApiError::permanence()`: an HTTP status by the shared status rule (`response_status_permanence`), a request that never built Permanent, any other transport failure (a body cut short or timed out while it streams included) and a token mint failure that is not deterministic Transient, a `Gateway` hop by `GatewayHopError::permanence()`, and every other variant (a body that arrived whole but does not parse, configuration, a definitive mint refusal, `RequestNotFound`, `DuplicateMintIssuerRequestId`, a request on another network or with none, `PollTimeout`) Permanent. The three tokenization polls retry by it, and the gateway's answer mapping uses it. |
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

## Gateway (`st0x-alpaca-gateway`)

The gateway is a consumer of this library, not a port. Each operation's handler (`crates/gateway/src/handlers`) runs on one `AlpacaBrokerApi`, one `AlpacaWalletService`, and one `AlpacaTokenizationService` per configured network, and in the `s01` profile on three issuer `AlpacaClient`s and one `CorporateActionStreamClient`. The design ([RAI-1927](https://linear.app/makeitrain/issue/RAI-1927)) names one library method per operation. Checks the gateway makes before it calls Alpaca (tier key forms, pinned destinations, the form and length of symbols, assets, networks, and journal counterparty names, query value rules, the human budget) are gateway policy, described in [docs/gateway.md](gateway.md).

Every operation not in the table below runs the library method of the same name unchanged: `AlpacaBrokerApi::{account_funds, withdrawable_cash_cents, fetch_inventory, fetch_position_mark, is_market_open, market_session, market_session_status, fetch_latest_trade_price, fetch_latest_quote, fetch_latest_overnight_quote, get_asset_details, prepare_counter_trade_shares, get_order_status, get_order_by_client_order_id, recover_order_by_client_id, cancel_order, submit_conversion, get_conversion_order, find_conversion_order, create_journal}` and `AlpacaWalletService::{get_transfer, list_all_transfers, find_deposit_by_tx_hash, get_wallet_address, get_whitelisted_addresses, create_whitelist_entry}`. `submit_conversion` is the conversion POST of `convert_usdc_usd` without its poll, and `get_conversion_order` one read of it. A journal goes to the account of the configured counterparty the request names, a whitelist entry is created on network `ethereum` with the configured Travel Rule beneficiary, and `wallet.transfer` sums `reported_fees()` (no fees for an asset other than USDC). Tokenization operations run on the client of the request's `network`, except `tokenization.requests` and `tokenization.find_mint`, which do not check the network and run on the first configured one.

| Operation | `st0x-alpaca` call | Status |
| --- | --- | --- |
| `activities.list` | `AlpacaBrokerApi::fetch_account_activities(query, max_pages)` | Changed: the design named `AlpacaBrokerApiCtx::fetch_account_activities`, which builds a new client for each call and reads up to 1000 pages. The instance method reuses the gateway's client and takes the page cap from the caller's tier: 1000 pages for a bot, 10 for a human. More matching pages answer `AccountActivitiesPageLimitExceeded`. The account is the configured one; `types` is split on commas. |
| `orders.place_market`, `orders.place_limit`, `orders.place_exact_limit` | `AlpacaBrokerApi::{place_market_order_reporting, place_limit_order_reporting, place_alpaca_limit_order_reporting}` | Changed: the reporting forms of `place_market_order`, `place_limit_order` and `place_alpaca_limit_order`, with the same requests. Their `PlacementError::written` tells the handler whether the order POST may have been sent, so a failure before the POST, or a definite rejection of it, answers `not_applied`, and any later failure `outcome_unknown`. An exact limit price that does not convert (`AlpacaLimitOrder::try_from`) is refused before anything is sent. |
| `wallet.withdraw` | `AlpacaWalletService::check_withdrawal_whitelist`, then `AlpacaWalletService::submit_withdrawal` | Changed: the design named `initiate_withdrawal`, which is exactly these two calls in this order. The handler makes them separately so that a failed whitelist read or an address that is not approved answers `not_applied`, and only a failed POST can answer `outcome_unknown`. The gateway runs every operation but `corporate_actions.stream` under a `request_id::gated` send gate that closes when it stops waiting (the operation deadline or shutdown), so a POST or credential mint that has not started by then never starts. |
| `wallet.whitelist_remove`, `wallet.whitelist_patch_travel_rule` | `AlpacaWalletService::get_whitelisted_addresses`, then `delete_whitelist_entry` or `patch_whitelist_travel_rule` for each entry of the address | Changed: the design named `remove_whitelist_entries` and `patch_all_whitelist_travel_rules`. The handler runs the same list read, address filter, `NoWhitelistEntries` refusal, and one write per entry itself, so a failure names the entries already changed in its message. The PATCH carries the configured Travel Rule beneficiary, and the library method's `error!` event for a failed entry is not emitted. |
| `issuer.mint_callback`, `issuer.redeem` | `AlpacaClient::{send_mint_callback_reporting, call_redeem_endpoint_reporting}` | Changed: the reporting forms of `IssuerApi::{send_mint_callback, call_redeem_endpoint}`, with the same requests. Their `IssuerCallError::written` tells the handler whether a POST may have been sent, so a failure with nothing written answers `not_applied`, and any other `outcome_unknown`; `IssuerCallError::alpaca_status` is the answer's `alpacaStatus`. |
| `issuer.request` | `AlpacaClient::poll_request_status_reporting` | Changed: the reporting form of `IssuerApi::poll_request_status`, with the same request. A read is never written; `IssuerCallError::alpaca_status` is the answer's `alpacaStatus`. |
| `corporate_actions.stream` | `CorporateActionStreamClient::connect_raw` | Changed: the handler relays the checked response's bytes unchanged instead of decoding them with `connect`. |

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

The broker, wallet, and tokenization surfaces emit the same `tracing` events as the source, with the same targets (`broker`, `wallet`, `tokenization`) and fields, plus `warn` events for a failed read a poll retries: one `tokenization` event ("Tokenization lookup failed, polling again") each time a tokenization poll looks up again after a transient lookup error, and one `broker` event when the conversion poll reads again after a gateway hop failure the gateway marked retryable. The conversion poll's deadline event reads "Conversion order not seen terminal by the deadline; cancelling the remainder" instead of the source's "Conversion order still not terminal at the deadline; cancelling the remainder", and its `status` and `filled` fields come from the reads that answered before the deadline, because no read runs at the deadline. `tracing` is a facade: the crate installs no subscriber and no exporter, so consumers keep full control of instrumentation. The issuer surface emits no events.

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

Send gate: a request asked after the gate closed is not sent, on the broker, market data and tokenization clients; a token mint asked after waiting on another caller's failed mint, once the gate closed, never starts; a close racing a send on another worker returns only once the request that found the gate open is handed over, and the server sees that request and no later one.
