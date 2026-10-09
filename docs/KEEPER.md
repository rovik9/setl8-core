# setl8-keeper: the payout-cycle runner

`tools/keeper` is an off-chain program that keeps the vault's payout cycle running: it reconciles every product, begins a heartbeat cycle when one is due, settles the queued claims in batches and finalizes the cycle. Companion documents: [DEPLOY-CHECKLIST.md](DEPLOY-CHECKLIST.md) (the keeper steps and the monitoring list), [SECURITY-REVIEW.md](SECURITY-REVIEW.md) section 11 (what it can and cannot do wrong), [THREAT-MODEL.md](THREAT-MODEL.md), [DEVNET-REHEARSAL.md](DEVNET-REHEARSAL.md) (the measured costs).

## 1. What it is, and what it is not

* **It has no authority.** Every instruction it sends is permissionless: `reconcile_product`, `begin_heartbeat`, `settle_claims`, `finalize_heartbeat`. It needs no admin key, only a funded fee payer, and it can move money only the way anyone can: by asking the program to pay each claim its pro-rata share to the claimant's own associated token accounts. It cannot choose a destination, an amount, a claim's owner or a price. It builds **no other instruction** (no deposit, no bond, no admin instruction, no transfer): the four builders in `src/ixs.rs` are the only place a vault instruction is made, and a test decodes every transaction a full cycle sends to prove it.
* **Anyone can run one, and several at once are harmless.** The keeper keeps **no state** between passes (only logs): every decision is made from freshly read chain state, so a crash or restart at any point is safe. When another keeper wins a race, the program answers with a hard error (`ClaimAlreadySettled`, `ClaimNotEligible`, `CycleInProgress`, `NoCycleInProgress`, `HeartbeatTooEarly`, or `InvalidClaim` for a claim the winner paid in full and closed: SR-06); the keeper recognises these as "someone else did it", logs `lost_race` and re-reads. Nothing is paid twice because the program refuses a second settlement of a claim in the same cycle. A lock file exists only to stop two copies on **one machine** from fighting; it is not needed for correctness.
* **It is not a trust anchor.** If the keeper lies, loses its key or is run by a stranger, the worst outcome is that it does nothing or wastes its own fees. If it stops, payouts stop (section 6): that is a liveness risk, not a theft risk.
* **Its fee-payer key is hot and must be a fresh key.** It refuses to start with the SL8 or ROV admin key (any of the real pair and the public test pair), and with a key file readable by group or others.

## 2. What one pass does

1. Read the vault, both pool token accounts (balance **and** frozen flag), the registered products, the chain's own clock, and the fee payer's balance. Verify the node's genesis hash against `--cluster`.
2. **A cycle is open:** continue it. List the claims (`getProgramAccounts` with a discriminator and size filter, returning only addresses, then `getMultipleAccounts` in pages of 100; or `--claims-file`). A claim can be settled in cycle N exactly when `created_in_cycle < N` and `last_settled_cycle != N` (read from the program). Sort by address, cut into batches of **at most 6** (`MAX_SETTLE_BATCH`), build each batch's triples `[claim, trader USDC ATA, trader USDT ATA]` with the exact derived associated token accounts, **re-read those claims**, simulate, send, wait, re-read the vault, repeat until `processed == eligible`, then send `finalize_heartbeat`. A claim whose accounts are missing, frozen or re-owned is skipped by the program (it counts as processed and stays owed); the keeper never retries it in the same cycle.
3. **No cycle is open:** if there are **no open claims** it does nothing (an empty cycle would only burn the 5-day slot: SR-08). If `now >= cycle_started_at + 432,000 s` (chain time; the first cycle may start at once) it first checks it can list the claims, then sends `reconcile_product` for **every active product** (a paused product is skipped and logged; a missing tally counts as 0/0; a wrong tally address is logged loudly and not retried; a mismatching tally pauses the product, which raises an alert and does not stop the cycle), then `begin_heartbeat`, then continues as above.
4. A batch that fails in simulation is **split** and settled claim by claim, so one bad claim cannot block the others; it is quarantined for the rest of the pass and, if it prevents the cycle from finalizing, the pass ends with a hard failure that names the problem (exit 20). A transaction that lands but is not seen confirmed is never blindly resent: the keeper re-reads state first. Transient errors (rate limits, network) are retried with bounded backoff.
5. Alerts are evaluated at the start and again after the pass if it changed anything.

## 3. Running it

