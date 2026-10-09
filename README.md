# setl8-core

The Setl8 **core vault** — an [Anchor](https://www.anchor-lang.com/) (Solana) program that holds the USDC/USDT payout pools and the per-trader bookkeeping for Setl8 sector programs (leveraged trading, options, …).

Sector programs never touch the pools directly. They call the vault over CPI, and the vault checks, against an on-chain registry, that the caller really is a registered sector program before it moves any money.

| | |
|---|---|
| Program name | `core_vault` (`programs/core-vault`) |
| Program ID (localnet) | `2Z6WNsj4hNhKhmK9Cj3sXV5San9VYhh8gwtyvBfpP6ft` |
| Anchor | 0.32.1 |
| Tokens | classic SPL Token only, 6-decimal USDC and USDT (Token-2022 is rejected) |
| Shared types | [`setl8-shared-interfaces`](https://github.com/rovik9/setl8-turbo) pinned at tag `v0.4.0` |

> **Status: pre-audit, not deployed.** See [Keys and builds](#keys-and-builds) before building anything you intend to deploy.

## How it works

1. **Setup.** Both admins (SL8 and Rov, a 2-of-2 multisig) call `init_vault` once. It records the USDC and USDT mints and creates one pool token account per mint, as PDAs.
2. **Register a sector.** The admins call `register_product` for a sector program. This stores a `ProductRegistry` PDA with its challenge sizes, its fee split (`fee_split_bps`), its reset price and its payout cap.
3. **Money in.** When a trader pays, the sector program CPIs `deposit_fee` (new challenge) or `deposit_reset` (reset). The vault pulls the payment from the trader and splits it: `pool = floor(amount × fee_split_bps / 10_000)` goes to the matching pool and the remainder goes to the SL8 token account.
4. **Activity tracking.** The sector calls `record_activity` to prove a trader is still active. A trader who is inactive for **more than 7 days** is stale, and `mark_abandoned` (callable by anyone) can mark them `Abandoned`. Pausing a product freezes the inactivity clock.
5. **Money out is queued.** The sector CPIs `request_payout`. It validates the request exactly as before (active product, status, inactivity, payout cap, request id) but moves **no tokens**: it records a `PayoutClaim` (the amount owed to the trader, in 6-decimal dollar units, USDC = USDT = $1) and bumps `open_claims_count` / `open_claims_total` on `VaultState`. A stale challenge still flips to `Abandoned` and creates no claim. **The total of all open claims is hard-capped at `OPEN_CLAIMS_CEILING = 2_500_000_000_000` base units ($2.5M):** a request (or a bond withdrawal) that would push the total over it fails with `ClaimsCeilingExceeded` and changes nothing. Bonds alone can owe at most $780,000, which leaves about $1,720,000 for trader claims; if trader claims fill that headroom, new requests and bond exits are refused until a heartbeat pays the total down. Raising the ceiling needs a program upgrade.

   Claims are paid by a permissionless **heartbeat**, at most once every 5 days (`HEARTBEAT_MIN_GAP_SECS = 432_000`, measured between cycle starts):

   1. `begin_heartbeat` opens a cycle and snapshots the total owed and the total available in the two pools. A pool the issuer has **frozen** counts as empty in that snapshot.
   2. `settle_claims` is called in batches. Every eligible claim is paid the same pro-rata share, `min(available, owed) / owed`, of what it is owed, from the larger pool first and topped up from the other (a frozen pool is treated as empty, so it pays nothing and nothing is transferred from it; if both are frozen the claims are processed with zero pay and carry over, and the cycle still finishes). Destinations are the trader's associated token accounts; a claim whose accounts are unusable is skipped and stays owed. Whatever is unpaid stays owed and carries over, with no priority by age and no expiry. A claim paid in full is closed and its rent goes to the caller.
   3. `finalize_heartbeat` ends the cycle once every eligible claim has been processed, and sets each pool's reserve floor to 25% of its balance. Floors only constrain a future admin withdrawal; they never limit claim settlement.

### Who may call what

| Group | Instructions | Authority |
|---|---|---|
| `admin/` | `init_vault`, `register_product`, `update_product_config`, `pause_product`, `reactivate_product`, `admin_withdraw_marketing_funds` | Both admin signatures (SL8 + Rov) |
| `sector/` | `deposit_fee`, `deposit_reset`, `record_activity`, `request_payout`, `flag_trader_failed` | A registered sector program via CPI, authenticated by its `sector_authority` PDA |
| `permissionless/` | `mark_abandoned`, `reconcile_product`, `deposit_bond`, `request_bond_payout`, `begin_heartbeat`, `settle_claims`, `finalize_heartbeat` | Anyone |

The CPI-auth check is `utils::assert_sector_authority`. It verifies the caller's on-chain identity against the `ProductRegistry` and never trusts a self-reported program ID.

### Accounts (PDAs)

| Account | Seeds | Holds |
|---|---|---|
| `VaultState` | `["vault_state", SL8_ADMIN, ROV_ADMIN]` | USDC/USDT mints and pool addresses, `sl8_wallet`, reserve floors, the open-claim counters, the bond totals, the marketing-withdrawal totals and the heartbeat cycle state |
| Pool token account | `["pool", vault_state, mint]` | The USDC / USDT funds (authority = `vault_state`) |
| `ProductRegistry` | `["product_registry", product_program_id]` | Per-sector config, `active` flag and pause fields, and the request counters reconciliation uses |
| `TraderState` | `["trader_state", product_program_id, trader_wallet, challenge_id (u64 LE)]` | Trader status, activity clock, reset history |
| `PayoutClaim` (kind 0, trader) | `["payout_claim", trader_state, request_id (u64 LE)]` | An amount owed to a trader, until a heartbeat cycle pays it |
| `PayoutClaim` (kind 1, bond) | `["bond_claim", depositor, deposit_index (u64 LE)]` | An amount owed to a bond holder after `request_bond_payout` |
| `BondPosition` | `["bond", depositor, deposit_index (u64 LE)]` | One open bond; closed by `request_bond_payout` |
| `BondCapTracker` | `["bond_cap", depositor]` | A wallet's open principal and its next deposit index |

Every PDA type has a different total seed length (40 to 84 bytes), so no two types can collide; a unit test guards this.

## Repository layout

```
programs/core-vault/src/
  lib.rs              the program: one thin wrapper per instruction, grouped by who calls it
  constants/          seeds.rs  admin.rs  limits.rs  tokens.rs
  errors.rs           VaultError
  state/              on-chain accounts: vault_state, product_registry, trader_state, payout_claim,
                      bond_position, bond_cap_tracker
  instructions/       one file per instruction (Accounts struct + handler)
    admin/              init_vault, register_product, update_product_config, pause_product, reactivate_product,
                        admin_withdraw_marketing_funds
    sector/             deposit_fee, deposit_reset, record_activity, request_payout, flag_trader_failed
    permissionless/     mark_abandoned, reconcile_product, deposit_bond, request_bond_payout, begin_heartbeat, settle_claims, finalize_heartbeat
  utils/              auth.rs (sector CPI-auth check), token_payment.rs (fee split + transfers),
                      settlement.rs (pro-rata arithmetic), destination.rs (ATA checks), reconciliation.rs (tally check), reserve.rs (marketing reserve), bond.rs (bond fees and interest),
                      pda_account.rs
tests-rs/             LiteSVM integration tests (own Cargo workspace)
tools/admin/          setl8-admin: the admin signing tool (own Cargo workspace; see docs/ADMIN-TOOL.md)
tools/devnet-sector/   mock sector program: TEST SCAFFOLDING for the devnet rehearsal (own workspace; insecure by design)
tools/devnet-rehearsal/ driver that runs the whole protocol on a real cluster (own workspace; see docs/DEVNET-REHEARSAL.md)
tests/                Anchor TypeScript tests + admin test keypairs
scripts/              build, test and deploy-gate scripts (see below)
vault-repo-spec.md    original spec for this repo
setl8-architecture-update-2026-09-11.md   architecture decisions this repo follows
```

## Keys and builds

The two admin keys (SL8 and Rov) are compiled into the program as constants and are part of the `VaultState` PDA seeds.

- **Real keys never enter this repo or a `.env` file.** Only the public addresses are in the source (`programs/core-vault/src/constants/admin.rs`).
- **Default build = real pubkeys.** `anchor build` / `scripts/anchor-build-checked.sh` produce `target/deploy/core_vault.so` with the real, founder-held admin addresses.
- **`localnet` feature = public test pubkeys.** Their private keys are committed in `tests/fixtures/`, so anyone can sign as them. That build goes to `target/test-deploy/` and is used only by the tests. It is never in `default`, and it derives different vault PDAs.
- **Before any deploy, run `scripts/verify-deploy-build.sh`.** It builds the default program, then fails unless `target/deploy/core_vault.so` contains the raw bytes of both real addresses and neither test address. It prints the `.so` sha256.
- `.gitignore` blocks `.env*`, `*-keypair.json`, `id.json` and `*.pem`. The only keypairs allowed in git are the public test fixtures in `tests/fixtures/*.json`.

> **Decided by the founder: the revenue address stays the SL8 admin key.** `init_vault` sets `sl8_wallet = SL8_ADMIN_PUBKEY`, so SL8's share of every fee lands in token accounts owned by that one key. **Whoever holds the SL8 admin key holds the revenue and half of every bond (see Bonds), so protect it accordingly.** Its USDC and USDT token accounts must exist before any fee arrives (`docs/DEPLOY-CHECKLIST.md`).

## Admin signing tool

The six 2-of-2 admin instructions (`init_vault`, `register_product`, `update_product_config`, `pause_product`, `reactivate_product`, `admin_withdraw_marketing_funds`) are signed through **`tools/admin`**, the `setl8-admin` command-line tool: one side prepares a transaction file, each signer inspects it from its raw bytes and signs separately (on their own machine, with a durable nonce so the signatures may be hours apart), anyone submits. It is its own Cargo workspace (`cargo test --manifest-path tools/admin/Cargo.toml --features localnet`; `scripts/verify-admin-tool-build.sh` builds the release binary and checks it embeds the real admin keys). The ceremony step by step, key-custody options and their limits (the SL8 key currently lives in a phone wallet, which cannot sign these transactions), and what the tool does not protect against are in [`docs/ADMIN-TOOL.md`](docs/ADMIN-TOOL.md).

## Building

Prerequisites: Rust, the Solana/Agave toolchain (`cargo build-sbf`), Anchor CLI 0.32.1, Node.js.

Build with the checked script instead of plain `anchor build`:

```bash
scripts/anchor-build-checked.sh
```

`anchor build` exits 0 even when the SBF toolchain reports a stack-frame overflow (a function frame over 4,096 bytes), and such a program can silently corrupt memory at runtime. The script fails on that warning, for both the default build and the `localnet` test build (`scripts/build-test-so.sh`), and prints each `.so` sha256.

## Testing

```bash
npm install
scripts/test-all.sh               # everything below, stops at the first failure
```

`test-all.sh` runs, in order:

1. `scripts/build-test-so.sh`, which builds the `localnet` `.so` into `target/test-deploy/` (never `target/deploy/`);
2. `cargo test -p core-vault`, with default features (real keys pinned) and with `--features localnet` (test keys pinned);
3. `cargo test --manifest-path tests-rs/Cargo.toml`, the LiteSVM integration suite;
4. `scripts/test-ts.sh`, the TypeScript suite on a validator loaded with the test `.so`.

`tests-rs` also holds `claims_ceiling.rs` and `frozen_pools.rs` (the claims ceiling and the frozen-pool handling, with exact amounts), and `invariants_fuzz.rs`, a seeded model-based random-sequence tester (30 fixed seeds x 400 steps in the normal run; an `#[ignore]`d 20 x 5,000-step run: `cargo test --manifest-path tests-rs/Cargo.toml --test invariants_fuzz -- --ignored fuzz_long --nocapture`; replay one seed with `FUZZ_SEED=<n> ... fuzz_one`). It compares the real program with an independent model after every step and asserts global invariants (token conservation, counters vs accounts, caps, cycle state machine, rent, no state change on any rejected call). `compute_budget.rs` measures every instruction's compute units.

`tests-rs` is a separate Cargo workspace with its own `Cargo.lock`, so LiteSVM's dependency tree never touches the program's lockfile. It exercises every instruction with exact-error assertions: admin signatures, CPI authentication, fee splits, the pause-adjusted inactivity clock, and the payout queue with its pro-rata heartbeat settlement. It enables the `localnet` feature and refuses to run against a `.so` that does not embed the same admin keys. Set `CORE_VAULT_SO=/path/to/other.so` to run it against a different build (used for mutation testing).

Don't use plain `anchor test`: it would load the real-key build, which the suite cannot sign for.

## Bonds

Anyone can lock USDC or USDT in a bond for a fixed term and earn interest at maturity. No admin instruction can change, close or redirect a bond or its claim, and the product-level pause does not apply to bonds. Two caveats. First, the documented exception below: bond principal sits in the payout pools (half of it; SL8 receives the other half at deposit), and the two admins together can withdraw up to 75% of a pool's live balance with no deduction for bond liabilities (see [Security model](#security-model-and-the-one-known-exception)). Second, **the bond product is not trustless: depositors must trust the holder of the SL8 key.** SL8's half of each principal goes straight to a token account owned by the SL8 admin key, and a withdrawal pays the whole principal back out of the pool, so the SL8 key holder, or any wallet they control, can take roughly half of every bond they open, bounded by $50K per wallet and $600K in total, with no second signature (SR-01, accepted by the founder as the same trust class as the admin-withdrawal exception).

| Term | Length | Hard lock | Interest (only at maturity) |
|---|---|---|---|
| 6 months | 180 days | first 90 days | 20% |
| 9 months | 270 days | first 135 days | 30% |

- **Deposit (`deposit_bond`).** The depositor signs and pays `principal + fee` from their own token account of the chosen mint. The fee is 0.2% of the principal, rounded **up**, and goes 100% to the SL8 wallet's token account. The principal is split exactly like a `deposit_fee` payment: half (rounded down) into the same-mint payout pool, the rest to the SL8 wallet. Example: $1,000 in USDC costs 1,002.000000; the pool gets +500 and the SL8 wallet +502.
- **Limits.** At least $50, at most $50K open per wallet (summed over all its positions) and $600K open in total. Caps are measured on principal, not on the fee. Each wallet's deposits are numbered 0, 1, 2, ... (`deposit_index`) and an index is never reused.
- **State.** A `BondPosition` per deposit (`["bond", depositor, index]`) and one `BondCapTracker` per wallet (`["bond_cap", depositor]`).
- **Rules at withdrawal.** Interest is not accrued over time. Before the hard lock ends the bond cannot be withdrawn (`BondLocked`). From the lock until maturity the bond is worth its principal. At or after maturity it is worth principal plus `floor(principal * interest_bps / 10_000)`. The interest rate is copied into the position at deposit, so it cannot change under an open bond.
- **Withdrawal (`request_bond_payout`).** Only the depositor can call it. It closes the position (rent back to the depositor), frees the wallet's and the vault's cap room, and queues the amount owed as a normal `PayoutClaim` (kind 1, address `["bond_claim", depositor, index]`). The 0.2% withdrawal fee (rounded up) comes off the amount owed. No tokens move at request time, so the fee is simply not owed: it stays in the pool, and `bond_withdrawal_fees_retained` counts it. Examples for $1,000: six months at maturity owes 1,200 less 2.4 = 1,197.6; the same bond at month 4 owes 1,000 less 2 = 998; nine months at maturity owes 1,300 less 2.6 = 1,297.4.
- **Payment.** Bond claims are settled by the same permissionless heartbeat as trader claims, with the same single cycle ratio, equal pro-rata, carry-over, skip rules and associated-token-account destinations. The two kinds are indistinguishable in settlement; only the claim address derivation differs (`PayoutClaim.kind`).

## Reconciliation

The vault and each sector program keep independent books of the same thing: how much the sector has asked the vault to pay. `reconcile_product` compares them and **auto-pauses a product whose books disagree**.

- **The vault's books.** `ProductRegistry` counts `total_requests_emitted` (accepted `request_payout` calls) and `total_requested_amount` (the sum of their `amount`s), both bumped at the same moment a claim is queued. The stale/`Abandoned` path counts nothing.
- **The sector's books.** Every sector program keeps a *payout tally*, a PDA owned by the sector at `derive_payout_tally(product_program_id)`, laid out as defined in `setl8-shared-interfaces` (`PayoutTally`): `requested_count` and `requested_total`, updated in the same transaction whenever `request_payout` returns `Paid`.
- **The check.** `reconcile_product(product_program_id)` is permissionless; any signer pays the fee. Accounts, in order: `caller` (signer), `product_registry` (writable PDA), `payout_tally` (read-only, must be exactly the canonical tally address or the call fails with `InvalidTally`; junk can never pause a product).
  - The product must be active; an already-paused product fails with `ProductAlreadyPaused` and keeps its original pause reason and time.
  - A tally owned by the sector is read with the shared crate's own parser. An unparseable one is a mismatch. No data and not owned by the sector (nonexistent, or only holding lamports) counts as `0 / 0`. Data owned by anyone else is a mismatch.
  - Both numbers must be equal: a difference in either field, in either direction, is a mismatch.
  - On a mismatch the product is paused with reason `PAUSE_RECONCILIATION_DEFICIT` (`paused_since` = now), the four numbers are logged, and the call still returns **Ok** so the pause persists. On a match nothing changes.
- **What a pause does.** `deposit_fee`, `deposit_reset` and `request_payout` fail with `ProductNotActive`, and traders' inactivity clocks freeze. Claims already queued are **still settled** by the heartbeat. Un-pausing stays 2-of-2 (`reactivate_product`).
- **Keeper duty.** The heartbeat instructions do not call `reconcile_product`. The keeper must call it for **every product before each `begin_heartbeat`**.
- **Known limit.** Both sides' counts only ever go up. A sector that *under*-reports can catch up and then match again after an admin reactivates it. A sector whose tally *over*-reports (it counted a request the vault never accepted) cannot match until its tally is repaired: the vault's books cannot be lowered to meet it, so every reconcile re-pauses it. The repair is on the sector side, with no vault change: upgrade the sector program to write the vault's counters (`total_requests_emitted`, `total_requested_amount`, readable from the registry) into its tally account, then reactivate. If that is not possible, register a new product (SR-05, accepted by the founder).

## Security model and the one known exception

The design rule is **no admin key on money**: the admins configure products and pause or resume them, but cannot change a claim, edit a bond or send money anywhere but SL8's own token account. There is exactly one documented exception, `admin_withdraw_marketing_funds`, which does move pool funds to SL8's account.

**What the two admins can do.** Together (both signatures, SL8 and Rov), they can move up to **75% of one pool's live balance** to the SL8 wallet's token account.

- `admin_withdraw_marketing_funds(pool, amount)` names a side (`Usdc` or `Usdt`) and an amount. Each pool is handled alone; the two are never combined.
- Per pool, with the LIVE balance: `reserve = max(stored_floor, ceil(live * 25%))` and `withdrawable = live - reserve`. The amount must be greater than zero and at most `withdrawable`, else `ZeroAmount` or `WithdrawalExceedsReserve`.
- The `max` is deliberate. The stored floors are 0 until the first `finalize_heartbeat`, and a plain "balance minus stored floor" would let the admins take 100% before then. The stored floor still wins whenever the balance has fallen since it was stored.

**What they cannot do.**
- Send funds anywhere but the SL8 wallet's token account for that mint. There is no destination argument and no extra account; the account is checked exactly as `deposit_fee` checks SL8's destination.
- Take more than 75% of a pool's live balance in one call, or touch the other pool in the same call.
- Do anything with one signature. Both must sign.
- Change claims, bonds, caps or any trader's state.

**What it deliberately does NOT protect (the exception).** By the founder's explicit choice the cap is plain "75% of the pool balance, 25% floor". There is **no deduction for open claims, bond liabilities or the current heartbeat cycle**, and the call works at any time: mid-cycle, with a product paused, and repeatedly.

- The pool can hold bond principal and queued trader payouts, and the admins can still take 75% of it.
- Repeated withdrawals shrink a pool geometrically: after `n` calls roughly `balance * 0.25^n` remains. (Exact: each call leaves `ceil(25%)` of the previous balance.)
- Queued claims then settle pro rata against what is left. `settle_claims` already caps every payment at the live balance, so claims receive less and the unpaid remainder stays owed and carries over to later cycles. Nothing is created or lost, and nobody is paid more than they are owed, but a claim can wait a long time if the pool is drained and not refilled.
- The withdrawal is vault-level: neither a product pause nor a reconciliation auto-pause blocks it.

Every withdrawal logs the pool, the amount, the withdrawable amount and the reserve, and the vault keeps cumulative totals (`marketing_withdrawn_usdc`, `marketing_withdrawn_usdt`).

## Security documentation

- [`docs/SECURITY-REVIEW.md`](docs/SECURITY-REVIEW.md): the instruction-by-instruction review, the 13 hunted bug classes, the findings list (`SR-xx`) and the compute table. Internal review, **not an audit**. SR-21 (one huge `request_payout` could lock every bond exit) and SR-03 (a frozen pool could wedge the heartbeat) are fixed. Open: SR-02 (a registered sector's payout amounts are trusted; no per-product limit is built because the sector payout mechanics are not decided) and SR-15 (who holds the upgrade authority). SR-01 (the SL8 key holder can take about half of every bond they open), SR-18 (revenue on the SL8 admin key), SR-04, SR-05 and SR-14 are accepted by the founder, with their consequences written out there.
- [`docs/THREAT-MODEL.md`](docs/THREAT-MODEL.md): assets, actors, trust assumptions, attack trees, and what each compromise can do.
- [`docs/DEPLOY-CHECKLIST.md`](docs/DEPLOY-CHECKLIST.md): devnet then mainnet, upgrade-authority handling, keeper duties, monitoring, incident steps.
- [`docs/ADMIN-TOOL.md`](docs/ADMIN-TOOL.md): the admin signing ceremony (`tools/admin`).
- [`docs/DEVNET-REHEARSAL.md`](docs/DEVNET-REHEARSAL.md): the devnet rehearsal (`tools/devnet-rehearsal`, `tools/devnet-sector`, `scripts/devnet-rehearsal.sh`): how to repeat it, what a real validator showed (95/95 checks on a local validator; the devnet run itself is pending a funded faucet), and what differs from LiteSVM.

## Design notes

- **Minimal money surface.** Tokens move in only in `deposit_fee`, `deposit_reset` and `deposit_bond`, and out only in `settle_claims` and the documented-exception `admin_withdraw_marketing_funds`. `request_payout` and `request_bond_payout` only record a claim.
- **Settlement batches.** `settle_claims` takes at most `MAX_SETTLE_BATCH = 6` claims per call: a full batch is 1,106 bytes (limit 1,232) and about 113,000 to 140,000 compute units for ordinary wallets (about 235,000 to 243,000 for wallets ground to make the token-account derivation expensive; one claim alone is under 45,000; the figures vary with the random wallets' derivation cost), so add a `SetComputeUnitLimit` of 400,000. Any single claim can always be settled alone. `tests-rs/tests/settle_batch.rs` measures all of this.
- **Unusable destinations are skipped, wrong ones are errors.** A claim whose associated token accounts are missing, frozen, re-owned, or of the wrong mint is skipped and stays owed until its owner fixes the account. (A frozen payout *pool* is different: it is treated as empty rather than skipping or reverting, see the heartbeat above.) Deposits into a frozen pool and an admin withdrawal from it fail inside the token program; traders and bond depositors can use the other mint. Passing an address that is not the trader's associated token account reverts the whole transaction.
- **Stale paths return `Ok`.** An inactivity timeout has to persist the `Abandoned` state, so the stale path of `record_activity` and `request_payout` returns success plus return data (`ActivityOutcome` / `PayoutOutcome`) instead of an error, which would roll the state change back.
- **Fee split is validated.** `fee_split_bps` above 10,000 is rejected in `register_product` and `update_product_config`.
- **Large accounts are boxed.** Wide `Accounts` structs use `Box<Account<…>>` to stay under the SBF stack limit.
