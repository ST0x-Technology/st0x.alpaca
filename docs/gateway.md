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
- Symbols in paths and bodies are restricted to letters, digits, `.`, `/` and `-`, at most 32 characters; anything else answers `400 invalid_request` before Alpaca is called. A journal `counterparty` is 1 to 64 letters, digits, `_` or `-`, and a wallet `asset` and a deposit address `network` carry at most 32 characters; a longer value is refused by its length alone.
- Client order ids must have the form of the caller's tier: a bare UUID for bots, `cli-` followed by a UUID for human writers. Bot `orders.recover`, `orders.find` and `conversions.find` accept only bot keys and answer `400 invalid_request` to a `cli-` key, so the bot never adopts a human order as its own. Humans may look up any key.
- `activities.list` needs a non blank `types`. A human call reads at most 10 pages from Alpaca (a bot call 1000); a longer history answers `400 invalid_request`, so narrow the `after` and `until` window.
- The read and write tiers share `human_budget_per_minute`, counted in calls: each human call takes one unit, except the keyed reads that client poll loops repeat (`orders.get`, `conversions.get`, `wallet.transfer`, `wallet.find_deposit`, `tokenization.request`, `tokenization.find_redemption`), which are free. A spent budget answers `429 backpressure` with `Retry-After`. Every `Retry-After` and `retryAfterSecs` is the hold rounded up to whole seconds, at least 1, so a caller that waits it out never retries early. Bots are never charged. Each gateway process keeps its own budget, and a rollout runs two side by side (see [Rollouts](#rollouts)).
- Every call runs its work on a detached task and answers by its deadline (`Operation::deadline`), or at once on shutdown; a deadline, shutdown or a caller that goes away never aborts a request already sent to Alpaca. The work runs under a send gate that closes at the deadline, at shutdown, and before the caller is answered without the result. The library asks the gate before each credential mint starts and as each Alpaca API request would leave, so a request is either in flight before that answer, which its `outcome_unknown` covers as one that may still land, or held back and never sent; a mutation that sent nothing settles `not_applied`. Without the result a mutation answers `504 outcome_unknown` and a read `502 upstream_transient`, which is retryable. `wallet.withdraw`, `wallet.whitelist_remove` and `wallet.whitelist_patch_travel_rule` read the whitelist before they write, and that read is cut at the deadline, so a stalled read ends the work with nothing written. A whitelist loop stopped after its first write answers `outcome_unknown` with a message naming the entries it already changed; read `wallet.whitelist` to see the current state. Detached work runs until its last Alpaca request answers: the wallet client has no total request timeout, so a wallet request Alpaca never answers holds its task until the process ends.

## Config

The service reads the TOML file named by `ST0X_ALPACA_GATEWAY_CONFIG` (the image sets `/run/t0-alpaca/gateway.toml`). Unknown keys are refused at the top level and in the gateway's own tables; `[broker]` is read with the library's `AlpacaBrokerApiCtx`, which ignores keys it does not know, so a misspelled key there takes the library default and the startup account check is what catches a wrong endpoint. `environment` is exactly `production` or `staging`; any other value, `prod` or `Production` included, fails to parse. With `environment = "production"`, or with `mode = "production"` in `[broker]` (the real money Broker API, whatever the environment), only the Cloud KMS credential (`client_id` and `kms_key_version`) passes validation, so no key material for the real money endpoint lives in the config; a staging deployment against the sandbox may also use `api_key` and `api_secret`. The bot, read and write audiences must differ. `identity.google_jwks_url` and `identity.iap_jwks_url` default to Google's key URLs; an override must be HTTPS, or HTTP on a loopback host. `st0x-alpaca-gateway --validate-config <path>` checks a file without contacting anything. A file that does not parse is reported with the parser's message, never the offending line itself, so a malformed credential line stays out of the startup log and the validation output.

```toml
environment = "staging"
listen = "0.0.0.0:8080"
# Startup refuses to serve unless Alpaca reports this number for broker.account_id.
expected_account_number = "<T0 account number>"
# Operations switched off without a redeploy of the image (403 capability_disabled).
disabled_operations = []
# Per process, and a rollout runs two: half the human share of the credential's rate limit.
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
# Empty: the bot's wallet.withdraw answers 403 capability_disabled and the write tier's keeps serving; disabled_operations switches wallet.withdraw off on the write tier too.
bot_withdrawal_destinations = ["<liquidity market maker wallet>"]
travel_rule_beneficiary = "<beneficiary entity name>"

[tokenization]
# Empty: every tokenization operation answers 422 unsupported_network or 403 capability_disabled.
networks = ["base"]
# Empty: every mint answers 403 destination_not_allowed.
mint_recipients = ["<liquidity bot wallet>"]

# Empty: journals.create answers 403 capability_disabled.
[journal.counterparties]
```

The account id, the account number and every pinned address live in this reviewed file and nowhere else.

## Startup and health

1. Parse and validate config. Invalid config exits nonzero.
2. Verify the account with Alpaca (`GET /v1/trading/accounts/{id}/account`), require status ACTIVE and the configured account number. Any failure exits nonzero, so Cloud Run never routes traffic to a gateway bound to the wrong account or unable to reach Alpaca.
3. Serve. `/healthz` and `/readyz` answer without credentials; `/readyz` reports environment and version.

On SIGTERM the listener stops taking requests, and one 8 second budget starts, inside Cloud Run's 10 second termination grace. Every call in flight answers at once without its result, as described under [Request rules](#request-rules), and a request arriving after the signal answers `503 unavailable` (`not_applied` for a mutation). The process then waits for the detached tasks until 8 seconds after the signal and exits. A detached task still running at that point is cut and leaves no `settled` record; callers reconcile from reads.

## Audit

Every request on a catalog operation's route writes one JSON line to stdout with `logging.googleapis.com/labels.log = "st0x_alpaca_gateway_audit"` and the event under `audit` (`st0x_alpaca_gateway_api::AuditEvent`). That includes a request the gateway refuses before any work: a credential that does not verify (caller subject `unverified`), a bot subject outside `identity.bot_principals`, and a body, path or query that does not parse; a refused mutation answers `not_applied`. A path no operation serves answers `404 unknown_operation` and writes no record. Fields: request id, deployment, environment, account id, caller subject and email, tier, `X-On-Behalf-Of`, operation, key, reason, request digest, money moving fields, Alpaca status (on a success the status of the last Alpaca API answer, on a failure the failure's own Alpaca status, none when it has none), `alpacaRequestIds` (the `X-Request-ID` of the Alpaca responses the work received, token mint answers included, in order, the join key with Alpaca support: the first 100, each cut at 128 characters), Alpaca object id (the order, transfer, journal, tokenization request or whitelist entries the call created or touched, comma joined; on a failure the entries a whitelist loop had already changed, or the tokenization request a network refusal is about), outcome, error code, latency and the gateway version, `<package version>+<commit>` (`unknown` in a build outside the flake). The key, the reason, each money moving field and `X-On-Behalf-Of` keep at most their first 256 characters, so one record stays one bounded log line whatever the caller sends; the request digest still covers the whole request. The detached task writes the record that carries the result and its Alpaca traffic: phase `answered` when the caller got that result, `settled` when the gateway had already answered without it (at the deadline or on shutdown) or the caller had gone away. An answer without the result writes its own `answered` record, without Alpaca traffic. The order of the two records is not guaranteed. Records never contain credentials, tokens, Travel Rule names or response bodies.

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
3. Deploy the digest with no traffic, check its startup log line `Gateway bound to its Alpaca account`, then move all traffic to it at once and check `/readyz` (see [Rollouts](#rollouts)).
4. From the staging liquidity VM, call `GET /bot/v1/account/funds` with the VM's ID token; from a laptop, call `/alpaca-read/v1/account/funds` through the load balancer. Both write audit records.

## Deploying to production

Same steps in `t0-alpaca`, with the production BrokerDash credential and account, behind the app deploy PAM grant like every other T0 service. The production config sets `environment = "production"` and must use the Cloud KMS credential; validation refuses any other shape. Production `bot_principals` lists only the production liquidity runtime service account.

## Rollouts

Min and max instances 1 does not make the gateway a single process. Cloud Run caps instances per revision, so the old and the new revision run side by side while traffic moves and while the old one finishes the requests it already admitted (up to the 300 s request timeout) and their detached work; a platform replacement of the instance overlaps two processes the same way. Each process keeps its own human budget and its own `Retry-After` hold after a throttled credential mint, so a hold one process takes does not stop the other, and together they can admit twice `human_budget_per_minute` in one minute. Two rules keep the humans within their share of the credential's Alpaca rate limit:

1. Set `human_budget_per_minute` to half the intended human share, so two processes together stay within it.
2. Deploy every revision (a new digest, a new config version, a rollback) with no traffic, then move all traffic to it at once. A revision without traffic admits nothing, so the overlap lasts only while the old revision drains. Never split traffic between two revisions: both keep admitting for as long as the split lasts. A config roll runs the same two commands with `--update-secrets` in place of `--image`.

```bash
gcloud run services update t0-alpaca --project <project> --region <region> --image <digest> --no-traffic
# Check the new revision's startup log line `Gateway bound to its Alpaca account`.
gcloud run services update-traffic t0-alpaca --project <project> --region <region> --to-latest
```

## Rollback

| Problem | Action | Effect |
| --- | --- | --- |
| A bad gateway release | Redeploy the previous attested digest as in [Rollouts](#rollouts) | Callers see the previous behavior; the `/v1` contract only ever adds operations and optional response fields |
| One operation misbehaves | Add it to `disabled_operations`, publish the config, roll the revision | That operation answers `403 capability_disabled` on every tier; the rest keep serving |
| The bot must stop withdrawing while operators keep withdrawing | Empty `wallet.bot_withdrawal_destinations`, publish the config, roll the revision | The bot's `wallet.withdraw` answers `403 capability_disabled`; operator withdrawals and every other bot operation keep serving |
| A caller must lose access | Remove its `run.invoker` binding (bots) or group membership (humans) | Its requests stop at the platform |
| All human access must stop | Remove the IAP service agent's `run.invoker` binding | Every human request stops; bots keep serving |
| The credential is suspect | Revoke the BrokerDash credential | All Alpaca traffic for the account stops at Alpaca |
| The gateway is unusable | No caller depends on the gateway yet: the bots still call Alpaca directly with their own credential, so stopping or rolling back the gateway affects only human callers | Once the bots ship the `[alpaca] transport` switch ([RAI-1935](https://linear.app/makeitrain/issue/RAI-1935)), the rollback becomes: set the bots back to the direct transport. Their direct credential stays provisioned until that switch has been tested, and [RAI-1938](https://linear.app/makeitrain/issue/RAI-1938) removes it only after that |

The gateway never undoes an Alpaca action by itself. Undoing an order, a conversion or a transfer is the caller's decision, through the same operations.
