# Alpaca gateway (`t0-alpaca`, `s01-alpaca`)

`st0x-alpaca-gateway` holds one Alpaca account's credential and serves the operation catalog of `st0x-alpaca-gateway-api` to bots and operators. Every operation runs bounded `st0x-alpaca` calls against the account fixed in config; [docs/parity.md](parity.md#gateway-st0x-alpaca-gateway) lists the library methods each operation runs. No request names an account or an Alpaca URL, the gateway signs nothing onchain, and it keeps no state.

The contract (catalog, capability matrix, error, idempotency, audit and rollback rules) is [RAI-1927](https://linear.app/makeitrain/issue/RAI-1927). This page covers running it.

## Crates

| Crate | What it is | Who links it |
| --- | --- | --- |
| `st0x-alpaca-gateway-api` | Tiers, operation catalog with the capability matrix, request and response bodies, error body, audit event. Feature `client` adds the typed HTTP client. | Bots, operator tools, the gateway |
| `st0x-alpaca-gateway` | The Axum service | The deployed image only |

## Profiles

One image serves two deployments, each bound to its own account. `profile` in config picks the deployment: it names it in every audit record and selects its capability matrix (`Operation::tiers` in `crates/gateway-api/src/ops.rs`).

| Profile | Deployment | Bot tier | Read and write tiers |
| --- | --- | --- | --- |
| `t0` | `t0-alpaca` | The liquidity runtime's operations | Operator reads and writes |
| `s01` | `s01-alpaca` | The issuance runtime's operations only: `issuer.mint_callback`, `issuer.redeem`, `issuer.request` and `corporate_actions.stream` | The same operations as on `t0`, plus `issuer.request` |

`t0` serves none of the four issuer and stream operations on any tier.

## Tiers and identity

| Tier | Prefix | Caller | Check in the app |
| --- | --- | --- | --- |
| bot | `/bot/v1` | Bot runtime service accounts, with a Google ID token from the metadata server (`format=full`) sent as `Authorization: Bearer` | Signature against Google's keys, issuer, audience `identity.bot_audience`, and the token's `sub` in `identity.bot_principals` |
| read | `/alpaca-read/v1` | Humans in the readers and admins groups, through IAP | `x-goog-iap-jwt-assertion`, audience pinned to `identity.read_audience` |
| write | `/alpaca-write/v1` | Humans in the admins group, through IAP | `x-goog-iap-jwt-assertion`, audience pinned to `identity.write_audience` |

Outside the app, Cloud Run `run.invoker` is granted only to the bot service accounts and to the project's IAP service agent, and the load balancer routes only the two human prefixes. An operation is mounted on a tier only if the profile's capability matrix lists it; any other path answers `404 unknown_operation`. Human mutations need a non blank `reason` in the body.

## Request rules

- Request bodies and queries refuse unknown fields.
- Symbols in paths and bodies are restricted to letters, digits, `.`, `/` and `-`, at most 32 characters; anything else answers `400 invalid_request` before Alpaca is called. A journal `counterparty` is 1 to 64 letters, digits, `_` or `-`, and a wallet `asset` and a deposit address `network` carry at most 32 characters; a longer value is refused by its length alone. An issuer `tokenizationRequestId` (in a body or the `issuer.request` path) or `issuerRequestId` is non blank, contains no control character, and has at most 128 characters. A redeem `quantity` must be above zero and use plain decimal notation with at most 9 fractional digits.
- Client order ids must have the form of the caller's tier: a bare UUID for bots, `cli-` followed by a UUID for human writers. Bot `orders.recover`, `orders.find` and `conversions.find` accept only bot keys and answer `400 invalid_request` to a `cli-` key, so the bot never adopts a human order as its own. Humans may look up any key.
- `activities.list` needs a non blank `types`. A human call reads at most 10 pages from Alpaca (a bot call 1000); a longer history answers `400 invalid_request`, so narrow the `after` and `until` window.
- The read and write tiers share `human_budget_per_minute`, counted in calls: each human call takes one unit, except the keyed reads that client poll loops repeat (`orders.get`, `conversions.get`, `wallet.transfer`, `wallet.find_deposit`, `tokenization.request`, `tokenization.find_redemption`, `issuer.request`), which are free. A spent budget answers `429 backpressure` with `Retry-After`. Every `Retry-After` and `retryAfterSecs` is the hold rounded up to whole seconds, at least 1, so a caller that waits it out never retries early. Bots are never charged. Each gateway process keeps its own budget, and a rollout runs two side by side (see [Rollouts](#rollouts)).
- Every call except `corporate_actions.stream` runs its work on a detached task and answers by its deadline (`Operation::deadline`), or at once on shutdown; a deadline, shutdown or a caller that goes away never aborts a request already sent to Alpaca. The work runs under a send gate that closes at the deadline, at shutdown, and before the caller is answered without the result. The library asks the gate before each credential mint starts and as each Alpaca API request would leave, so a request is either in flight before that answer, which its `outcome_unknown` covers as one that may still land, or held back and never sent; a mutation that sent nothing settles `not_applied`. Without the result a mutation answers `504 outcome_unknown` and a read `502 upstream_transient`, which is retryable. `wallet.withdraw`, `wallet.whitelist_remove` and `wallet.whitelist_patch_travel_rule` read the whitelist before they write, and that read is cut at the deadline, so a stalled read ends the work with nothing written. A whitelist loop stopped after its first write answers `outcome_unknown` with a message naming the entries it already changed; read `wallet.whitelist` to see the current state. Broker API requests and wallet requests built by the gateway each have a 30 second total timeout. After `outcome_unknown` for a withdrawal, journal, or whitelist creation, wait for the record that carries the work result: `answered` with Alpaca traffic when the work returned while the handler waited, or `settled` when the handler had already answered without it. Shutdown can cut the task before either result record exists. A local timeout and one empty recovery read never prove that Alpaca rejected the mutation. Before any new withdrawal, reconcile repeatedly over a window longer than the local timeout and expected Alpaca processing delay, matching the amount, destination, and a transfer `created_at` no earlier than the gateway answer. Before any new journal, apply the same rule to its quantity, symbol, destination, and `created_at`. Whitelist creation recovery checks the intended address and asset for the same longer window. Any replacement after no match is a deliberate operator decision, never an automatic retry.

## Issuer operations and the corporate action stream (`s01`)

- `issuer.mint_callback`, `issuer.redeem` and `issuer.request` run the library's `IssuerApi` calls against the configured account under a 270 second deadline, with the library's retry policy: up to 5 retries of a transient failure, waiting out Alpaca's `Retry-After` within a 30 second budget; a longer hold ends the call at once, and its answer relays the hold. Mint callbacks, redemptions and `issuer.request` each have their own issuer client, so a hold on one (a human polling `issuer.request` included) does not stall the others.
- The two POSTs answer `outcome_unknown` once any attempt may have reached Alpaca (any answer but a 4xx other than 408, or no answer after the request could have left), even when Alpaca refused a later resend; the answer keeps the last Alpaca status and any hold. Every issuer answer carries the status of the last answer Alpaca gave the call, and none when the client held the call back under an earlier `Retry-After` or no attempt was answered. A call refused before anything left (a held back request, a credential failure, a network outside the ITN list) or answered with a 4xx other than 408 on every attempt is `not_applied`, except a mint callback 400, which Alpaca documents as an internal failure while it processes the confirmation, and a redeem 422, which Alpaca also gives to an `issuerRequestId` it has already seen: both are `outcome_unknown`. The library sends a redeem's `issuerRequestId` as its `Idempotency-Key`, so Alpaca replays the first answer to the library's own resend. `retryableWithSameKey` is false for both: Alpaca's dedupe on `tokenizationRequestId` is not established, and Alpaca does not document how long it keeps a redeem key for replay.
- `issuer.request` for a request Alpaca does not hold answers `422 rejected` with `request_not_found` and Alpaca status 404.
- `corporate_actions.stream` relays Alpaca's event stream byte for byte with `content-type: text/event-stream`. `sinceId` alone resumes after a cursor, `since` with an optional `until` replays from an instant, and no query starts at live events; any other combination answers `400 invalid_request`. The type and region filter come from `corporate_actions.stream_url`. The stream has no deadline and no send gate: it ends when Alpaca closes it, after `corporate_actions.idle_timeout_secs` without a chunk, at the Cloud Run request timeout, or when gateway shutdown starts, and issuance reconnects from its cursor. A connect still pending when shutdown starts answers `503 unavailable` at once while the tracked connect uses the remaining shutdown grace. A connect failure answers as any failed read does; a 429 answers `backpressure` with Alpaca's `Retry-After`, and an answer that is not an event stream keeps the status Alpaca answered.

## Config

The service reads the TOML file named by `ST0X_ALPACA_GATEWAY_CONFIG`. The deployment supplies that path. `profile` is required, `t0` or `s01`. Unknown keys are refused at the top level and in the gateway's own tables; `[broker]` is read with the library's `AlpacaBrokerApiCtx`, which ignores keys it does not know, so a misspelled key there takes the library default and the startup account check is what catches a wrong endpoint. `environment` is exactly `production` or `staging`; any other value, `prod` or `Production` included, fails to parse. With `environment = "production"`, or with `mode = "production"` in `[broker]` (the real money Broker API, whatever the environment), only the Cloud KMS credential (`client_id` and `kms_key_version`) passes validation, so no key material for the real money endpoint lives in the config. A staging deployment against the sandbox signs with `api_key` and `api_secret` only: the wallet and tokenization clients mint Cloud KMS tokens at the production token endpoint, so validation refuses KMS with a sandbox or omitted mode, and it refuses the local private key (`private_key_pem`) everywhere. The bot, read and write audiences must differ. `identity.google_jwks_url` and `identity.iap_jwks_url` default to Google's key URLs; an override must be HTTPS, or HTTP on a loopback host. `st0x-alpaca-gateway --validate-config <path>` checks a file without contacting anything. A file that does not parse is reported with the parser's message, never the offending line itself, so a malformed credential line stays out of the startup log and the validation output.

```toml
profile = "t0"
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
kms_key_version = "projects/<project>/locations/<region>/keyRings/<ring>/cryptoKeys/<key>/cryptoKeyVersions/<version>"
account_id = "<account id>"
mode = "production"

[identity]
bot_audience = "<Cloud Run service URL>"
bot_principals = ["<unique id of bot service account>"]
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

An `s01` config has the same shape, with the S01 account and credential, the unique id of the issuance runtime service account in `bot_principals`, the issuer wallet in `tokenization.mint_recipients`, and an empty `wallet.bot_withdrawal_destinations` (the `s01` bot tier has no wallet operation). Only `s01` reads `[corporate_actions]`; both keys are optional:

```toml
profile = "s01"

[corporate_actions]
# Default: Alpaca's stream filtered to US cash and stock dividends. Any URL but HTTPS on stream.data.alpaca.markets fails startup.
stream_url = "https://stream.data.alpaca.markets/v1beta1/events/corporate-actions?type=cash_dividend_corporateaction_event,stock_dividend_corporateaction_event&region=us"
# The relay ends after this many seconds without a chunk.
idle_timeout_secs = 90
```

## Startup and health

1. Parse and validate config. Invalid config exits nonzero.
2. Verify the account with Alpaca (`GET /v1/trading/accounts/{id}/account`), require status ACTIVE and the configured account number. Any failure exits nonzero, so Cloud Run never routes traffic to a gateway bound to the wrong account or unable to reach Alpaca.
3. Serve. `/healthz` and `/readyz` answer without credentials; `/readyz` reports environment and version.

On SIGTERM the listener stops taking requests, the corporate action stream body ends, a pending stream connect answers `503 unavailable`, and one 8 second budget starts, inside Cloud Run's 10 second termination grace. Every deadline call in flight answers at once without its result, as described under [Request rules](#request-rules), and a request arriving after the signal answers `503 unavailable` (`not_applied` for a mutation). The connection drain therefore leaves the budget for detached tasks, which may finish until 8 seconds after the signal. A detached task still running at that point is cut and leaves no `settled` record. For a keyless mutation, neither the 30 second local request bound nor one empty recovery read proves that Alpaca rejected it; use the longer repeated reconciliation under [Request rules](#request-rules) before any replacement.

## Audit

Every request on a catalog operation's route writes a JSON line to stdout with `logging.googleapis.com/labels.log = "st0x_alpaca_gateway_audit"` and the event under `audit` (`st0x_alpaca_gateway_api::AuditEvent`). That includes a request the gateway refuses before any work: a credential that does not verify (caller subject `unverified`), a bot subject outside `identity.bot_principals`, and a body, path or query that does not parse; a refused mutation answers `not_applied`. A path no operation serves answers `404 unknown_operation` and writes no record. Fields: request id, deployment, environment, account id, caller subject and email, tier, `X-On-Behalf-Of`, operation, key, reason, request digest, money moving fields, Alpaca status (on a success the status of the last Alpaca API answer, on a failure the failure's own Alpaca status, none when it has none), `alpacaRequestIds` (the `X-Request-ID` of the Alpaca responses the work received, token mint answers included, in order, the join key with Alpaca support: the first 100, each cut at 128 characters), Alpaca object id (the order, transfer, journal, tokenization request or whitelist entries the call created or touched, comma joined; on a failure the entries a whitelist loop had already changed, or the tokenization request a network refusal is about), outcome, error code, latency and the gateway version, `<package version>+<commit>` (`unknown` in a build outside the flake). The key, the reason, each money moving field and `X-On-Behalf-Of` keep at most their first 256 characters, so one record stays one bounded log line whatever the caller sends; the request digest still covers the whole request. The detached task writes the record that carries the result and its Alpaca traffic: phase `answered` when the caller got that result, `settled` when the gateway had already answered without it (at the deadline or on shutdown) or the caller had gone away. An answer without the result writes its own `answered` record, without Alpaca traffic, so a request whose work outlives its answer has two records under one request id: the answer's `answered` record and the work's `settled` record. The order of the two records is not guaranteed. Records never contain credentials, tokens, Travel Rule names or response bodies.

`corporate_actions.stream` writes one result record for each connect, with Alpaca's status and request id or the failure: `answered` when the caller took the response, `settled` when the caller had gone away or shutdown answered first. A connect still pending at shutdown first writes an `answered` unavailable record without Alpaca traffic, then its tracked task writes the result as `settled` if it finishes within the shutdown grace.

Query the audit stream in Cloud Logging with `labels.log="st0x_alpaca_gateway_audit"`.

## Image

```bash
nix build .#gateway-oci
./result | docker load    # <gateway-image>:latest, entrypoint /bin/st0x-alpaca-gateway
```

The image has no base layer and a pinned creation time, so a commit always rebuilds to the same digest. `gateway-oci` and `st0x-alpaca-gateway` are flake outputs on Linux only, so on a Mac the image needs a Linux builder (`nix build .#packages.x86_64-linux.gateway-oci`). CI builds the image on every pull request.

## Deploying `t0-alpaca` to staging

Concrete organization, project, identity, key, registry, secret, and log routing names belong in the private infrastructure repository. A staging deployment needs:

1. A dedicated runtime service account and Cloud KMS key, with that account as the only `signerVerifier` and KMS Data Access audit logging enabled.
2. A BrokerDash credential registered against that key's public half, scoped to the staging account, with the narrowest scopes the operation matrix needs.
3. A Cloud Run service using the attested image digest, min and max instances 1, CPU always allocated, ingress all, request timeout 300 s, and the config mounted from Secret Manager at `<config-mount-path>`.
4. `run.invoker` for the staging bot runtime service account and the project's IAP service agent only.
5. An external HTTPS load balancer with a serverless NEG and separate `<read-prefix>/*` and `<write-prefix>/*` backends, each with IAP and its own group, backend timeout 300 s, and request logging at sample rate 1.0. Their backend ids are the two audiences in config.
6. A project log sink and audit module routing records with `labels.log="st0x_alpaca_gateway_audit"` to the protected audit destination.

Then:

1. Build and push the image to `<artifact-registry>/<repository>/<image>` and sign its Binary Authorization attestation.
2. Validate and publish the staging config: `docker run --rm -v $PWD/staging.toml:/candidate.toml <gateway-image>:latest --validate-config /candidate.toml`, then add it as a new Secret Manager version.
3. Deploy the digest with no traffic, check its startup log line `Gateway bound to its Alpaca account`, then move all traffic to it at once and check `/readyz` (see [Rollouts](#rollouts)).
4. From the staging bot runtime, call `GET /bot/v1/account/funds` with its ID token; from an operator machine, call the read tier account funds route through the load balancer. Both write audit records.

## Deploying `t0-alpaca` to production

Use the same controls in the production project, with a separate production BrokerDash credential, account, service account, KMS key, config, and artifact digest. The production config sets `environment = "production"` and must use the Cloud KMS credential; validation refuses any other shape. Production `bot_principals` lists only the production bot runtime service account.

## Deploying `s01-alpaca`

`s01.devops` deploys each environment into its own `<project>` in `<s01 org>`, with the `t0-alpaca` steps above and these differences:

1. The runtime service account is `<runtime service account>`, with its own KMS key `<KMS key>` and a BrokerDash credential scoped to the S01 account and the `s01` matrix. No T0 principal holds a role in either project.
2. Cloud Run service `s01-alpaca` with an `s01` config.
3. `run.invoker` for that environment's issuance runtime service account from its own `<project>` and the gateway project's IAP service agent only. Production `bot_principals` lists only the production issuance runtime service account.
4. IAP on the read backend admits `<reader group>` and `<admin group>`; on the write backend only `<admin group>`.
5. The S01 org's CI builds and attests the image from the same st0x.alpaca tag into `<artifact registry>`, and the project sink and audit bucket are the S01 org's.
6. To check a deployment, call `GET /bot/v1/issuer/requests/{tokenization_request_id}` for a known request from the issuance runtime with its ID token, and `<read-prefix>/v1/account/funds` from a laptop through the load balancer.

## Rollouts

Min and max instances 1 does not make the gateway a single process. Cloud Run caps instances per revision, so the old and the new revision run side by side while traffic moves and while the old one finishes the requests it already admitted and their detached work; shutdown ends its open corporate action stream before draining connections so detached work gets the remaining grace period. A platform replacement of the instance overlaps two processes the same way. Each process keeps its own human budget. Its broker, wallet, each tokenization client, the three issuer clients, and corporate action stream client each keep an independent credential token cache, mint lock, and `Retry-After` hold, so a hold one client takes does not stop its siblings or any client in the other process. Together the two processes can admit twice `human_budget_per_minute` in one minute. Two rules keep the humans within their share of the credential's Alpaca rate limit:

1. Set `human_budget_per_minute` to half the intended human share, so two processes together stay within it.
2. Deploy every revision (a new digest, a new config version, a rollback) with no traffic, then move all traffic to it at once. A revision without traffic admits nothing, so the overlap lasts only while the old revision drains. Never split traffic between two revisions: both keep admitting for as long as the split lasts. A config roll runs the same two commands with `--update-secrets` in place of `--image`. A release that adds a required config field (as `profile` does) cannot start on the previous config, and the previous image refuses the new field, so roll it with `--image` and `--update-secrets` in the same command, and roll it back the same way to the previous digest and config version.

```bash
gcloud run services update <service> --project <project> --region <region> --image <digest> --no-traffic
# Check the new revision's startup log line `Gateway bound to its Alpaca account`.
gcloud run services update-traffic <service> --project <project> --region <region> --to-latest
```

## Rollback

Both deployments roll back the same way.

| Problem | Action | Effect |
| --- | --- | --- |
| A bad gateway release | Redeploy the previous attested digest as in [Rollouts](#rollouts), with the previous config version when the release changed the config fields | Callers see the previous behavior; the `/v1` contract only ever adds operations and optional response fields |
| One operation misbehaves | Add it to `disabled_operations`, publish the config, roll the revision | That operation answers `403 capability_disabled` on every tier; the rest keep serving |
| The bot must stop withdrawing while operators keep withdrawing | Empty `wallet.bot_withdrawal_destinations`, publish the config, roll the revision | The bot's `wallet.withdraw` answers `403 capability_disabled`; operator withdrawals and every other bot operation keep serving |
| A caller must lose access | Remove its `run.invoker` binding (bots) or group membership (humans) | Its requests stop at the platform |
| All human access must stop | Remove the IAP service agent's `run.invoker` binding | Every human request stops; bots keep serving |
| The gateway credential is suspect | Revoke its BrokerDash credential. While bots still call Alpaca directly, revoke their credentials too for a full account stop. To restore the gateway, register a new BrokerDash client against the existing KMS key's public half, replace `client_id` in config, validate and publish the config, then roll the revision. | Revocation stops only traffic signed with each revoked client id. The KMS key remains nonextractable and can sign for the replacement client. |
| The gateway is unusable | No caller depends on the gateway yet: liquidity and issuance still call Alpaca directly with their own credential, so stopping or rolling back either deployment affects only human callers | Once a bot ships the `[alpaca] transport` switch (liquidity [RAI-1935](https://linear.app/makeitrain/issue/RAI-1935), issuance [RAI-1936](https://linear.app/makeitrain/issue/RAI-1936)), the rollback becomes: set it back to the direct transport. Its direct credential stays provisioned until that switch has been tested, and [RAI-1938](https://linear.app/makeitrain/issue/RAI-1938) removes it only after that |

The gateway never undoes an Alpaca action by itself. Undoing an order, a conversion or a transfer is the caller's decision, through the same operations.
