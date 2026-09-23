# Threat model

This library is the typed boundary between st0x services and Alpaca. It does
not own balances or run business workflows, but a wrong wire value can place
an order, conversion, journal, withdrawal, mint, or redemption with the wrong
asset or amount, and a leaked credential can do the same directly.

## Trust boundaries and assets

- Consumer domain values cross into JSON requests sent to Alpaca.
- Alpaca JSON responses cross into validated Rust types.
- Credentials (Basic key pair, KMS-signed or locally signed JWT assertions,
  bearer tokens) cross into HTTP headers.
- The `mock` feature runs local HTTP servers and, for the tokenization mock,
  sends onchain ERC-20 transfers from a test wallet. It is for test suites
  only.

The protected assets are equity quantities, USD cash and prices, USDC
conversion and transfer amounts, symbol and network routing, idempotency
keys, Travel Rule beneficiary identity, and API credentials.

## Threats and controls

| Threat | Concrete risk | Control (where) |
| --- | --- | --- |
| Spoofing | A request is sent without the configured identity, or credentials reach a host the operator did not configure. | Every client attaches credentials through the crate-private `AuthRuntime` (`auth.rs`). Base and token URLs must be HTTPS, or HTTP on a loopback host, with no embedded credentials, query, or fragment (`endpoint.rs`; broker, wallet, and tokenization clients; `AuthRuntime::build`). The corporate-action stream sends credentials only to `stream.data.alpaca.markets` over HTTPS; a development loopback endpoint is credential-free. Issuer request URLs are built on the configured origin from percent-encoded path segments, and empty or dot segments are rejected, so an identifier cannot redirect a credentialed request to another host or endpoint (`endpoint::resolve_segments`). The issuer, broker, wallet, tokenization, corporate-action stream, and token-mint HTTP clients do not follow redirects. `KmsJwtAuth` and `AuthRuntime` cannot be built with arbitrary URLs or HTTP clients from outside the crate. |
| Tampering | Invalid amounts or routing values reach Alpaca, or malformed responses reach consumers. | Order quantities and journal quantities are `Positive<FractionalShares>`; limit prices use `AlpacaLimitPrice` precision rules; withdrawals take `Positive<Usdc>`; USD-to-USDC buys use a whole-cent `notional` and USDC sells a 6-decimal `qty`. Alpaca crypto amounts keep the raw value for cash valuation and expose only the value floored to 6 decimals (`AlpacaAmount`). Responses are checked for symbol mismatch (positions, quotes), required fields per order status (`IncompleteOrder`), filled-quantity mismatch, crossed or non-positive quotes, calendar date mismatch, network mismatch on tokenization requests, and response id mismatch on issuer polls. The issuer redeem call refuses a network that is not a published ITN value before any HTTP call. |
| Repudiation | A retried mutation cannot be reconciled with what Alpaca executed. | Caller-supplied idempotency keys are sent unchanged: `client_order_id` for orders and conversions, `Idempotency-Key` and `client_request_id` for mints. A duplicate `client_order_id` adopts the existing order; keyed lookups (`orders:by_client_order_id`, mint recovery by issuer request id, redemption by tx hash) recover a lost response. A stalled conversion reports how the cancel was answered instead of claiming a result. |
| Information disclosure | Credentials, tokens, private keys, or Travel Rule identity appear in logs or errors. | `Debug` for `AlpacaAuth`, `AlpacaBrokerApiCtx`, the clients, the token cache, and the signer redacts secrets; credential header values are marked sensitive. Wallet response bodies have `beneficiary_entity_name` redacted in trace logs and `ApiError` messages, and a body that cannot be redacted is omitted (fail closed). Error bodies from the token endpoint are truncated and never contain tokens. Note: trace-level logs include full broker and market-data response bodies, which contain account balances; consumers must treat trace logs as sensitive. |
| Denial of service | A malformed value panics, or a retry or poll never ends. | No `unwrap` or `expect` outside tests (Clippy denies them). Retries are limited to classified transient errors: the issuer policy retries at most 5 times with jittered backoff; wallet polling retries 5xx at most 10 times. The SSE decoder caps a frame at 64 KiB and releases the buffer on a poison frame. Every poll loop is bounded: conversion orders (300 s, then cancel and a 30 s settle window), wallet transfers (30 min), tokenization requests (`PollingConfig` timeout), account activities (1000 pages and a repeated-token check). Broker, market-data, tokenization, issuer, and token-mint requests have connect and request timeouts. Known gap: wallet HTTP requests have no timeout (unchanged from the source, see `docs/parity.md`), so one hung wallet request can stall its caller past the poll deadline. Rate limits surface as `Backpressure` with the `Retry-After` hint so the consumer can back off. |
| Elevation of privilege | A feature surface gains access to unrelated integrations. | Features are additive and each compiles alone. The crate exposes no account administration, and the wallet surface can only withdraw to an address that Alpaca reports as an approved whitelist entry for the asset. |

## Required evidence

- Request bodies preserve Alpaca's string encodings without rounding: exact
  issuer redeem `qty`, order `qty` and `notional`, journal `qty`, withdrawal
  amount (unit tests in each module).
- Credential tests: insecure base and token URLs rejected for every client,
  URL syntax in path segments encoded, redirects not followed (including the
  token mint).
- Each feature builds independently and the full test suite passes
  (`.github/workflows/ci.yaml`).
- `docs/parity.md` shows no silent behavior change against the consumer code.
