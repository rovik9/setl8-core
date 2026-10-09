# Deploy checklist: core-vault (devnet, then mainnet)

Companion to [SECURITY-REVIEW.md](SECURITY-REVIEW.md), [THREAT-MODEL.md](THREAT-MODEL.md) and [ADMIN-TOOL.md](ADMIN-TOOL.md) (the signing ceremony for every admin action below). Do the devnet column completely and watch it for a full heartbeat cycle before starting the mainnet column.

> **Every step marked `FOUNDER ONLY - Claude Code must never touch` needs a real key (an admin key, the program keypair, or the upgrade authority). An assistant must never read, print, copy, generate or use those keys, and must not run those commands.** Prepare the command, hand it over, and let a founder sign on their own device.

Conventions: `$SO` = `target/deploy/core_vault.so`. All hashes are SHA-256. Record every value you are told to record in a deploy log kept outside the repo.

## 0. Preconditions

- [ ] Clean `main` at the commit you intend to ship; `git status` is empty; `git log -1` recorded.
- [ ] `scripts/test-all.sh` is green (localnet build, `cargo test` x2, LiteSVM suite including `invariants_fuzz`, TypeScript suite).
- [ ] You have read SECURITY-REVIEW.md, section 5. **Open before mainnet:** SR-15 (decide who holds the upgrade authority; plan: a multisig, then revoke) and SR-02 (a registered sector's payout amounts are trusted; revisit when each product's payout rule exists). **Decided by the founder and accepted, with consequences you must know:** SR-01 (the holder of the SL8 key can take roughly half of every bond they open; bond depositors must trust that key holder), SR-18 (the revenue address is the SL8 admin key), SR-14 (no admin-key rotation), SR-04, SR-05. **Fixed:** SR-21 (the $2.5M claims ceiling) and SR-03 (a frozen pool counts as empty).
- [ ] You understand the **$2.5M claims ceiling** (`OPEN_CLAIMS_CEILING`): bonds alone can owe up to $780,000, leaving about $1,720,000 for trader claims. If trader claims fill that headroom, `request_bond_payout` and `request_payout` fail with `ClaimsCeilingExceeded` until a heartbeat pays the total down. Raising it needs a program upgrade.
- [ ] **The admin signing tool is built and verified on every signer's machine:** `scripts/verify-admin-tool-build.sh` ends with `OK: contains both real admin keys and no test keys.`; the printed `sha256` is in the deploy log (build it on a second machine and compare). See [ADMIN-TOOL.md](ADMIN-TOOL.md).
- [ ] **Key custody is decided and rehearsed.** Every admin instruction needs both admin signatures from the exact keys compiled into the program (they cannot be rotated: SR-14). The SL8 key currently lives in a phone wallet, **which cannot sign these transactions**: before mainnet the founder must be able to sign with those exact keys (a key file on an offline machine, or another way that the tool can use), or the program must be redeployed with admin keys that can sign. Hardware wallets are an open decision (ADMIN-TOOL.md, section 5). Rehearse the whole ceremony on devnet with the real key arrangement.
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
5. **Mainnet, phase 3 (when the design is final):** **FOUNDER ONLY - Claude Code must never touch:** make it immutable with `solana program set-upgrade-authority <PROGRAM_ID> --final`. This is irreversible, and it also removes the only way to fix any future defect, including a change to the claims ceiling; decide with that in mind (SR-15).
6. Write down who holds each key, where it is backed up, and the recovery plan. Losing an admin key has no on-chain recovery (SR-14).

## 4. Deploy

1. **FOUNDER ONLY - Claude Code must never touch:** `solana program deploy $SO --program-id <PROGRAM_KEYPAIR> --upgrade-authority <AUTHORITY> --url <devnet|mainnet-beta>` (pay with the deployer wallet; set a priority fee on mainnet).
2. `solana program show <PROGRAM_ID> --url <cluster>`: authority, data length, last deploy slot are as intended.
3. **Compare the deployed bytes with the recorded hash:** `solana program dump <PROGRAM_ID> dump.so --url <cluster>`, then `shasum -a 256 dump.so`. The on-chain program is zero-padded to its allocated length; compare the first `$SO`-length bytes (`head -c $(wc -c < $SO) dump.so | shasum -a 256`). It must equal the hash recorded in 2.4.
4. Smoke test with a throwaway client against the cluster: an unsigned `init_vault` must fail with a missing-signature error, never succeed.

