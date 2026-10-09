# Deploy checklist: core-vault (devnet, then mainnet)

Companion to [SECURITY-REVIEW.md](SECURITY-REVIEW.md) and [THREAT-MODEL.md](THREAT-MODEL.md). Do the devnet column completely and watch it for a full heartbeat cycle before starting the mainnet column.

> **Every step marked `FOUNDER ONLY - Claude Code must never touch` needs a real key (an admin key, the program keypair, or the upgrade authority). An assistant must never read, print, copy, generate or use those keys, and must not run those commands.** Prepare the command, hand it over, and let a founder sign on their own device.

Conventions: `$SO` = `target/deploy/core_vault.so`. All hashes are SHA-256. Record every value you are told to record in a deploy log kept outside the repo.

## 0. Preconditions

- [ ] Clean `main` at the commit you intend to ship; `git status` is empty; `git log -1` recorded.
- [ ] `scripts/test-all.sh` is green (localnet build, `cargo test` x2, LiteSVM suite including `invariants_fuzz`, TypeScript suite).
- [ ] You have read the open items in SECURITY-REVIEW.md. In particular decide **before mainnet**: SR-21 (one huge sector request locks every bond exit; a tested patch is in `docs/proposed-fixes/`), SR-01 (single-key bond recycling), SR-02 (sector payout amounts are trusted), SR-03 (no escape from a frozen pool), SR-14/SR-15 (key rotation, upgrade authority), and the open treasury decision (SR-18: SL8 revenue lands in token accounts owned by the SL8 admin key).
- [ ] Tool versions recorded: `solana --version`, `anchor --version`, `cargo build-sbf --version`, `rustc --version`. Use the same toolchain for devnet and mainnet.

## 1. Program identity

1. **FOUNDER ONLY - Claude Code must never touch:** generate the **program keypair** on a clean machine (`solana-keygen new -o <path-outside-the-repo>`). Back it up offline. Use the **same** program keypair on devnet and mainnet so the same `.so` bytes are what you rehearse and what you ship.
2. Put the public key into `declare_id!` (`programs/core-vault/src/lib.rs`) and `[programs.*]` in `Anchor.toml`. The program id is baked into the `.so`: a mismatch fails every instruction with `DeclaredProgramIdMismatch`.
3. Commit that change on its own ("chore: program id for <cluster>").
4. The throw-away `target/deploy/core_vault-keypair.json` created by the build is **not** the program keypair. It is git-ignored. Do not deploy with it.

## 2. Build and verify the artefact (default features only)

1. Build with default features (real admin keys): `scripts/anchor-build-checked.sh`. It must print no stack-frame warning for either build and the line `OK: no stack-frame overflow reported (default build)`.
2. Run **`scripts/verify-deploy-build.sh`**. It must end with `OK: contains both real admin keys and no test keys.` Any `FAIL` line means this is not a deployable build: stop.
3. After deploying, run the same gate on the bytes that are really on chain: `scripts/verify-deploy-build.sh dump.so` (step 4.3). It takes a path and skips the build.
4. **Record the `.so` hash** (`shasum -a 256 $SO`) and the commit id in the deploy log. This hash is what you compare against the deployed program (step 4.3).
5. Never deploy anything from `target/test-deploy/` (the `localnet` build, public test keys).
6. Reproducibility: rebuild once more on a second machine (or in a clean checkout) with the same toolchain and compare the hash. A difference means the build is not reproducible; find out why before deploying.

## 3. Upgrade authority

The upgrade authority can replace the program, so it is the strongest key in the system (SR-15). Plan, in order:

1. **FOUNDER ONLY - Claude Code must never touch:** at first deploy the authority is the deployer wallet. Use a dedicated hardware-wallet deployer; never a hot key and never one of the two admin keys.
2. **Devnet:** deploy with the final authority arrangement you intend for mainnet, so the move below is rehearsed.
3. **Mainnet, phase 1 (launch):** authority = the deployer hardware wallet.
4. **Mainnet, phase 2 (after the bug-bounty / audit window you choose):** **FOUNDER ONLY - Claude Code must never touch:** `solana program set-upgrade-authority <PROGRAM_ID> --new-upgrade-authority <MULTISIG>` to a multisig (a Squads-style 2-of-3 or better, members on separate devices). Check with `solana program show <PROGRAM_ID>`.
5. **Mainnet, phase 3 (when the design is final):** **FOUNDER ONLY - Claude Code must never touch:** make it immutable with `solana program set-upgrade-authority <PROGRAM_ID> --final`. This is irreversible, and it also removes the only fix for SR-03 (a frozen pool); decide with that in mind.
6. Write down who holds each key, where it is backed up, and the recovery plan. Losing an admin key has no on-chain recovery (SR-14).

