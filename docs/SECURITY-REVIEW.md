# Security review: core-vault

Status: internal adversarial review, **not an external audit**. Written against commit `686bd71` plus the changes of this pass (see [Fixes in this pass](#fixes-in-this-pass)). Everything here was checked against the source; where a statement rests on a test, the test is named.

> The brief for this pass said "all 21 instructions". The program has **18** (`lib.rs`): 6 admin, 5 sector, 7 permissionless. All 18 are covered below.

**Reading the tables.** `M` = writable, `S` = must sign. "Why enough" says what makes the check sufficient, not just what it is. Rounding: *floor* = rounds against the payer of the fee / in the vault's favour unless stated. Every `u128` product is of two `u64`-range values, so it cannot overflow `u128`.

Contents: [1. Per-instruction review](#1-per-instruction-review) · [2. The 13 hunted classes](#2-the-13-hunted-classes) · [3. Fixes in this pass](#fixes-in-this-pass) · [4. Compute and stack](#4-compute-and-stack) · [5. Findings](#5-findings)

---

## 1. Per-instruction review

Common to every instruction: classic SPL Token only (`Program<Token>`, `Account<Mint>`, `Account<TokenAccount>` all check the classic owner), `transfer_checked` with the mint's decimals, `overflow-checks = true` in the release profile, and every PDA created either by Anchor `init` or by `utils::pda_account` (both safe against a pre-funded address).

### Admin group (2-of-2: `SL8_ADMIN_PUBKEY` + `ROV_ADMIN_PUBKEY`, compile-time constants)

#### 1.1 `init_vault(usdc_mint, usdt_mint)`

| # | account | M | S | checks | why enough |
|---|---|---|---|---|---|
| 0 | `sl8_admin` | M | S | `address = SL8_ADMIN_PUBKEY` | constant key + `Signer`; pays the rent |
| 1 | `rov_admin` | | S | `address = ROV_ADMIN_PUBKEY` | the second signature |
| 2 | `vault_state` | M | | `init`, seeds `["vault_state", SL8, ROV]`, canonical bump | `init` fails on an existing account; a dusted address is topped up by Anchor |
| 3-4 | `usdc_mint`, `usdt_mint` | | | `UncheckedAccount`; handler: key == arg, owner == Token program, `Mint::unpack` succeeds, `decimals == 6` | all validated before anything is created |
| 5-6 | `usdc_pool`, `usdt_pool` | M | | seeds `["pool", vault_state, mint]`, canonical bump | created with `create_pda_account` (dust-safe), then `initialize_account3` with authority `vault_state` |
| 7-8 | `token_program`, `system_program` | | | `Program<Token>`, `Program<System>` | program ids checked |

Args: `usdc_mint != usdt_mint` (`DuplicateMint`), each equal to its account (`InvalidMint`). Arithmetic: none. State: whole `VaultState` initialised, `sl8_wallet = SL8_ADMIN_PUBKEY`. CPIs: system create/transfer/allocate/assign, token `initialize_account3` x2. **Worst a hostile caller can do:** nothing; both founder signatures are needed and `init` makes it one-shot. Dusting any of the three PDAs cannot block it (`tests-rs/tests/init_vault.rs`, and the fuzz setup dusts them at random).

#### 1.2 `register_product(product_program_id, fee_split_bps, challenge_sizes, max_payout_count, reset_price_bps)`

| # | account | M | S | checks | why enough |
|---|---|---|---|---|---|
| 0 | `sl8_admin` | M | S | `address = SL8` | pays rent |
| 1 | `rov_admin` | | S | `address = ROV` | 2-of-2 |
| 2 | `product_registry` | M | | `init`, seeds `["product_registry", product_program_id]` | one registry per sector id; a second registration is "account in use" |
| 3 | `system_program` | | | `Program<System>` | |

Args: `fee_split_bps <= 10_000` (`InvalidFeeSplit`), `challenge_sizes.len() <= 32` (`TooManyChallengeSizes`), `reset_price_bps.len() <= 8` (`TooManyResetPhases`). Not validated (admin config, see findings SR-09/SR-10): the sector id is not checked to be an executable program, tier sizes/costs may be 0, a reset price may exceed 100% of the size, `max_payout_count` may be 0. Arithmetic: none. State: the registry. **Worst:** admin-only. A mistyped sector id cannot be corrected or closed (there is no close instruction).

#### 1.3 `update_product_config(product_program_id, challenge_sizes, fee_split_bps, max_payout_count, reset_price_bps)`

Accounts `sl8_admin` (S), `rov_admin` (S), `product_registry` (M; seeds `["product_registry", id]`, `bump = stored`). Same argument validation as 1.2. State: tiers, fee split, payout cap, reset prices. **Not** touched: `active`, the pause fields, the request counters. Worst: admin-only; lowering `max_payout_count` under an `Active` trader makes that trader's next `request_payout` fail with `PayoutCapReached` (fuzz-covered).

#### 1.4 `pause_product(product_program_id)`

Accounts as 1.3. `require!(active, ProductAlreadyPaused)`, then `pause(PAUSE_PLANNED_UPGRADE, now)`. State: `active`, `pause_reason`, `paused_since`. Worst: admin-only. Cannot overwrite an existing pause (so a reconciliation pause keeps its reason and time).

#### 1.5 `reactivate_product(product_program_id)`

Accounts as 1.3. No state precondition: `resume(now)` banks `max(now - paused_since, 0)` into `total_paused_secs` when `paused_since > 0`, clears the pause, sets `active`. Idempotent on an active product. Worst: admin-only. Note it also lifts a **reconciliation** pause without any check that the books now agree (by design, "manual review"); `reconcile_product` simply pauses it again if they still disagree.

#### 1.6 `admin_withdraw_marketing_funds(pool, amount)` — the one documented exception to "no admin key on money"

| # | account | M | S | checks | why enough |
|---|---|---|---|---|---|
| 0 | `sl8_admin` | | S | `address = SL8` | |
| 1 | `rov_admin` | | S | `address = ROV` | 2-of-2 |
| 2 | `vault_state` | M | | seeds + `bump = stored` | the only vault; carries `sl8_wallet`, floors, totals |
| 3 | `mint` | | | `Account<Mint>`, `key == vault_state.mint_of(pool)` | pool and mint are chosen together by the `pool` argument |
| 4 | `pool_token_account` | M | | `key == vault_state.pool_of(pool)` | cannot name any other source |
| 5 | `sl8_token_account` | M | | `owner == vault_state.sl8_wallet && mint == mint` | the only destination; no argument or remaining account can change it |
| 6 | `token_program` | | | `Program<Token>` | |

Args: `pool` (enum, Borsh rejects other variants), `amount > 0` (`ZeroAmount`), `amount <= live - max(stored_floor, ceil(live * 25%))` (`WithdrawalExceedsReserve`; `live` is re-read with `reload()`). Arithmetic: `ceil_bps` is a `u128` ceil (rounds the reserve **up**, so the admins get the smaller amount); `saturating_sub` returns 0 when the balance is below the stored floor; the `marketing_withdrawn_*` counters use `checked_add`. State: the counters. CPI: one `transfer_checked`, signed by the `vault_state` PDA. **Worst (both admin keys):** take 75% of a pool's live balance per call, repeatedly (geometric: `0.25^n` remains), with no deduction for open claims or bonds, at any time. Claims then settle pro rata against what is left. This is the documented exception; see THREAT-MODEL. **Worst (one key):** nothing directly — but see SR-01 for a one-key path to pool money through bonds.

### Sector group (CPI from a registered sector program; `sector_authority` PDA signature)

`assert_sector_authority(signer, registry.product_program_id)` recomputes `derive_sector_authority(registry.product_program_id)` from the shared crate and requires the signer to equal it. A PDA can only sign through `invoke_signed` of the program it belongs to, so a valid signature proves the call came from the registered sector program. The registry address is derived from the `product_program_id` argument, and the registry stores the same id, so product A's authority cannot act on product B's registry, traders or claims.

#### 1.7 `deposit_fee(amount, product_program_id, challenge_id, trader_wallet, account_size)`

| # | account | M | S | checks | why enough |
|---|---|---|---|---|---|
| 0 | `sector_authority` | | S | `assert_sector_authority` | caller identity |
| 1 | `product_registry` | M* | | seeds `["product_registry", id]`, `bump = stored` | *writable but never written (info) |
| 2 | `trader_state` | M | | `init`, seeds `["trader_state", id, trader_wallet, challenge_id_le]` | a reused id is "account in use" |
| 3 | `payer` | M | S | `Signer` | pays the rent of 2 |
| 4 | `system_program` | | | `Program<System>` | |
| 5 | `vault_state` | | | seeds + `bump = stored` | source of mints, pools, `sl8_wallet` |
| 6 | `trader` | | S | `address = trader_wallet` | the payer of the fee must consent |
| 7 | `trader_token_account` | M | | `owner == trader && mint == mint` | only the signer's own account is debited |
| 8 | `mint` | | | `Account<Mint>`, `key in {usdc, usdt}` | |
| 9 | `pool_token_account` | M | | `key == pool_for(mint)` | |
| 10 | `sl8_token_account` | M | | `owner == sl8_wallet && mint == mint` | |
| 11 | `token_program` | | | `Program<Token>` | |

Args: `(account_size, amount)` must be an exact registered tier (`InvalidChallengeTier`); `trader_wallet` is bound by the `trader` signer; `product_program_id` by the registry seeds. Arithmetic: `pool = floor(amount * fee_split_bps / 10_000)` (`u128`, checked mul), `sl8 = amount - pool` (`checked_sub`, fails closed if `fee_split_bps > 10_000`), balance pre-check (`InsufficientTokenBalance`); `paused_secs_at` uses saturating adds. State: a new `TraderState` (Active, clock fields set). CPIs: system create (Anchor), token `transfer_checked` x2 (zero legs skipped), authority = the trader. **Worst:** only the registered sector can call. A malicious sector can make a *signing* trader pay a registered tier price and can burn challenge ids; it cannot move anyone's tokens without their signature, nor choose a destination.

#### 1.8 `deposit_reset(amount, trader_wallet, product_program_id, prev_challenge_id, new_challenge_id, reset_phase)`

Same account set as 1.7 plus `prev_trader_state` (M; seeds with `prev_challenge_id`, `bump = stored`) before `new_trader_state` (`init`, seeds with `new_challenge_id`). `prev == new` fails at `init`. Preconditions: product active, `prev.status == Failed && !prev.reset_used` (`ResetNotAllowed`), `reset_phase < reset_price_bps.len()` (`InvalidResetPhase`), `amount == floor(prev.account_size * bps / 10_000)` (`WrongAmount`; the size comes from the vault's own record, never the sector). Then `plan_payment`, `prev.reset_used = true`, and the new record copies `account_size` and `payout_count` from `prev` (the sector cannot grant extra payouts). Arithmetic as 1.7; the price is a `u128` floor (can round to 0; admin config). CPIs as 1.7. **Worst:** one reset per failed record; the whole transaction reverts if the payment fails, so `reset_used` is never burned without payment.

#### 1.9 `record_activity(trader_wallet, product_program_id, challenge_id)`

Accounts: `sector_authority` (S), `product_registry` (seeds, `bump = stored`, read-only), `trader_state` (M; seeds, `bump = stored`). Preconditions: authority; `status == Active` (`InvalidTraderStatus`). Logic: if idle `> 604_800` s (strict, pause-adjusted) -> status `Abandoned`, return `Ok` + `ActivityOutcome::Abandoned` (an error would roll the write back); else if `now - last < 86_400` -> `Throttled`; else `touch` -> `Recorded`. Does **not** require the product to be active (a paused product freezes the clock anyway). Arithmetic: `saturating_sub` throughout, so a backwards clock gives idle 0. **Worst:** a sector can refresh or abandon its own traders; cannot touch another product's.

#### 1.10 `request_payout(trader_wallet, amount, product_program_id, challenge_id, proposed_request_id)`

| # | account | M | S | checks | why enough |
|---|---|---|---|---|---|
| 0 | `sector_authority` | | S | `assert_sector_authority` | |
| 1 | `product_registry` | M | | seeds, `bump = stored` | counters are bumped |
| 2 | `trader_state` | M | | seeds (wallet, challenge), `bump = stored` | the claim's wallet is the one in these seeds |
| 3 | `vault_state` | M | | seeds, `bump = stored` | counters, `cycle_id` |
| 4 | `payout_claim` | M | | `UncheckedAccount`, seeds `["payout_claim", trader_state, id_le]` | created by hand only after every check, so the stale path leaves nothing behind |
| 5 | `payer` | M | S | `Signer` | claim rent |
| 6 | `system_program` | | | `Program<System>` | |

Order of checks (matters, the stale path returns `Ok`): product active -> `amount > 0` -> `status == Active` -> **stale? then `Abandoned`, `Ok`, no claim** -> `payout_count < max_payout_count` (`PayoutCapReached`) -> `proposed_request_id == payout_count + 1` (`RequestIdMismatch`) -> checked adds. Arithmetic: `payout_count + 1`, `open_claims_count + 1`, `open_claims_total + amount`, `total_requests_emitted + 1`, `total_requested_amount + amount` — all `checked_add` (`MathOverflow`). State: `payout_count`, activity clock, `Graduated` at the cap, registry totals, vault counters, the claim (`kind 0`, `created_in_cycle = cycle_id`). **No tokens move.** **Worst:** the claim amount is whatever the sector says (no per-request or total bound): a compromised sector can queue arbitrary claims for wallets that signed a `deposit_fee` (SR-02), and one request of about `u64::MAX` makes every later `request_bond_payout` and `request_payout` fail with `MathOverflow`, locking bond holders in (**SR-21, high, needs a founder decision**; a tested fix is in `docs/proposed-fixes/`). Request ids are monotonic, so a claim address is never reused.

#### 1.11 `flag_trader_failed(trader_wallet, product_program_id, challenge_id)`

Accounts as 1.9 (registry read-only). `status == Active` -> `Failed`. Allowed while paused (a breach is a breach). **Worst:** a sector can fail its own traders (which makes them resettable once).

### Permissionless group

#### 1.12 `mark_abandoned(trader_wallet, product_program_id, challenge_id)`

Accounts: `caller` (S), `product_registry`, `trader_state` (M), seeds as 1.9. Requires `Active` and strictly stale (pause-adjusted), else `NotAbandonable`. **Worst:** cannot kill a live challenge; flips only genuinely idle ones.

#### 1.13 `begin_heartbeat()`

Accounts: `caller` (S), `vault_state` (M, seeds), `usdc_pool`, `usdt_pool` (`address = vault_state.*_pool`). Requires no cycle open (`CycleInProgress`) and, if a cycle ever started, `now >= cycle_started_at + 432_000` (`HeartbeatTooEarly`, `checked_add`). Snapshots `open_claims_total`, the two pools' sum (`checked_add`), `open_claims_count`; `cycle_id += 1` (checked). **Worst:** anyone can open a cycle, including an empty one, which only consumes the 5-day slot; claims already queued are always in it. Pools cannot be lowered by an outsider, so the snapshot cannot be gamed down.

#### 1.14 `settle_claims()` — remaining accounts: `[claim, trader_usdc_ata, trader_usdt_ata]` x 1..=6

Fixed accounts: `caller` (M, S; receives closed-claim rent), `vault_state` (M), `usdc_mint`, `usdt_mint` (`address =` the vault's), `usdc_pool`, `usdt_pool` (M, `address =` the vault's), `token_program`.
Per claim: owner == this program and writable; deserialises as `PayoutClaim` (discriminator checked); `kind` is 0 or 1; the account address equals `create_program_address(kind-specific seeds, stored bump)`; `created_in_cycle < cycle` (`ClaimNotEligible`); `last_settled_cycle != cycle` (`ClaimAlreadySettled`); both destinations equal the **associated token accounts of `claim.trader_wallet`** (`InvalidTokenAccount` otherwise, a hard error for the submitter only); if either ATA is missing, frozen, re-owned, uninitialised or of the wrong mint the claim is **skipped** (counted as processed, stays owed). Pay: pools reloaded; `target = floor(owed * min(available, owed) / owed_snapshot)` in `u128`; `pay = min(target, live total)`; larger pool first (tie -> USDC), the other tops up; legs `<=` balances and sum to `pay`. `owed` and `open_claims_total` shrink by `checked_sub`; a fully paid claim is closed (lamports to the caller, owner reassigned to the system program, data truncated) and `open_claims_count` shrinks; `cycle_processed_count` bumps once per claim per cycle. Batch shape: non-empty (`EmptyBatch`), multiple of 3 (`InvalidClaim`), at most 6 (`BatchTooLarge`). CPIs: `transfer_checked` signed by `vault_state`. **Worst:** nobody can redirect funds, double-pay a claim, or mark a claim processed with a wrong destination. A hostile claimant can (a) make their own claim skipped by closing/freezing their ATA, (b) grind their wallet so the ATA derivation costs more CU (SR-13), (c) front-run a keeper so the keeper's batch reverts on `ClaimAlreadySettled` (SR-06). An issuer freeze of a *pool* account makes the instruction revert (SR-03).

#### 1.15 `finalize_heartbeat()`

Accounts as 1.13. Requires a cycle open (`NoCycleInProgress`) and `processed == eligible` (`CycleIncomplete`). Sets each floor to `floor(balance * 25%)` (`u128`), `floor_updated_at`, closes the cycle; leaves `cycle_started_at` alone. **Worst:** the floors only ever add protection against the admin withdrawal (`reserve = max(stored floor, ceil(25% of live))`) and never limit claim settlement; anyone can finalize, so no particular keeper is required.

#### 1.16 `reconcile_product(product_program_id)`

Accounts: `caller` (S), `product_registry` (M, seeds, `bump = stored`), `payout_tally` (`UncheckedAccount`, read-only; address must equal `derive_payout_tally(product_program_id)`, `InvalidTally`). Requires an active product (`ProductAlreadyPaused`, so an existing pause keeps its reason/time). `read_tally`: owned by the sector -> parsed with the shared parser (magic + version), unparseable = mismatch; not owned by the sector and empty = `0/0`; not owned and non-empty = mismatch. Both count and total must match. A mismatch **pauses and returns `Ok`**. **Worst:** nobody can pause a product with junk (the address is pinned and the owner decides the contents). Anyone can re-pause a product whose books truly disagree (SR-05). Integration requirement: a sector must update its tally in the same transaction as the accepted `request_payout` (SR-17).

#### 1.17 `deposit_bond(deposit_index, principal, term)`

| # | account | M | S | checks | why enough |
|---|---|---|---|---|---|
| 0 | `depositor` | M | S | `Signer` | pays and owns |
| 1 | `vault_state` | M | | seeds, `bump = stored` | global counter |
| 2 | `depositor_token_account` | M | | `owner == depositor && mint == mint` | |
| 3 | `mint` | | | in `{usdc, usdt}` | |
| 4 | `pool_token_account` | M | | `== pool_for(mint)` | |
| 5 | `sl8_token_account` | M | | `owner == sl8_wallet`, same mint | |
| 6 | `bond_position` | M | | `UncheckedAccount`, seeds `["bond", depositor, index_le]` | created by hand after all checks |
| 7 | `bond_cap_tracker` | M | | `UncheckedAccount`, seeds `["bond_cap", depositor]`; read by hand (program-owned with the right depositor, or empty system account) | no `init_if_needed` (unsafe with pre-funded addresses) |
| 8-9 | `token_program`, `system_program` | | | | |

Args: `principal >= $50` (`BondBelowMinimum`); `deposit_index == tracker.next_deposit_index` (`BondIndexMismatch`); wallet open `<= $50,000` and global open `<= $600,000` (`checked_add`, `BondWalletCapExceeded` / `BondGlobalCapExceeded`); balance `>= principal + fee`; `term` is a Borsh enum. Arithmetic: `fee = ceil(principal * 0.2%)`, pool share `floor(principal / 2)`, SL8 gets the exact remainder **plus** the fee, so `pool + sl8 = principal + fee`. State: tracker (created on first use), position, global counter. CPIs: system create x1-2, token `transfer_checked` x2. **Worst:** a depositor can only lock their own money; indexes never repeat. See SR-01 for the economic exposure.

#### 1.18 `request_bond_payout(deposit_index)`

Accounts: `depositor` (M, S), `vault_state` (M, seeds), `bond_position` (M, `UncheckedAccount`), `bond_cap_tracker` (M, `UncheckedAccount`), `payout_claim` (M, seeds `["bond_claim", depositor, index_le]`), `system_program`. The position and tracker are read by hand: owner == this program, discriminator, `depositor` == signer, `deposit_index` == arg, address == `create_program_address(seeds, stored bump)`. So a missing, closed, foreign, forged or mismatched account is one clean `InvalidBondPosition`. Rules: before `created_at + lock` -> `BondLocked`; `now >= created_at + lock` -> principal; `now >= created_at + term` -> principal + `floor(principal * bps / 10_000)`; fee `ceil(gross * 0.2%)` is *not owed* (stays in the pool); `net = gross - fee`. Effects: tracker and global counters `checked_sub` the principal, `bond_withdrawal_fees_retained += fee`, `open_claims_count/total` `checked_add`, claim created (`kind 1`, `created_in_cycle = cycle_id`), tracker rewritten, position closed (rent to the depositor). **Worst:** only the depositor can withdraw, once per position; claims queue behind the heartbeat like any other.

---

## 2. The 13 hunted classes

Verdicts: **not present** / **present** (a defect, fixed or listed) / **needs-decision** (fixing it would change a locked design). The same review was made independently by seven read-only reviewers (one per lens) and every candidate they raised was challenged by a second, sceptical reviewer; what survived is in the findings list (SR-xx). Each "not present" below names the reason.

| # | class | verdict |
|---|---|---|
| 1 | missing signer / owner / address / seeds checks; `UncheckedAccount`/`AccountInfo`; `remaining_accounts` | **not present** |
| 2 | the same account passed twice as two parameters | **not present** |
| 3 | bump canonicality, non-canonical forgery, seed collisions between PDA types | **not present** (a regression test now guards the collision argument) |
| 4 | revival / re-init after close, dusting, data not zeroed, discriminator confusion | **not present** |
| 5 | overflow, `as` casts, narrowing, rounding in the attacker's favour | **present once: SR-21 (needs-decision)**: a reachable `u64` overflow of `open_claims_total` locks bond exits. No `as` or rounding issue |
| 6 | token-account tricks | **not present**; one external-dependency risk, SR-03 |
| 7 | clock limits, `>=` vs `>`, `i64` | **not present** |
| 8 | denial of service | **not present** for hostile accounts; SR-03 (issuer freeze of a pool), SR-04, SR-06 are needs-decision / accepted |
| 9 | griefing / economic | **present as design exposure**: SR-01, SR-02, SR-05; SR-21 (needs-decision); the rest accepted |
| 10 | authorisation story | **not present** in code; deploy procedure risk documented (SR-15) |
| 11 | error handling hiding state | **not present** |
| 12 | compute and stack | **not present**; table in section 4 |
| 13 | test-only code reaching production | **not present** |

### 2.1 Missing checks; unchecked accounts; `remaining_accounts` (class 1)

* Every signer is a `Signer<'info>`; both admin keys are `Signer` + `address =` a compile-time constant; the sector authority is a `Signer` checked against the registry.
* The only `UncheckedAccount`s: the two mints in `init_vault` (validated by hand before anything is created), `payout_claim` (`request_payout`), `bond_position`/`bond_cap_tracker` (`deposit_bond`, `request_bond_payout`), `payout_claim` (`request_bond_payout`) — all pinned by seeds or re-derived, and read only after an owner + discriminator + address check — and `payout_tally` (`reconcile_product`, address pinned, contents never trusted).
* `remaining_accounts` is used in exactly one place, `settle_claims`; every triple is authenticated as described in 1.14 and a forged or foreign account can only revert the submitter's own transaction.
* Where an Anchor `Accounts` struct reads instruction arguments (`#[instruction(..)]`), every prefix was compared with the handler signature: no seed is bound to the wrong argument.

### 2.2 Same account twice (class 2)

Checked for: both pools, a pool as a destination, a pool as a source, `prev_trader_state == new_trader_state`, `payer == trader`, `payer == claim`, `trader == sl8_wallet`, `trader_token_account == sl8_token_account`, `bond_position == bond_cap_tracker`, `usdc_ata == usdt_ata`, the claim as a destination.

* The two pools are fixed by `vault_state` and `init_vault` rejects equal mints, so they are different accounts.
* A pool is owned by the `vault_state` PDA, which can never sign, so it can never be a `trader`/`depositor` source or a `sl8_wallet` destination.
* `prev == new` fails at `init` (already in use). Position and tracker have different seeds and discriminators.
* `trader == sl8_wallet` (the SL8 admin acting as a trader) only moves SL8's own tokens in a circle plus the pool leg; harmless.
* An ATA is a PDA of the associated-token program and cannot equal a claim or a pool.
* The fuzz tester swaps these accounts on purpose in every instruction (the "swap ..." hostile variants) and they are always rejected with no state change.

### 2.3 PDA bumps and collisions (class 3)

* Every `seeds` constraint uses either a bare `bump` (canonical, found by `find_program_address`) or `bump = x.bump` where `x.bump` was stored at creation (canonical). The hand-checked paths (`settle_claims`, `request_bond_payout`) use `create_program_address` with the bump stored in an account only this program can write. A non-canonical address cannot hold a program-owned account.
* Seeds are concatenated without separators, so cross-type collisions were checked arithmetically. Total seed lengths (all variable parts are fixed width: `Pubkey` 32 bytes, `u64` 8 bytes): `bond_cap` 40, `bond` 44, `product_registry` 48, `bond_claim` 50, `payout_claim` 52, `pool` 68, `vault_state` 75, `trader_state` 84 — **all different**, so two PDA types cannot hash the same preimage, and within a type the layout is injective. Regression guard: `constants::seeds::tests::pda_seed_lengths_are_pairwise_distinct` fails if a future seed makes two lengths equal.
* The sector's own PDAs (`sector_authority`, `payout_tally`) are derived under the **sector's** program id, a different address space.

### 2.4 Revival, dusting, close, discriminators (class 4)

* `close_pda_account` moves all lamports out, reassigns the owner to the system program and truncates the data to 0 bytes, which is Anchor's `close` semantics. A re-funded closed claim/position is a system account with no data; every reader checks owner then discriminator, so it is rejected. A claim listed twice in one batch fails the owner check on the second pass. The fuzz tester runs the same instruction twice in one transaction for every instruction that creates an account, closes one or changes state non-idempotently (the second run must fail and the whole transaction roll back), and checks that closed claims never reappear (invariant 4).
* Claim and position addresses are never legitimately recreated: request ids and deposit indexes only grow.
* Lamport dusting: every creation goes through Anchor `init` or `create_pda_account`, which top up only when `required > have`, then `allocate` + `assign` with the PDA's seeds. The fuzz tester dusts the vault, both pools and the **next** address of every creating instruction before it is created (tags `created_over_dust:*`).
* Discriminator confusion: hand-deserialised accounts are `PayoutClaim` (`settle_claims`), `BondPosition`/`BondCapTracker` (`deposit_bond`, `request_bond_payout`); each uses `try_deserialize`, which checks the 8-byte discriminator, and each cross-checks the stored depositor/kind/bump against the address.
* `TraderState`, `ProductRegistry`, `VaultState`, `BondCapTracker` are never closed (rent stays locked; info, SR-12).

### 2.5 Arithmetic and rounding (class 5)

* Every `as` in program code is a widening cast (`u8/u16/u64 -> u128/usize`). Every `u128 -> u64` goes through `u64::try_from` with an error or is bounded by construction (`pay <= owed`, a fee `<= amount` for bps `<= 10_000`).
* **Reachable overflow (SR-21, high, needs a founder decision):** `open_claims_total` is a `u64` that a sector grows by an amount of its own choosing, and `request_bond_payout` adds to it too. One request of about `u64::MAX` is accepted (an existing test pins that), after which every later checked add overflows and bond holders cannot exit. The proposed fix is a ceiling of 2^62 on the total in `request_payout` (bond claims, at most 1.3 x the $600K cap, always fit above it); it is written, tested and mutation-tested in `docs/proposed-fixes/SR-21-claims-ceiling.patch` but not applied, because it contradicts that existing test.
* Every counter or balance add/sub is `checked_*` (`open_claims_*`, request counters, `payout_count`, bond totals, `marketing_withdrawn_*`, `cycle_id`, the pool sum, rent lamports on close, timestamp additions). The only `+` on `u64` outside `checked_*` is `Settlement::total`, which adds two legs whose sum is `pay <= u64::MAX`.
* Rounding: fee-split pool share **floor** (SL8 takes the exact remainder: no unit created or lost); bond deposit fee **ceil**; bond withdrawal fee **ceil** and *not owed*; bond interest **floor**; per-claim settlement **floor** (dust stays owed); marketing reserve **ceil** (admins get less); reset price **floor** (can be 0, admin config). No rounding favours an outside attacker: splitting a bond into many only raises the rounded-up fees.
* Settlement conservation: `target <= owed`; `pay <= live pool total`; each leg `<=` its pool; the legs sum to `pay`. Per cycle, `sum floor(owed_i * num / den) <= num <= available at the snapshot` because the eligible claims sum to `den`. Tested exhaustively (`utils::settlement` unit tests) and by fuzz invariant 11.

### 2.6 Token-account tricks (class 6)

* **Token-2022**: every token program is `Program<Token>` and every mint/account is an `anchor_spl::token` type, so Token-2022 accounts and programs fail the owner check. `destination_usable` rejects accounts not owned by the classic program (the fuzz tester plants a Token-2022-owned account at an ATA address).
* **Decimals**: `init_vault` requires 6; mints are pinned by address afterwards; transfers are `transfer_checked`.
* **Delegate / close authority on a victim ATA**: irrelevant for incoming transfers and only the ATA owner can set them. A re-owned ATA fails `a.owner == wallet` and is skipped.
* **Frozen**: a frozen destination is skipped, never reverts the batch. A frozen **source** (trader) or SL8 account only reverts that caller's transaction. A frozen **pool** reverts `settle_claims` (SR-03).
* **Native mint / mint authority**: the native mint has 9 decimals and cannot pass `init_vault`; the vault's mints are fixed forever by address, so mint-authority tricks cannot introduce a new mint. Mint/freeze authority of the real USDC/USDT is the issuer's (SR-16).
* **ATA owned by someone else / wrong address**: derived and compared with `require_keys_eq` (a hard error for the submitter only).

### 2.7 Time (class 7)

`> 7 days` for inactivity (strict, matches the README), `>= created_at + lock` unlocked, `>= created_at + term` matured, `>= started + 432_000` for the next heartbeat (inclusive), throttle `< 86_400`. The fuzz tester warps to exactly one second either side of each of these boundaries (tags `bond:withdraw_exactly_on_boundary`, `begin:exactly_at_gap`, and the stale boundary) and the clock also wobbles backwards by up to an hour (`warp:backwards`); all `saturating_sub`/`max(0)` paths were exercised. `i64` additions are `checked_add` where an attacker value could enter. `paused_since > 0` is a sound sentinel because a pause stores a real positive timestamp and cannot be started twice.

### 2.8 Denial of service (class 8)

* **Can a hostile account stop begin/settle/finalize?** For `begin`/`finalize`: no (the caller is only a fee payer; the pools are address-pinned). For `settle_claims`: a claim whose destinations are unusable is skipped (counted processed), so a trader cannot block finalize by closing/freezing/re-owning an ATA; a wrong destination is a hard error only for the submitter. Anyone can settle any single claim alone, so no batch composition can block progress.
* **Permanently unsettleable claim blocking finalize?** No, with one external exception: a *pool* account frozen by the issuer (SR-03).
* **Junk claims filling a cycle?** Trader claims come only from registered sectors. Bond claims cost the attacker a locked `$50` principal each (returned minus 0.4% fees and rent) with a hard lock of 90+ days and a $600K global cap, so at most ~12,000 junk claims exist at once = ~2,000 settle transactions of 6; a cost, not a block.
* **Compute**: batch of 6 ~113,000 CU typical (section 4); ground wallets raise the ATA derivation (SR-13) but a single claim always fits.
* **Cycle-slot burning**: anyone can begin an empty cycle and so consume a 5-day slot; claims already queued are always eligible in it, so this can only delay *new* claims by at most one gap (SR-08).

### 2.9 Griefing and economics (class 9)

* **Dusting claims**: claim addresses are derived from monotonic ids, so dust cannot block creation and is returned on close.
* **Forced mismatch pause on a healthy product**: impossible unless the sector's tally is wrong or updated non-atomically (SR-17). Junk cannot pause (address pinned, owner decides the contents).
* **Front-running `begin_heartbeat`**: possible and harmless: the claims queued are all eligible; the pools cannot be lowered by an outsider.
* **Tiny claims rounding to 0 forever**: while the cycle ratio is below 1 a claim of 1 base unit pays 0 each cycle and stays open; it costs nothing to carry and clears as soon as a cycle has ratio 1 (SR-08).
* **Dilution**: all claims, trader and bond, have equal priority and share one ratio per cycle, so a bond run dilutes trader payouts and vice-versa (THREAT-MODEL). **SR-01** is the sharp edge of this.

### 2.10 Authorisation (class 10)

* `sector_authority` is derived from `registry.product_program_id` with the shared crate's `derive_sector_authority`; the registry address is derived from the same argument. It is not self-reported.
* Both admin keys are checked by `address =` constants plus `Signer`; the vault PDA seeds contain both keys, so the real and `localnet` builds derive different vaults.
* **Upgrade authority**: not part of the program; whoever holds it can replace the logic and so every guarantee here. See DEPLOY-CHECKLIST for the procedure and SR-15. Nothing in the code was changed for this.

### 2.11 Error handling (class 11)

Paths that must persist state despite an "error" return `Ok` with return data: the stale branch of `record_activity` and `request_payout` (`Abandoned` is stored), and the mismatch branch of `reconcile_product` (the pause is stored). Every other failure is an `Err` and Solana reverts the whole transaction, so no half-written state is possible: the fuzz tester asserts that every rejected call (honest, hostile and composite) leaves every non-fee-payer account byte-for-byte unchanged (invariant 9). No `Ok` was found that should have failed: the model-vs-chain comparison runs after every step.

### 2.12 Compute and stack (class 12)

See section 4. `scripts/anchor-build-checked.sh` reports no stack-frame warning for either build; wide `Accounts` structs are boxed.

### 2.13 Test-only code in production (class 13)

The `localnet` feature only swaps the two admin pubkey constants. It is not in `default`, `tests-rs` is a separate workspace (so Cargo feature unification cannot leak it into `anchor build`), and `scripts/verify-deploy-build.sh` builds the default program and fails unless the `.so` contains both real keys and neither test key. This pass additionally greps the default `.so` for the two test pubkeys (see section 4).

<a id="fixes-in-this-pass"></a>
## 3. Fixes in this pass

**One real defect was found, SR-21 (high), and it is deliberately NOT fixed in this commit.** It was found by the tester's wider seed search (seed 214), not by the manual review or the read-only reviewers, who had rated the missing amount bound "info". A fix exists, is written test first and is mutation-tested, but it makes `request_payout` refuse an amount that an existing test (`payout_claims::the_total_cannot_overflow_u64`) pins as accepted. The standing rules forbid weakening an existing test and send changes to a locked behaviour to the founder, so the fix ships as a proposal: `docs/proposed-fixes/SR-21-claims-ceiling.patch` (apply with `git apply`; it also updates that one test and the fuzz model). Everything else in the pass is documentation, comments and tests.

What the pass changed:

| change | kind | test |
|---|---|---|
| `request_bond_payout.rs`: the comment said the bond **lock** comes from the position's own copy; only `interest_bps` is copied, the lock and maturity come from `position.term` and the `BOND_*_LOCK_SECS` / `BOND_*_TERM_SECS` constants | comment (SR-19) | none (comment) |
| Stale doc comments: `lib.rs` layout (state and utils lists), `constants/admin.rs` and `utils/auth.rs` (instruction lists), `errors.rs` (`Unauthorized`, `TooManyChallengeSizes`, `ProductAlreadyRegistered`, `BondTermInvalid`), `constants/limits.rs`, `state/product_registry.rs`, `reactivate_product.rs` (references to "Module 1", "the brief", "Module 3+"); three informational fields marked as such (`BondPosition.mint`, `PayoutClaim.product_program_id`, `VaultState.floor_updated_at`) | comments (SR-19) | none (comments) |
| `VaultError::ZeroAmount` message: "payout amount must be greater than zero" -> "amount must be greater than zero" (the same error is returned by `admin_withdraw_marketing_funds`). The numeric code is unchanged | message text | `admin_withdraw.rs` asserts the code, not the text |
| README: accounts table completed (bond accounts, claim kind 1, full VaultState/TraderState descriptions); the Bonds section no longer says "no admin key can touch bond money" without the exception; the Design notes no longer say only three instructions move tokens; compute figures updated to the measured 122,000 / 242,000 CU; testing and security-documentation sections added | docs (SR-19) | none |
| `constants/seeds.rs`: `pda_seed_lengths_are_pairwise_distinct` and `every_literal_seed_fits_the_per_seed_limit` | regression guard (SR-20) | shown red below |
| New tests: `compute_budget.rs` (every instruction's CU, with ceilings), `known_exposures.rs` (runnable reproductions of SR-01 to SR-06 and SR-21, each pinning today's behaviour); `docs/proposed-fixes/SR-21-claims-ceiling.patch` (the tested SR-21 patch, not applied) | tests, proposal | n/a |

**SR-21: the proposed fix, red then green.** Before any fix, one `request_payout` of `u64::MAX - 5` is accepted: `open_claims_total` becomes 18,446,744,073,709,551,610 and then a bond holder's `request_bond_payout` returns `Custom(6016)` (`MathOverflow`), as does a second product's `request_payout` of 10 (`known_exposures::sr21_*` pins this). With only a new error variant and constant added (no check), the five new `claims_ceiling` tests ran **3 failed, 2 passed** (`expected failure, but the transaction succeeded` for the hostile request). With the ceiling check added they ran **5 passed**, and the whole LiteSVM suite ran 377 passed, 1 failed: the one failure was `the_total_cannot_overflow_u64`, which pins the vulnerable behaviour. The patch updates exactly that test (same scenario; it now expects `ClaimsCeilingExceeded`, and that the counter stops exactly at the ceiling) and passes. Mutation testing of the proposed fix: 8 mutants (check removed, boundary off by one, constant 2^63, ceiling ignoring what is already open; in the fuzz tester and in the full suite), all killed.

**Sensitivity of the new seed-length guard (red, then green).** With `BOND_CAP_SEED` temporarily lengthened to `b"bond_cap_xxx"` (12 bytes, so `bond_cap` = 44 = `bond`), `pda_seed_lengths_are_pairwise_distinct` fails with `bond and bond_cap PDAs would have the same total seed length (44)`; restored, it passes.

**Dead code / unused errors.** Nothing was deleted. Provably unused (grep over `programs/core-vault/src`, no construction site): `VaultError::ProductAlreadyRegistered` (comment-only mention) and `VaultError::BondTermInvalid` (`BondTerm` is a Borsh enum, so an unknown term never reaches the handler). Both are kept on purpose: removing a variant renumbers every later error code, which every client and test decodes. Both docs now say so. The three informational fields above are written but never read back; removing them changes account layouts, which is a migration, not a cleanup.

**README claims checked against the source.** Every number in the README (gap 432,000 s, batch 6, 1,106 bytes, the 7-day window, the 180/270-day terms and 90/135-day locks, 20%/30%, $50 / $50K / $600K, the 0.2% fees and their rounding, the worked examples 1,002 / 1,197.6 / 998 / 1,297.4, the reconcile account order and errors, the withdrawal formula and errors) matches the code. The statements that did not match are the ones corrected in the README row of the table above.

## 4. Compute and stack

Measured by `tests-rs/tests/compute_budget.rs` on the real program, one transaction per row (rows that derive a PDA from a freshly generated key vary by about 1,500 CU per extra bump tried between runs); each row asserts a ceiling (200,000 CU, the default per-instruction limit; 400,000 for a full settle batch).

| instruction | measured case | CU | bump searches (`find_program_address`) |
|---|---|---:|---|
| `init_vault` | creates the vault and both pools | 33,834 | 3 |
| `register_product` | largest registry (32 tiers, 8 reset phases) | 12,002 | 1 |
| `update_product_config` | largest registry | 9,419 | 0 |
| `pause_product` | | 4,716 | 0 |
| `reactivate_product` | | 4,718 | 0 |
| `admin_withdraw_marketing_funds` | one transfer | 16,953 | 0 |
| `deposit_fee` | both token legs, creates the trader state | 35,188 | 1 |
| `deposit_reset` | both token legs, creates the new state | 41,086 | 1 |
| `record_activity` | refresh | 7,862 | 0 |
| `request_payout` | creates the claim | 18,200 | 1 |
| `flag_trader_failed` | | 7,536 | 0 |
| `mark_abandoned` | | 6,184 | 0 |
| `reconcile_product` | match / mismatch (pauses) | 9,149 / 10,293 | 1 (the sector's tally PDA) |
| `begin_heartbeat` | | 5,661 | 0 |
| `settle_claims` | 1 claim | 24,121 | 2 per claim |
| `settle_claims` | 6 claims, both pools paying (typical wallets) | 112,797 | 12 |
| `settle_claims` | 6 claims, wallets ground to 7+ tries per ATA (`settle_batch.rs`) | 242,481 | 12 |
| `finalize_heartbeat` | | 5,645 | 0 |
| `deposit_bond` | first deposit (creates tracker and position) | 36,371 | 2 |
| `deposit_bond` | later deposit | 35,016 | 2 |
| `request_bond_payout` | matured, creates the claim | 14,872 | 1 |

**Worst case.** Every `find_program_address` costs about 1,500 CU for each bump tried. A caller that chooses its own key (a trader or depositor) can only make *its own* transaction dearer. The one place where somebody else's key sets the cost for a *third party* is `settle_claims`, where the claimant's wallet decides the two ATA derivations (SR-13): a claim whose wallet is ground to 30 tries per ATA costs a keeper about 90,000 CU, still a single transaction, and the transaction limit (1.4 million CU) exceeds the cost of even a 255-try derivation per ATA, so **any single claim can always be settled alone**. A batch of 6 ground claims can exceed 400,000 CU; the keeper then retries with smaller batches.

**Stack.** `scripts/anchor-build-checked.sh` prints no stack-frame warning for the default or the `localnet` build. Wide `Accounts` structs are boxed.

**Test keys in the deployable binary.** `scripts/verify-deploy-build.sh` passes on the default-feature `.so`; a separate scan of its raw bytes finds neither test pubkey and both real admin pubkeys.

**Account sizes.** `ProductRegistry` is the only variable-size account and is allocated at its maximum (32 tiers, 8 reset phases); both limits are enforced before serialisation (`TooManyChallengeSizes`, `TooManyResetPhases`). `ProductRegistry::SPACE` is 621 bytes; no account exceeds it.

## 5. Findings

Severity: critical / high / medium / low / info. Status: **fixed** (with the test or change named), **needs founder decision** (fixing it would change a locked design or product rule; a runnable reproduction is in `tests-rs/tests/known_exposures.rs`), **accepted, documented**.

No critical finding. One **high** defect in the program was found (SR-21, an overflow that locks bond exits); it is **not applied** because the fix contradicts an existing test, so it is listed as needs-decision with a tested patch. Every other class of section 2 came out "not present" for the code as written. What remains is design exposure (SR-01, 02, 03, 05, 14, 15, 18, 21) and documentation drift (fixed).

| ID | severity | status | title |
|---|---|---|---|
| SR-01 | **high** | **needs founder decision** | One key (SL8) can pull pool money by recycling its own bonds; bypasses the 2-of-2 and the 25% reserve |
| SR-02 | medium (high if a sector is compromised) | **needs founder decision** | `request_payout` trusts the sector's amount; a consistent over-report is invisible to reconciliation |
| SR-03 | medium | **needs founder decision** | A frozen payout pool (issuer action) makes every `settle_claims` revert and the cycle can never finish; no escape hatch |
| SR-04 | low | **needs founder decision** | A claim needs both of the trader's ATAs usable even when one pool would pay; a trader without both is skipped every cycle |
| SR-05 | medium | **needs founder decision** | Registry request counters only grow: an over-reporting sector can never reconcile again; no repair instruction |
| SR-06 | low | accepted, documented | `ClaimAlreadySettled` / `ClaimNotEligible` are hard errors: a front-runner reverts a keeper's batch |
| SR-07 | info | accepted, documented | A mid-cycle admin withdrawal makes the order of payments inside one cycle matter (part of the documented exception) |
| SR-08 | low | accepted, documented | Nuisance within the design: empty-cycle slot burning, junk bond claims, 0-paying tiny claims |
| SR-09 | info | accepted, documented | Admin config can create free tiers, free resets, a reset price above 100%, `max_payout_count = 0`; the sector id is not checked to be a program |
| SR-10 | info | accepted | Lowering `max_payout_count` under an `Active` trader blocks its next request; a reset can inherit a `payout_count` at or above the cap |
| SR-11 | info | accepted | `product_registry` is writable in `deposit_fee` / `deposit_reset` but never written (lock contention only) |
| SR-12 | info | accepted, documented | `TraderState`, `ProductRegistry`, `VaultState`, `BondCapTracker` are never closed; three fields are informational; `ProductAlreadyRegistered` and `BondTermInvalid` are never returned (kept for stable error codes) |
| SR-13 | low | accepted, documented | Wallet grinding raises the ATA-derivation compute in `settle_claims` (one claim < 45,000 CU; a full batch 242,000 CU measured) |
| SR-14 | medium | **needs founder decision** | No admin-key rotation or recovery: both keys are in the binary and in the vault PDA seeds |
| SR-15 | high | **needs founder decision / procedure** | The upgrade authority is total control of the program; handling is in DEPLOY-CHECKLIST |
| SR-16 | low | accepted | The issuers can freeze or blacklist any token account (ATAs, SL8's account, the pools) |
| SR-17 | low | accepted (integration requirement) | A sector that updates its tally in a later transaction than the vault CPI can be paused by anyone in the gap |
| SR-18 | medium | **needs founder decision** (open pre-deploy decision) | SL8's revenue lands in token accounts owned by the SL8 admin key itself (`sl8_wallet = SL8_ADMIN_PUBKEY`) |
| SR-19 | info | **fixed** | Documentation drift: stale comments and README statements (list in section 3) |
| SR-20 | info | **fixed** (test) | The PDA seed-collision argument is now enforced by `constants::seeds::tests::pda_seed_lengths_are_pairwise_distinct` |
| SR-21 | **high** | **needs founder decision** (tested patch ready) | One `request_payout` with an amount near `u64::MAX` saturates `open_claims_total`; afterwards every `request_bond_payout` and every other product's `request_payout` fails with `MathOverflow`: bond principal is locked in until that claim is paid down (practically never) |

### Needs founder decision, in plain words

**SR-21 (high): one huge sector request locks every bond exit.** `request_payout` accepts any amount. The vault keeps the sum of all open claims in a 64-bit counter. A sector request of about 18.4 quintillion base units (a sector bug that wraps, or a malicious sector) is accepted and fills that counter; after that every attempt to add any further claim, a bond holder's withdrawal included, overflows and fails. The bond holder's principal stays in its position until the huge claim is paid down, which with a pool of real size is effectively never. Reproduction: `known_exposures::sr21_*`. Fix, ready and tested: refuse a `request_payout` that would take the total above 2^62 base units (about 4.6 trillion coins, more than all USDC and USDT in existence); bond claims fit above the ceiling by construction (`docs/proposed-fixes/SR-21-claims-ceiling.patch`, `git apply` it). It is not applied because an existing test pins the old behaviour. Decide, then apply.

**SR-01: one key can take pool money through bonds.** When anyone opens a bond, half the principal goes into the payout pool and the other half goes straight to SL8's token account, which is owned by the SL8 admin key. A bond withdrawal after the hard lock pays the *whole* principal back out of the pool. So the SL8 key alone, with no second signature and ignoring the 25% reserve, can open a bond from its own wallet, get half back at once, wait 90 days, withdraw, and end up with about half the principal of other people's trader-fee money. Reproduction: `known_exposures::sr01_*` (SL8 ends 498 USDC richer, the pool 498 USDC poorer on a 1,000 USDC bond). Bounded by $50K per wallet, $600K open in total, the lock, and the pro-rata dilution that applies to the claim like any other. It contradicts "no admin key on money" for a *single* key. Options: send SL8's bond share to a time-locked or 2-of-2 account; pay bond interest and principal shortfall from SL8's share rather than the pool; refuse `sl8_wallet` as a depositor (does not stop other wallets SL8 controls); lower the caps.

**SR-02: the vault believes the sector's amount.** A registered sector, or a bug in one, can queue a claim of any size for a wallet that bought a challenge. The pro-rata rule then gives that claim almost the whole pool (`known_exposures::sr02_*`: a claim 1,000x the pool takes more than 999 of 1,000 USDC and starves an honest 100 USDC claim). Reconciliation does not see it because the sector's own tally matches. Since SR-21 the sum of all open claims is bounded by 2^62 base units, which stops the overflow lock-out but is not a business limit. Options: a per-request cap tied to the tier size (for example a multiple of `account_size`) set at registration; a per-product per-epoch cap; a delay between queueing and eligibility with a 2-of-2 cancel.

**SR-03: no way out of a frozen pool.** The USDC and USDT issuers can freeze any token account. If a pool is frozen, every `settle_claims` that needs it fails, so the cycle never finishes and nothing in the program can skip or reset it (`known_exposures::sr03_*`). Only a program upgrade could help. Options: a 2-of-2 `abort_cycle` after N days that closes the cycle without paying; treat a frozen pool as "pay from the other pool only".

**SR-04: both ATAs are required.** A trader with a USDC account but no USDT account is skipped every cycle even if the USDC pool could pay in full (`known_exposures::sr04_*`). They are paid the cycle after they create the second account. Option: require only the account of the pool(s) that actually pay.

**SR-05: a wrong tally can never be repaired.** The vault's counters only grow and no instruction changes them. A sector whose tally ran *ahead* of the vault (counted a request the vault never accepted) is re-paused by every reconcile, even after the admins reactivate it, and must be replaced by a new product (`known_exposures::sr05_*`). Option: a 2-of-2 instruction that records an acknowledged difference.

**SR-14 / SR-15: keys.** There is no way to rotate an admin key: both are compiled in and are part of the vault's address. A lost key permanently disables admin actions; a stolen one cannot be revoked. The upgrade authority can replace the program, so it dominates every other guarantee. Decide the custody plan before mainnet (DEPLOY-CHECKLIST, section 3).

**SR-18: where SL8's revenue lands.** `init_vault` sets the revenue destination to the SL8 admin key's own token accounts. Anyone who obtains that single key takes the revenue and, via SR-01, more. Consider a separate treasury key set at `init_vault` (a one-line change to the argument list that would alter the instruction's interface, hence a decision).