## 5. One-time initialisation, in this order

1. **Mints.** Confirm the USDC and USDT mint addresses for the target cluster from the issuers' official documentation (on devnet use mints you control or the devnet faucet mints). Both must be classic SPL Token mints with **6 decimals**; `init_vault` rejects anything else.
2. **FOUNDER ONLY - Claude Code must never touch:** `init_vault(usdc_mint, usdt_mint)` signed by **both** admin keys (SL8 pays the rent), through the ceremony in [ADMIN-TOOL.md](ADMIN-TOOL.md): `setl8-admin status --cluster <c>` first (it must say the vault does not exist yet), `setl8-admin nonce-create` once, then `plan init-vault --usdc-mint <USDC> --usdt-mint <USDT> --nonce-account <NONCE> --out tx.json`, `inspect` and `sign` on each signer's own machine (compare the mint addresses with the issuers' documentation and the message hash over a second channel), `send`, and `status` again. It creates `VaultState` and the two pool token accounts. Record the vault PDA and the two pool addresses.
3. **SL8 token accounts.** `deposit_fee`, `deposit_reset` and `deposit_bond` pay SL8 into *any existing token account owned by `vault_state.sl8_wallet`* (= the SL8 admin key, by founder decision: SR-18). **The SL8 admin key's USDC and USDT token accounts must exist BEFORE any fee arrives**; without them every `deposit_fee`, `deposit_reset` and `deposit_bond` fails. Create both associated token accounts right after `init_vault` and before the first purchase or bond. **FOUNDER ONLY - Claude Code must never touch** (the owner is a real admin key). Remember the consequence: whoever holds that one key holds SL8's revenue and, through SR-01, roughly half of every bond, so keep it on a hardware wallet with a tested backup and sweep the accounts to cold storage on a routine.
4. **FOUNDER ONLY - Claude Code must never touch:** `register_product(product_program_id, fee_split_bps, challenge_sizes, max_payout_count, reset_price_bps)` per sector, signed by both admins through the ceremony (`plan register-product --config product.json ...`, `inspect`, `sign` x2, `send`). Double-check the sector id: a registry cannot be corrected or closed. Check `fee_split_bps <= 10_000`, no zero-cost tier unless intended, reset prices sensible.
5. **The sector creates its payout tally** (`derive_payout_tally(product_program_id)`, owned by the sector, initialised `0 / 0` with `PayoutTally::write_into`) **before its first `request_payout`**. An uninitialised or missing tally with a non-zero request count pauses the product at the next `reconcile_product`. The sector must update the tally **in the same transaction** as every accepted `request_payout` (SR-17).
6. **Smoke purchase**, a payout request and a full heartbeat with a tiny amount on the target cluster before announcing anything: `deposit_fee` -> `request_payout` -> `reconcile_product` -> `begin_heartbeat` -> `settle_claims` -> `finalize_heartbeat`.
7. **First `begin_heartbeat`:** the first cycle may start immediately; every later cycle needs 432,000 s (5 days) from the previous *start*.

### 5a. Every admin action: check the state before and after

For `init_vault`, `register_product`, `update_product_config`, `pause_product`, `reactivate_product` and `admin_withdraw_marketing_funds`, run the ceremony of [ADMIN-TOOL.md](ADMIN-TOOL.md) and bracket it with **`setl8-admin status --cluster <c>`**. Keep both outputs in the deploy log.

| action | what must differ between the two `status` outputs, and nothing else |
|---|---|
| `init_vault` | the vault exists; the two mints are the documented ones; both pools exist with balance 0; no claims; the SL8 admin key's USDC and USDT token accounts exist (create them first: `status` shows `MISSING` otherwise and every fee, reset or bond deposit would fail) |
| `register_product` | one new product, `ACTIVE`, with the configured fee split, tier count and payout cap |
| `update_product_config` | that product's fee split / tier count / payout cap; its pause state and request counters unchanged |
| `pause_product` / `reactivate_product` | that product shows `PAUSED (paused: planned upgrade ...)` / `ACTIVE` |
| `admin_withdraw_marketing_funds` | the pool balance is **down by exactly the amount**, the SL8 token account is **up by exactly the amount**, `withdrawn so far` is up by the amount, and the pool still holds at least its reserve. Read `admin_withdraw could take NOW` and the claims figures before you decide the amount (the withdrawal has no deduction for open claims or bonds: SR-07, section 7 of the review) |