## 4. Deploy

1. **FOUNDER ONLY - Claude Code must never touch:** `solana program deploy $SO --program-id <PROGRAM_KEYPAIR> --upgrade-authority <AUTHORITY> --url <devnet|mainnet-beta>` (pay with the deployer wallet; set a priority fee on mainnet).
2. `solana program show <PROGRAM_ID> --url <cluster>`: authority, data length, last deploy slot are as intended.
3. **Compare the deployed bytes with the recorded hash:** `solana program dump <PROGRAM_ID> dump.so --url <cluster>`, then `shasum -a 256 dump.so`. The on-chain program is zero-padded to its allocated length; compare the first `$SO`-length bytes (`head -c $(wc -c < $SO) dump.so | shasum -a 256`). It must equal the hash recorded in 2.4.
4. Smoke test with a throwaway client against the cluster: an unsigned `init_vault` must fail with a missing-signature error, never succeed.

## 5. One-time initialisation, in this order

1. **Mints.** Confirm the USDC and USDT mint addresses for the target cluster from the issuers' official documentation (on devnet use mints you control or the devnet faucet mints). Both must be classic SPL Token mints with **6 decimals**; `init_vault` rejects anything else.
2. **FOUNDER ONLY - Claude Code must never touch:** `init_vault(usdc_mint, usdt_mint)` signed by **both** admin keys (SL8 pays the rent). It creates `VaultState` and the two pool token accounts. Record the vault PDA and the two pool addresses.
3. **SL8 token accounts.** `deposit_fee`, `deposit_reset` and `deposit_bond` pay SL8 into *any existing token account owned by `vault_state.sl8_wallet`* (= the SL8 admin key). Create the SL8 admin key's USDC and USDT associated token accounts before the first purchase. **FOUNDER ONLY - Claude Code must never touch** (decides which account receives revenue; see the treasury decision).
4. **FOUNDER ONLY - Claude Code must never touch:** `register_product(product_program_id, fee_split_bps, challenge_sizes, max_payout_count, reset_price_bps)` per sector, signed by both admins. Double-check the sector id: a registry cannot be corrected or closed. Check `fee_split_bps <= 10_000`, no zero-cost tier unless intended, reset prices sensible.
5. **The sector creates its payout tally** (`derive_payout_tally(product_program_id)`, owned by the sector, initialised `0 / 0` with `PayoutTally::write_into`) **before its first `request_payout`**. An uninitialised or missing tally with a non-zero request count pauses the product at the next `reconcile_product`. The sector must update the tally **in the same transaction** as every accepted `request_payout` (SR-17).
6. **Smoke purchase**, a payout request and a full heartbeat with a tiny amount on the target cluster before announcing anything: `deposit_fee` -> `request_payout` -> `reconcile_product` -> `begin_heartbeat` -> `settle_claims` -> `finalize_heartbeat`.
7. **First `begin_heartbeat`:** the first cycle may start immediately; every later cycle needs 432,000 s (5 days) from the previous *start*.

## 6. Keeper duties

Anyone can run these; run at least two independent keepers. All instructions are permissionless.

| when | do | notes |
|---|---|---|
| before **every** `begin_heartbeat` | `reconcile_product` for **every** registered product | skip products that are already paused (it fails with `ProductAlreadyPaused`, which is fine). Begin is permissionless, so a stranger may open the cycle first; that is harmless (claims are paid either way) |
| every 5 days + a margin (gap check is inclusive: `now >= start + 432_000`) | `begin_heartbeat` | fails with `CycleInProgress` / `HeartbeatTooEarly`; do not retry in a loop |
| during a cycle | `settle_claims` in batches of **at most 6** claims, with `SetComputeUnitLimit(400_000)` | pass `[claim, trader USDC ATA, trader USDT ATA]` per claim; the ATAs must be the *associated* token accounts of the claim's `trader_wallet`. A wrong address reverts the whole transaction; an unusable (missing/frozen/re-owned) account just skips that claim |
| | **simulate before sending** | another keeper may have settled a claim first (`ClaimAlreadySettled` reverts your whole batch); re-read the claims and rebuild |
| | on a revert, retry with batch size 1 | any single claim always fits, even with a ground wallet (measured: a full batch of 6 is ~113k CU typical and ~242k CU with ground wallets; one ground claim is ~44k) |
| when `cycle_processed_count == cycle_eligible_count` | `finalize_heartbeat` | fails with `CycleIncomplete` until every eligible claim was processed. Skipped claims count as processed |
| daily | `mark_abandoned` sweep (optional) | only for challenges nobody revisits; the sector's own `record_activity` also flips stale ones |

