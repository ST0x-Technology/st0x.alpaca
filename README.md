# st0x.alpaca

Shared Alpaca client library for st0x services. It holds every Alpaca API
integration that st0x.issuance and st0x.liquidity use, with the behavior and
tests of the consumer code it replaces. [docs/parity.md](docs/parity.md) maps
each source item to its counterpart here and lists every intentional
difference.

| Feature        | Surface                                                                                                                       | Consumer       |
| -------------- | ----------------------------------------------------------------------------------------------------------------------------- | -------------- |
| `issuer`       | ITN mint callback, redeem initiation with the network preflight, keyed request polling                                        | st0x.issuance  |
| `corporate-actions` | Corporate-action SSE stream: endpoint validation, authenticated replay requests, bounded SSE decoder, wire identities | st0x.issuance |
| `broker`       | Broker API account, assets, equity orders, USD/USDC conversion, positions, journals, account activities, market hours, quotes | st0x.liquidity |
| `wallet`       | Crypto wallet deposit addresses, USDC withdrawals and deposits, transfer polling, withdrawal whitelists                       | st0x.liquidity |
| `tokenization` | Mint requests, request history lookups, redemption detection, terminal-state polling                                          | st0x.liquidity |
| `mock`         | Stateful httpmock broker, wallet, and tokenization servers for end-to-end suites                                              | st0x.liquidity |
| `test-support` | Test-only constructors (`AlpacaWalletClient`, `AlpacaApiErrorMessage::for_test`, request mocks)                               | both           |

```toml
[dependencies]
st0x-alpaca = { git = "ssh://git@github.com/ST0x-Technology/st0x.alpaca", features = ["broker", "wallet", "tokenization"] }
```

## Design

- **Authentication (`core::AlpacaAuth`)**: legacy Basic/APCA headers, a Cloud
  KMS-backed `private_key_jwt`, or a local P-256 private-key JWT. The token
  cache refreshes early and rides a still-valid token when a refresh fails.
- **Credential-bearing URLs**: base and token URLs must be HTTPS (plain HTTP
  only on loopback) without embedded credentials, query, or fragment. The
  corporate-action stream accepts only `stream.data.alpaca.markets` (its
  query carries the stream filter). Issuer
  request URLs are built from percent-encoded path segments on the
  configured origin.
  No client follows redirects.
- **Consumer boundary**: Alpaca wire types, validation, retries, polling, and
  error classification live here. Consumer traits (liquidity's `Executor` and
  `Tokenizer`), onchain actions, and trading policy (preflight sizing,
  slippage, hedge floor) stay in the consumer.
- **Finance types**: quantities, prices, fees, and amounts use the Rain
  Float-backed `st0x-finance` `v0.3.0` types. The issuer redeem request keeps
  the caller's exact quantity spelling on the wire.
- **Telemetry**: the broker, wallet, and tokenization surfaces emit the same
  `tracing` events as the code they replace. The crate installs no
  subscriber. The issuer surface emits no events.

## Development

```bash
nix develop
cargo check --all-features
cargo test --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
```

Each feature must also compile standalone, for example
`cargo check --no-default-features --features broker`.
