# st0x.alpaca

Shared Alpaca transport and the issuer API surface used by st0x.issuance. The
st0x.liquidity broker, wallet, and market-data extraction is deferred until its
existing endpoint and recovery contracts have parity tests.

| Feature  | Surface                                                                       | Consumer      |
| -------- | ----------------------------------------------------------------------------- | ------------- |
| `issuer` | ITN callbacks and polling across Base, Ethereum, HyperEVM, Robinhood, and BNB | st0x.issuance |

```toml
[dependencies]
st0x-alpaca = { git = "ssh://git@github.com/ST0x-Technology/st0x.alpaca", features = ["issuer"] }
```

## Design

- **Shared transport (`core`)**: `AlpacaClient` carries the base URL, account
  id, exponential-backoff retry policy, and one of three authentication modes:
  legacy Basic/APCA headers, a Cloud KMS-backed `private_key_jwt`, or a local
  P-256 private-key JWT. `AlpacaError` preserves rate-limit backpressure and
  classifies transient versus permanent failures; surface modules add their own
  invariant variants on top.
- **Issuer finance types**: validated symbols and share quantities use the
  released `st0x-finance` `v0.2.0` contract. Redeem requests preserve the
  issuer's decimal spelling at the wire boundary.
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

The `issuer` feature must also compile standalone
(`cargo check --no-default-features --features issuer`).