## 7. Monitoring: what to alert on

Alert immediately (page):
- Any **product paused** (`active == false`), especially `pause_reason == 2` (reconciliation deficit). Read the `reconcile_product` logs: they contain the tally and vault numbers.
- Any **`admin_withdraw_marketing_funds`** call (program log line `admin_withdraw_marketing_funds: pool=..`), and any change in `marketing_withdrawn_*`.
- A **pool balance** that fell without a matching `settle_claims` or admin withdrawal.
- **Upgrade-authority or program-data changes** (poll `solana program show`; any change in last-deploy slot or authority).
- A **pool or SL8 token account becoming frozen** (state field), or the mint's freeze authority acting.
- A cycle **open longer than 24 h** (`cycle_active` with no `cycle_processed_count` progress), or **no cycle begun for > 6 days** while `open_claims_count > 0`.

Alert (ticket):
- Coverage ratio `(usdc_pool + usdt_pool) / open_claims_total` below 1 (claims are being paid pro rata) and its trend; `open_claims_count` growth; claims with `last_settled_cycle` far behind `cycle_id`.
- `bond_principal_open_total` over 80% of the $600K cap, any wallet near $50K, and the size of bond withdrawals queued versus the pools.
- `bond_withdrawal_fees_retained`, `floor_updated_at` stale.
- Failed `settle_claims` rate and compute used (ground wallets).
- SL8 token-account inflows versus expected fee splits.
- Registry `total_requests_emitted` / `total_requested_amount` versus each sector's tally (before the keeper's reconcile does it).

## 8. Rollback and incident steps

There is **no rollback**: state is on chain and claims/bonds are irrevocable. The levers are 2-of-2 pauses, the keepers, and (while it exists) the upgrade authority.

**A product is paused by reconciliation.**
1. Do not reactivate yet. Read the log of the reconcile transaction: `tally_count/tally_total` versus `vault_count/vault_total`.
2. If the **tally is lower** than the vault: the sector missed an update. Fix the sector, bring its tally up to the vault's numbers, then **FOUNDER ONLY - Claude Code must never touch:** `reactivate_product` (both admins).
3. If the **tally is higher** (the sector counted a request the vault never accepted): the books can never match again (counters only grow). Retire the product: leave it paused and register a new product id (SR-05).
4. Queued claims of a paused product still settle; a pause does not stop the heartbeat.

**A cycle is stuck** (open, not progressing).
1. Check the keepers are running; run a keeper manually.
2. Simulate `settle_claims` for one eligible claim alone. A **wrong ATA** means your keeper derived it wrongly. A **token error on the pool** means the issuer froze a pool account (SR-03): there is no on-chain remedy except a program upgrade, which needs the upgrade authority (step 3); until then no claim can be paid. Contact the issuer.
3. Claims with unusable destinations are skipped, never block; if `processed < eligible` something else is wrong, so inspect the unprocessed claims one by one.

**Suspected exploit or compromised sector.**
1. **FOUNDER ONLY - Claude Code must never touch:** `pause_product` for the affected product (both admins) stops new purchases, resets and payout requests. It does **not** stop queued claims, bond deposits/withdrawals, the heartbeat or the admin withdrawal.
2. You cannot stop `begin_heartbeat` (permissionless). If the pool must be protected from queued claims, the only levers are an upgrade (authority holder) or, as a last resort, the documented admin withdrawal to SL8's account (**FOUNDER ONLY - Claude Code must never touch**, both admins; this is the documented exception and it moves money out of the pool).
3. Preserve evidence: transaction signatures, the registry and tally accounts, vault state.

**An admin key is lost or compromised.** There is no rotation instruction (SR-14). A lost key permanently disables all admin actions; a compromised single key can do nothing admin-side but can spend the SL8 token accounts if it is the SL8 key (and see SR-01). Move SL8 revenue out of the SL8 key's accounts promptly and prepare a migration (new program + new vault) using the upgrade authority.

**After any incident:** update this checklist and the review's findings list before resuming.