Build it (`scripts/verify-keeper-build.sh` builds the release binary with the **real** admin pubkeys, which it needs to derive the vault address, and proves they are inside and that `version` reports them as compiled in; the binary also contains the two public test admin pubkeys, on purpose, in its list of keys it refuses as a fee payer). The binary is `tools/keeper/target/release/setl8-keeper`.

```
setl8-keeper <mode> --cluster <devnet|mainnet|localnet|URL> [options]
```

| mode | what it does | exit codes |
|---|---|---|
| `dry-run` | print exactly what would be sent (reconcile, begin, each settle batch, finalize); **send nothing**; needs no funded key (`--fee-payer-pubkey`) | 0 / 10 / 20 |
| `status` | read-only health summary as one JSON document on stdout | 0 / 10 / 20 |
| `once` | one pass, then exit (cron friendly) | 0 nothing to do **or progress made**; 10 an alert condition is present; 20 hard failure |
| `run` | loop: one pass every `--interval` seconds | the code of the last pass when it stops |

Start-up refusals exit **2** (an admin key as fee payer, loose key permissions, mainnet without the flag, a node on another cluster than `--cluster`); usage errors exit **1**; a node that cannot be reached at start-up exits **20**.

| flag | meaning |
|---|---|
| `--cluster <c>` | required. `devnet`, `mainnet`, `localnet` or an `http(s)` URL; the node's genesis hash must match (a `localnet`/URL cluster must **not** be devnet or mainnet) |
| `--rpc <url>` | default: the cluster's public endpoint |
| `--keypair <file>` | the fee payer: a fresh hot key, mode 600. **Admin keys are refused** |
| `--fee-payer-pubkey <pk>` | `dry-run` and `status` without a key file |
| `--program-id <pk>` | override the compiled-in program id (before the real id is baked in) |
| `--interval <s>` | `run`: pause between passes (default 60) |
| `--priority-fee-microlamports N` | default 0; only then are ComputeBudget instructions added (limit = simulated units x 1.3 + 1000) |
| `--max-sends-per-run N` | default 100: transactions submitted per process run |
| `--max-fee-lamports-per-run N` | default 100,000,000: estimated fees per process run |
| `--min-balance-lamports N` | fee-payer alert threshold (default 50,000,000 = 0.05 SOL) |
| `--max-retries N` | default 4, for transient failures |
| `--claims-file <file>` | claim addresses, one per line (`#` comments allowed), for providers without `getProgramAccounts` |
| `--log-file <file>` | append the JSON log lines |
| `--webhook <url>` | POST each alert as JSON. Prefer the **`SETL8_KEEPER_WEBHOOK` environment variable**: argv is visible to other users. The URL is never printed or logged |
| `--lock-file <file>` | stop two local copies from fighting (remove a stale one by hand) |
| `--i-understand-this-is-mainnet` | required for `--cluster mainnet` |

A first, safe look (no key, no sends):

```
setl8-keeper status  --cluster devnet --fee-payer-pubkey <pubkey>
setl8-keeper dry-run --cluster devnet --fee-payer-pubkey <pubkey>
setl8-keeper once    --cluster devnet --keypair ~/keeper.json --log-file keeper.log
```

**systemd** (a loop; restart on failure):

```
[Unit]
Description=setl8 keeper
After=network-online.target
[Service]
User=keeper
Environment=SETL8_KEEPER_WEBHOOK=https://hooks.example.org/xxxx
ExecStart=/opt/setl8/setl8-keeper run --cluster mainnet --i-understand-this-is-mainnet \
  --rpc https://your-provider.example/ --keypair /etc/setl8/keeper.json \
  --interval 300 --log-file /var/log/setl8-keeper.log
Restart=always
RestartSec=30
NoNewPrivileges=true
ProtectSystem=strict
ReadWritePaths=/var/log
[Install]
WantedBy=multi-user.target
```

**cron** (one pass an hour; the exit code feeds your monitoring, 10 means "look at the alert"):

```
7 * * * * keeper /opt/setl8/setl8-keeper once --cluster mainnet --i-understand-this-is-mainnet --rpc https://your-provider.example/ --keypair /etc/setl8/keeper.json --log-file /var/log/setl8-keeper.log || echo "keeper exit $?"
```

## 4. Alerts

Alerts are JSON log lines with `"level":"alert"` (stdout, `--log-file`) and, with a webhook, a POST of the same fields (a repeat of the same alert is posted again only after an hour; it is always logged). A webhook failure is logged (`webhook_failed`, without the URL) and the keeper carries on. Nothing secret is ever logged.

