# st0x.alpaca

Shared Alpaca client library for st0x services. One transport core plus
feature-gated API surfaces, so each consumer compiles only the endpoints it
uses:

| Feature       | Surface                                                                                         | Consumer       |
| ------------- | ----------------------------------------------------------------------------------------------- | -------------- |
| `issuer`      | ITN issuer callbacks: mint callback, redeem, request polling                                    | st0x.issuance  |
| `broker`      | Broker API trading: orders, conversions, journals, positions, account, activities, market hours | st0x.liquidity |
| `wallet`      | Crypto wallet: transfers, whitelists, wallet addresses, status polling                          | st0x.liquidity |
| `market-data` | Latest-trade price lookups                                                                      | st0x.liquidity |

```toml
[dependencies]
st0x-alpaca = { git = "ssh://git@github.com/ST0x-Technology/st0x.alpaca", features = ["broker", "wallet"] }
```

## Design

- **Shared transport (`core`)**: `AlpacaClient` carries the base URL, account
  id, Alpaca's dual authentication (HTTP Basic plus the `APCA-API-KEY-ID` /
  `APCA-API-SECRET-KEY` headers), and the exponential-backoff retry policy.
  `AlpacaError` is the shared error taxonomy; surface modules add their own
  invariant variants on top.
- **Shared finance types**: validated symbols, equity quantities, USD values,
  and USDC amounts use `st0x-finance` while preserving Alpaca's JSON
  string-or-number decoding. Asset-dependent position quantities remain wire
  decimals because one position endpoint returns equities, options, crypto, and
  USDC rows.
- **Telemetry-free**: no `tracing` dependency; consumers wrap calls with their
  own instrumentation.

## Development

```bash
nix develop
cargo check --all-features
cargo test --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
```

Every feature must also compile standalone
(`cargo check --no-default-features --features <feature>`).
