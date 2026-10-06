# Alpaca gateway (`t0-alpaca`)

`st0x-alpaca-gateway` holds one Alpaca account's credential and serves the operation catalog of `st0x-alpaca-gateway-api` to bots and operators. Every operation runs bounded `st0x-alpaca` calls against the account fixed in config; [docs/parity.md](parity.md#gateway-st0x-alpaca-gateway) lists the library methods each operation runs. No request names an account or an Alpaca URL, the gateway signs nothing onchain, and it keeps no state.

The contract (catalog, capability matrix, error, idempotency, audit and rollback rules) is [RAI-1927](https://linear.app/makeitrain/issue/RAI-1927). This page covers running it.

## Crates

| Crate | What it is | Who links it |
| --- | --- | --- |
| `st0x-alpaca-gateway-api` | Tiers, operation catalog with the capability matrix, request and response bodies, error body, audit event. Feature `client` adds the typed HTTP client. | Bots, operator tools, the gateway |
| `st0x-alpaca-gateway` | The Axum service | The deployed image only |

## Tiers and identity

| Tier | Prefix | Caller | Check in the app |
| --- | --- | --- | --- |
| bot | `/bot/v1` | Bot runtime service accounts, with a Google ID token from the metadata server (`format=full`) sent as `Authorization: Bearer` | Signature against Google's keys, issuer, audience `identity.bot_audience`, and the token's `sub` in `identity.bot_principals` |
| read | `/alpaca-read/v1` | Humans in the readers and admins groups, through IAP | `x-goog-iap-jwt-assertion`, audience pinned to `identity.read_audience` |
| write | `/alpaca-write/v1` | Humans in the admins group, through IAP | `x-goog-iap-jwt-assertion`, audience pinned to `identity.write_audience` |

Outside the app, Cloud Run `run.invoker` is granted only to the bot service accounts and to the project's IAP service agent, and the load balancer routes only the two human prefixes. An operation is mounted on a tier only if the capability matrix in `crates/gateway-api/src/ops.rs` lists it; any other path answers `404 unknown_operation`. Human mutations need a non blank `reason` in the body.

## Request rules

- Request bodies and queries refuse unknown fields.
- Symbols in paths and bodies are restricted to letters, digits, `.`, `/` and `-`; anything else answers `400 invalid_request` before Alpaca is called.
- Client order ids must have the form of the caller's tier: a bare UUID for bots, `cli-` followed by a UUID for human writers. Bot `orders.recover`, `orders.find` and `conversions.find` accept only bot keys and answer `400 invalid_request` to a `cli-` key, so the bot never adopts a human order as its own. Humans may look up any key.
- `activities.list` needs a non blank `types`. A human call reads at most 10 pages from Alpaca (a bot call 1000); a longer history answers `400 invalid_request`, so narrow the `after` and `until` window.
- The read and write tiers share `human_budget_per_minute`, counted in requests sent to Alpaca (the token mint is not counted). Admission reserves the most requests the operation can send (`Operation::human_budget_cost`, for example 2 for `wallet.withdraw` and 4 for an order placement); a cache hit can make it send fewer, never more. Config validation refuses a `human_budget_per_minute` below the largest reservation of an operation the human tiers may call (10, for `activities.list`, unless it is in `disabled_operations`) and names that operation. When the work ends (for a read, the end of its work or the moment its caller goes away; for a mutation, the end of its work at Alpaca, even after a deadline answer), the gateway keeps one unit per request sent and gives the rest back, whatever the outcome, so a request refused before anything was sent gets all its units back. Units go back only while the one minute window they were taken from is still current. A human `activities.list` reserves 10 units, its page cap. The keyed reads that client poll loops repeat (`orders.get`, `conversions.get`, `wallet.transfer`, `wallet.find_transfer`, `tokenization.request`, `tokenization.find_redemption`) are free. `wallet.whitelist_remove` and `wallet.whitelist_patch_travel_rule` reserve one unit for the whitelist read and, after it, one unit per entry they will write, all at once before the first write. A budget that cannot cover every write answers `429 backpressure` with `not_applied` and writes nothing; when the writes plus the read exceed `human_budget_per_minute` that answer is not retryable and carries no `Retry-After`. If the loop stops early, the units of the writes it never sent go back. A spent budget answers `429 backpressure` with `Retry-After`. Every `Retry-After` and `retryAfterSecs` is the hold rounded up to whole seconds, at least 1, so a caller that waits it out never retries early. Bots are never charged.

## Config

The service reads the TOML file named by `ST0X_ALPACA_GATEWAY_CONFIG` (the image sets `/run/t0-alpaca/gateway.toml`). Unknown keys are refused at every level, including inside `[broker]`, so a misspelled `mode` or auth field fails validation instead of falling back to a default. `[broker]` takes exactly one credential shape: `api_key` and `api_secret` (Basic), `client_id` and `kms_key_version` (Cloud KMS), or `client_id` and `private_key_pem` (local key). `environment` is exactly `production` or `staging`; any other value, `prod` or `Production` included, fails to parse. With `environment = "production"` only the Cloud KMS shape passes validation, so no production key material lives in the config. `identity.google_jwks_url` and `identity.iap_jwks_url` default to Google's key URLs; an override must be HTTPS, or HTTP on a loopback host. `st0x-alpaca-gateway --validate-config <path>` checks a file without contacting anything.

```toml
profile = "t0"
environment = "staging"
listen = "0.0.0.0:8080"
# Startup refuses to serve unless Alpaca reports this number for broker.account_id.
expected_account_number = "<T0 account number>"
# Operations switched off without a redeploy of the image (403 capability_disabled).
disabled_operations = []
human_budget_per_minute = 60

[broker]
# Keyless: Cloud KMS signs the private_key_jwt assertion with the runtime service account.
client_id = "<BrokerDash client id>"
kms_key_version = "projects/<project>/locations/<region>/keyRings/<ring>/cryptoKeys/alpaca-api-key/cryptoKeyVersions/1"
account_id = "<T0 account id>"
mode = "production"

[identity]
bot_audience = "<Cloud Run service URL>"
bot_principals = ["<unique id of t0-liquidity@t0-liquidity>"]
read_audience = "/projects/<number>/global/backendServices/<read backend id>"
write_audience = "/projects/<number>/global/backendServices/<write backend id>"

[wallet]
bot_withdrawal_destinations = ["<liquidity market maker wallet>"]
travel_rule_beneficiary = "<beneficiary entity name>"

[tokenization]
networks = ["base"]
mint_recipients = ["<liquidity bot wallet>"]

# Empty: journals.create answers 403 capability_disabled.
[journal.counterparties]
```

The account id, the account number and every pinned address live in this reviewed file and nowhere else.

## Startup and health

1. Parse and validate config. Invalid config exits nonzero.
2. Verify the account with Alpaca (`GET /v1/trading/accounts/{id}/account`), require status ACTIVE and the configured account number. Any failure exits nonzero, so Cloud Run never routes traffic to a gateway bound to the wrong account or unable to reach Alpaca.
3. Serve. `/healthz` and `/readyz` answer without credentials; `/readyz` reports profile, environment and version.

On SIGTERM the listener stops taking requests, and one 8 second budget starts, inside Cloud Run's 10 second termination grace. Every mutation still waiting for Alpaca answers its caller `504 outcome_unknown` at once and writes its `answered` audit record; its work at Alpaca continues as a detached task. A read in flight answers `502 upstream_transient`, which is retryable. A request arriving after the signal answers `503 unavailable` (`not_applied` for a mutation). The process then waits for the detached tasks until 8 seconds after the signal and exits. A detached task still running at that point is cut and leaves no `settled` record; callers reconcile from reads.

## Audit

Every request on a catalog operation's route writes one JSON line to stdout with `logging.googleapis.com/labels.log = "st0x_alpaca_gateway_audit"` and the event under `audit` (`st0x_alpaca_gateway_api::AuditEvent`). That includes a request the gateway refuses before any work: a credential that does not verify (caller subject `unverified`), a bot subject outside `identity.bot_principals`, and a body, path or query that does not parse; a refused mutation answers `not_applied`. A path no operation serves answers `404 unknown_operation` and writes no record. Fields: request id, deployment, environment, account id, caller subject and email, tier, `X-On-Behalf-Of`, operation, key, reason, request digest, money moving fields, Alpaca status (a failure's own Alpaca status, otherwise the status of the last Alpaca answer, so a body that does not parse after a 200 still records the 200), `alpacaRequestIds` (the `X-Request-ID` of every Alpaca response the request received, in order, which is the join key with Alpaca support), Alpaca object id (on a failure, the error body's `alpacaObjectIds` comma joined: the entries a whitelist loop had already changed, or the tokenization request a network refusal is about), outcome, error code, latency, `abandoned`, and the gateway version. The version is the package version, the commit the image was built from (the flake passes it at build time; `unknown` in a build outside the flake) and, under Cloud Run, the revision serving the request (`K_REVISION`), as `0.1.0+<commit>@<revision>`. A read whose caller goes away before the answer still writes its `answered` record, with `abandoned: true` and the Alpaca traffic sent until then. A mutation that finishes after its answer was sent writes a second record with phase `settled`. Records never contain credentials, tokens, Travel Rule names or response bodies.

Query the audit stream in Cloud Logging with `labels.log="st0x_alpaca_gateway_audit"`.

## Image

```bash
nix build .#gateway-oci
./result | docker load    # t0-alpaca:latest, entrypoint /bin/st0x-alpaca-gateway
```

The image has no base layer and a pinned creation time, so a commit always rebuilds to the same digest. `gateway-oci` and `st0x-alpaca-gateway` are flake outputs on Linux only, so on a Mac the image needs a Linux builder (`nix build .#packages.x86_64-linux.gateway-oci`). CI builds the image on every pull request.

## Deploying to staging

The `t0-alpaca-staging` project in the t0trade.com org needs, in t0.devops:

1. A runtime service account `t0-alpaca` and a KMS key `alpaca-api-key` (EC P-256) with that account as the only `signerVerifier`, with KMS Data Access audit logging on.
2. A BrokerDash credential registered against that key's public half, scoped to the staging account, with the narrowest scopes BrokerDash offers for the T0 matrix.
3. Cloud Run service `t0-alpaca`: the attested image digest, min and max instances 1, CPU always allocated, ingress all, request timeout 300 s, the config mounted from Secret Manager at `/run/t0-alpaca/gateway.toml`.
4. `run.invoker` for the staging liquidity runtime service account and the project's IAP service agent only.
5. An external HTTPS load balancer with a serverless NEG and two backends, `/alpaca-read/*` and `/alpaca-write/*`, each with IAP and its own group, backend timeout 300 s, request logging at sample rate 1.0. Their backend ids are the two audiences in config.
6. The project sink to `aggregated-logs`, and the `audit_trail` module with an extra filter for `labels.log="st0x_alpaca_gateway_audit"` so mutation records reach `t0-audit-trail`.

Then:

1. Build and push the image to `europe-west3-docker.pkg.dev/t0-artifacts/t0-alpaca/t0-alpaca` and sign its Binary Authorization attestation.
2. Validate and publish the staging config: `docker run --rm -v $PWD/staging.toml:/candidate.toml t0-alpaca:latest --validate-config /candidate.toml`, then add it as a new Secret Manager version.
3. Deploy the digest. Check the startup log line `Gateway bound to its Alpaca account` and `/readyz`.
4. From the staging liquidity VM, call `GET /bot/v1/account/funds` with the VM's ID token; from a laptop, call `/alpaca-read/v1/account/funds` through the load balancer. Both write audit records.

## Deploying to production

Same steps in `t0-alpaca`, with the production BrokerDash credential and account, behind the app deploy PAM grant like every other T0 service. The production config sets `environment = "production"` and must use the Cloud KMS credential; validation refuses any other shape. Production `bot_principals` lists only the production liquidity runtime service account.

## Rollback

| Problem | Action | Effect |
| --- | --- | --- |
| A bad gateway release | Redeploy the previous attested digest | Callers see the previous behavior; the `/v1` contract only ever adds operations and optional response fields |
| One operation misbehaves | Add it to `disabled_operations`, publish the config, roll the revision | That operation answers `403 capability_disabled`; the rest keep serving |
| A caller must lose access | Remove its `run.invoker` binding (bots) or group membership (humans) | Its requests stop at the platform |
| All human access must stop | Remove the IAP service agent's `run.invoker` binding | Every human request stops; bots keep serving |
| The credential is suspect | Revoke the BrokerDash credential | All Alpaca traffic for the account stops at Alpaca |
| The gateway is unusable | No caller depends on the gateway yet: the bots still call Alpaca directly with their own credential, so stopping or rolling back the gateway affects only human callers | Once the bots ship the `[alpaca] transport` switch ([RAI-1935](https://linear.app/makeitrain/issue/RAI-1935)), the rollback becomes: set the bots back to the direct transport. Their direct credential stays provisioned until that switch has been tested, and [RAI-1938](https://linear.app/makeitrain/issue/RAI-1938) removes it only after that |

The gateway never undoes an Alpaca action by itself. Undoing an order, a conversion or a transfer is the caller's decision, through the same operations.