| alert | when | what to do |
|---|---|---|
| `pool_frozen` | a payout pool's token account is frozen by the issuer | payouts shrink or stop until it thaws (SR-03: the heartbeat treats it as empty and pays from the other pool; both frozen = nothing is paid, the cycle still closes). Contact the issuer; tell users to use the other mint |
| `product_auto_paused` | `reconcile_product` found a sector's tally disagreeing with the vault (reason code 2) | read the reconcile log; fix the sector's tally (SR-05/SR-17), then the admins reactivate. Queued claims still settle |
| `cycle_open_too_long` | a cycle open for more than 24 h | check the keeper(s) are running and the RPC works; run `status`; look for `cycle_stuck` / `cycle_stalled` events |
| `no_cycle_for_too_long` | no cycle begun for more than 6 days while claims are open | the keepers are down or blocked; start one |
| `claims_near_ceiling` | open claims above **80%** of the $2,500,000 ceiling (`at_ceiling` true at 100%) | bond exits and new payout requests are refused at the ceiling: make sure cycles are running and the pools are funded; see SR-02 |
| `coverage_below_one` | spendable pool balance (frozen pools count as 0) is below the open claims | claims are paid pro rata; the ratio is in the alert. A cause to look at, not a keeper fault |
| `payer_balance_low` | the fee payer holds less than `--min-balance-lamports` | top it up (section 5) |
| `claim_skipped_repeatedly` | an open claim's destination accounts are missing/frozen/re-owned and it has been processed in 3 or more cycles | it stays owed until the trader fixes their account; nothing the keeper can do. Stateless approximation: judged from the claim's own cycle numbers and the accounts as they are now |
| `genesis_mismatch` | the node's genesis hash is not the `--cluster`'s (hard failure, nothing is sent) | you are pointed at the wrong network or a hijacked endpoint: stop and check the URL |

Other log events worth knowing: `lost_race` (benign), `batch_split`, `claim_quarantined` (a claim the program refuses for a reason other than a race: investigate), `cycle_stuck` (the cycle cannot finalize), `reconcile_failed`, `unconfirmed`, `send_cap_reached`, `transient_failure`, `cannot_load_claims`.

## 5. Funding the fee payer

Each transaction costs the base fee of **5,000 lamports** (one signature) plus any priority fee. A cycle sends `P + 2 + ceil(N / 6)` transactions (`P` products, `N` open claims): 1 product and 600 claims is 103 transactions, about 0.0005 SOL; at most one cycle starts every 5 days, so under 0.04 SOL a year. Closing a fully paid claim returns its rent (about 0.0018 SOL) **to the keeper that sent the settlement**, which usually exceeds the fees. Fund the key with **0.5 SOL** and let the default alert (0.05 SOL) tell you when to top it up. Measured on a real validator (see section 8): a full 6-claim batch uses 77,000-102,000 compute units, below the default 200,000 limit, so **no ComputeBudget instruction is needed**.

## 6. Risk model

* **A stopped keeper stops payouts.** Claims only get paid by a heartbeat cycle. If every keeper is down, queued claims wait, the cycle (once begun) stays open, and new claims queue behind it.
* **Once trader claims fill the headroom under the $2,500,000 ceiling, bond exits are refused** until a cycle pays the total down (SR-21 fix, SR-02). A dead keeper therefore also blocks bond withdrawals when the queue is large.
* **Run two independent keepers**, on different hosts and different RPC providers, with different fee-payer keys. They cannot hurt each other (section 1); the second only matters when the first is down. Alert on `no_cycle_for_too_long` from outside the keeper too (an external check of `status`).
* **The keeper trusts the RPC node for what it reads.** A dishonest node could make it do nothing or send transactions the program will reject; it cannot make the program pay anything the program does not owe. The genesis check catches a node on the wrong network.
* **Mainnet** needs `--i-understand-this-is-mainnet` every time; a devnet keeper cannot be pointed at mainnet by accident because the genesis hash is checked.

## 7. RPC providers and `--claims-file`

