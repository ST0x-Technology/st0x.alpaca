# st0x.alpaca

Shared Alpaca client library for st0x services. One transport core plus
feature-gated API surfaces, so each consumer compiles only the endpoints it
uses:

| Feature       | Surface                                                                          | Consumer       |
| ------------- | -------------------------------------------------------------------------------- | -------------- |
| `issuer`      | ITN callbacks and polling across Base, Ethereum, HyperEVM, Robinhood, and BNB    | st0x.issuance  |
| `broker`      | Trading, assets, account activity, and regular/extended/overnight session policy | st0x.liquidity |
| `wallet`      | Transfers, deposit lookup, whitelists, wallet addresses, and status polling      | st0x.liquidity |
| `market-data` | Latest trades, delayed SIP quotes, and timestamped overnight indicative quotes   | st0x.liquidity |

```toml
[dependencies]
st0x-alpaca = { git = "ssh://git@github.com/ST0x-Technology/st0x.alpaca", features = ["broker", "wallet"] }
```

## Design

- **Shared transport (`core`)**: `AlpacaClient` carries the base URL, account
  id, exponential-backoff retry policy, and one of three authentication modes:
  legacy Basic/APCA headers, a Cloud KMS-backed `private_key_jwt`, or a local
  P-256 private-key JWT. `AlpacaError` preserves rate-limit backpressure and
  classifies transient versus permanent failures; surface modules add their own
  invariant variants on top.
- **Shared finance types**: validated symbols, equity quantities, USD values,
  and USDC amounts use `st0x-finance` while preserving Alpaca's JSON
  string-or-number decoding. Asset-dependent position quantities remain wire
  decimals because one position endpoint returns equities, options, crypto, and
  USDC rows. The dependency is pinned to the released `st0x-finance` `v0.2.0`
  contract rather than an unreleased commit.
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