Also read, on every `status`: a **frozen** pool or SL8 account (issuer action), the **claims headroom** against the $2.5M ceiling, a **PAUSED** product with a reconciliation reason, and an **ACTIVE** cycle that is not progressing.

### Pre-signed transactions

A transaction signed against a durable nonce stays valid until the nonce is advanced, and anyone holding the fully signed file can send it. Send promptly, keep partially signed files private, and **advance the nonce (`setl8-admin nonce-advance`) whenever a ceremony is abandoned**. Executing any nonce transaction advances the nonce and so kills every other file signed against the same value.

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
- **Issuer freeze monitoring:** a **pool or SL8 token account becoming frozen** (poll the `state` field of both pools and both SL8 accounts, and watch the mint's freeze authority). A frozen pool no longer wedges the heartbeat (it counts as empty and the other pool pays), but payouts shrink or stop until it thaws, deposits into it fail, and `admin_withdraw_marketing_funds` from it fails. Contact the issuer, and tell traders and bond depositors to use the other mint meanwhile.
- A cycle **open longer than 24 h** (`cycle_active` with no `cycle_processed_count` progress), or **no cycle begun for > 6 days** while `open_claims_count > 0`.

Alert (ticket):
- **`open_claims_total` against the $2.5M ceiling** (alert at 80%, i.e. $2.0M): near the ceiling, new `request_payout` and `request_bond_payout` start failing with `ClaimsCeilingExceeded`; bond holders then cannot exit until the total is paid down.
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
3. If the **tally is higher** (the sector counted a request the vault never accepted): the vault's counters only grow, so repair it on the sector side: upgrade the sector program to write the vault's counters (`total_requests_emitted`, `total_requested_amount` from the registry) into its tally account, then `reactivate_product` (SR-05, accepted; no vault change needed). If the sector cannot be repaired, leave the product paused and register a new product id.
4. Queued claims of a paused product still settle; a pause does not stop the heartbeat.

**A cycle is stuck** (open, not progressing).
1. Check the keepers are running; run a keeper manually.
2. Simulate `settle_claims` for one eligible claim alone. A **wrong ATA** means your keeper derived it wrongly. A frozen **pool** no longer causes a token error (SR-03 is fixed: it counts as empty), so a token error from a pool transfer now means something else; investigate the transaction logs. If a pool is frozen, claims are paid from the other pool only (or not at all if both are frozen) and carry over; the cycle still finishes. Contact the issuer.
3. Claims with unusable destinations are skipped, never block; if `processed < eligible` something else is wrong, so inspect the unprocessed claims one by one.

**Suspected exploit or compromised sector.**
1. **FOUNDER ONLY - Claude Code must never touch:** `pause_product` for the affected product (both admins, through the ceremony: `plan pause-product --product <id> ...`; use `--recent-blockhash` if both signers are together and time matters) stops new purchases, resets and payout requests. It does **not** stop queued claims, bond deposits/withdrawals, the heartbeat or the admin withdrawal.
2. You cannot stop `begin_heartbeat` (permissionless). If the pool must be protected from queued claims, the only levers are an upgrade (authority holder) or, as a last resort, the documented admin withdrawal to SL8's account (**FOUNDER ONLY - Claude Code must never touch**, both admins; this is the documented exception and it moves money out of the pool).
3. Preserve evidence: transaction signatures, the registry and tally accounts, vault state.

**An admin key is lost or compromised.** There is no rotation instruction (SR-14, accepted by the founder). A lost key permanently disables all admin actions; a compromised single key can do nothing admin-side but can spend the SL8 token accounts if it is the SL8 key, and (SR-01, SR-18) whoever holds the SL8 key also holds the power to take roughly half of every bond they open. Move SL8 revenue out of the SL8 key's accounts promptly and prepare a migration (new program + new vault) using the upgrade authority.

**After any incident:** update this checklist and the review's findings list before resuming.