* The keeper needs `getProgramAccounts` (with `dataSize` + `memcmp` filters and `dataSlice`), `getMultipleAccounts`, `simulateTransaction`, `sendTransaction`, `getSignatureStatuses`, `getLatestBlockhash`, `getBlockHeight`, `getBalance`, `getAccountInfo`, `getGenesisHash`. The public devnet endpoint supports all of them; some free mainnet tiers disable `getProgramAccounts` or rate limit hard (HTTP 429: the keeper backs off and retries).
* **Fallback:** `--claims-file claims.txt` with one claim address per line. The file must list **every** open claim when a cycle is begun, or the cycle cannot finalize (the pass ends with `cycle_stuck` and exit 20; add the missing addresses and run again). Produce it from a provider or indexer that does support `getProgramAccounts`. The keeper still re-reads each claim from the chain before sending, so a stale file can only omit claims, never cause a wrong payment. With no list it refuses to begin a cycle it could not settle, before sending anything.

## 8. Measured: end to end on a real validator

`tools/keeper/selftest-local.sh` starts a local `solana-test-validator`, deploys the vault and the mock sector, builds a queue of 43 claims with the devnet rehearsal driver, then starts **three keeper processes with three different keys at the same instant**. Result (this is a local validator, not devnet): one cycle, the cycle closed, all 43 claims paid in full and nothing owed, 13 transactions landed in about 6 s wall time, and every keeper that lost a race logged `lost_race` (`CycleInProgress`, `InvalidClaim` on a claim a rival had already paid and closed, `NoCycleInProgress` on finalize) and carried on: no keeper reported an error.

| transaction | compute units on chain | fee |
|---|---:|---:|
| `reconcile_product` | 9,801 | 5,000 lamports |
| `begin_heartbeat` | 5,670 | 5,000 |
| `settle_claims` x1 | 16,557 | 5,000 |
| `settle_claims` x6 | 77,650 to 95,649 (varies with the PDA derivations of the wallets) | 5,000 |
| `finalize_heartbeat` | 5,645 | 5,000 |

## 9. Tests

`cargo test --manifest-path tools/keeper/Cargo.toml --features localnet` (part of `scripts/test-all.sh`): 201 tests with the `localnet` test keys, 27 unit tests in the default (real-key) build. They run the real program (and the mock sector, to create real claims and a real payout tally) in LiteSVM **with signature verification on**, and cover: a whole 15-claim cycle (USDC and USDT claimants, a missing and a frozen account, a bond claim) with every payment equal to the program's own plan to the base unit; a second and third keeper racing at send granularity; crash and restart at every point; unconfirmed and rate-limited sends; a poisoned claim; frozen pools (before begin, between begin and settle, both); a wrong tally; every alert at its boundary; the 432,000 s boundary to the second; and every refusal and secrecy rule. 37 mutants of the decision logic and safety checks are all killed ([SECURITY-REVIEW.md](SECURITY-REVIEW.md), appendix E).

## 10. Dependencies

The keeper is its own Cargo workspace with its own `Cargo.lock`, seeded from `tools/admin/Cargo.lock`: **no crate was added** beyond what the admin tool already uses. No build script touches the network, nothing reports telemetry, nothing needs an account, phone number or KYC. The one additive change to another crate: `HttpRpc::call` in `tools/admin` was made `pub` (the keeper uses the admin tool's JSON-RPC client instead of a second one); no behaviour changed.

| crate | version | why |
|---|---|---|
| `setl8-admin` (path) | 0.1.0 | the JSON-RPC client (`HttpRpc`), key-file loading with the permission check, `Keys` (vault / pool / registry addresses), cluster and genesis constants, the panic hook |
| `core-vault` (path) | 0.1.0 | account types, instruction structs, error enum, `MAX_SETTLE_BATCH`, `HEARTBEAT_MIN_GAP_SECS`, `OPEN_CLAIMS_CEILING` |
| `setl8-shared-interfaces` (git tag) | v0.4.1 | `derive_payout_tally` (the tally address of a product's sector) |
| `anchor-lang`, `anchor-spl` (`token` only) | =0.32.1 | `Pubkey`, account decoding, the SPL Token account layout |
| `solana-keypair`, `solana-signer`, `solana-signature`, `solana-message`, `solana-hash` | =2.2.3, =2.2.1, =2.3.0, =2.4.0, =2.3.0 | signing and building legacy transactions |
| `bs58`, `base64`, `bincode`, `serde_json` | =0.5.1, =0.22.1, =1.3.3, =1.0.151 | RPC filters and payloads, the transaction wire format, JSON log lines |
| `ureq` (`tls`, `json`) | =2.12.1 | the webhook POST (the only HTTP the keeper does itself) |

Dev-only (tests): `litesvm =0.7.0`, `solana-account`, `solana-transaction`, `solana-transaction-error`.
