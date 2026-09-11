# Threat model

This library is the typed boundary between st0x services and Alpaca. It does not
own balances or execute business workflows, but incorrect wire values can cause
orders, journals, redemptions, or wallet transfers with the wrong asset or
amount.

## Trust boundaries and assets

- Consumer domain values cross into JSON requests sent to Alpaca.
- Alpaca JSON responses cross into validated Rust domain types.
- Credentials cross into HTTP authentication headers; credential handling is
  unchanged by the finance-type migration.

The protected assets are equity quantities, USD balances and prices, USDC
conversion and transfer amounts, symbol routing, idempotency identifiers, and
API credentials.

## Threats and controls

| Threat | Concrete risk | Control |
| --- | --- | --- |
| Spoofing | A request is sent without the configured Alpaca identity. | Broker, issuer, and wallet calls use the shared authenticated transport; the market-data client builder applies its documented APCA headers. |
| Tampering | Blank symbols, cross-domain primitive amounts, or non-positive USDC conversions are accepted. | Deserialize and expose st0x-finance Symbol, FractionalShares, Usd, and Usdc values at the boundary, reject invalid conversion amounts, and retain asset-dependent decimals only for heterogeneous position rows. |
| Repudiation | A retried mutation cannot be reconciled. | Preserve caller-supplied idempotency identifiers and keyed lookup behavior. Telemetry remains the consumer's responsibility. |
| Information disclosure | Credentials or Travel Rule identity appear in diagnostics. | Keep credential Debug output redacted and preserve wallet beneficiary redaction. |
| Denial of service | Malformed external values panic or retry forever. | Return typed parse errors, keep retries limited to classified transient failures, and retain bounded pagination and polling. |
| Elevation of privilege | A feature surface gains access to unrelated integrations. | Keep feature flags additive and dependencies isolated; the library exposes no authorization or administrative operations. |

## Required evidence

- Real HTTP response paths reject blank symbols before values reach consumers.
- Requests and responses preserve Alpaca's existing string encodings without
  rounding or coercion.
- Each feature builds independently and the full test suite passes.
- Dependency audit and staged-diff review find no new secret exposure.
