# setl8-vault — Repo Spec

Depends on `setl8-shared-interfaces` (pinned version) for cross-program call shapes. Build after the shared-interfaces repo exists.

## Core principles (unchanged, carried from locked architecture)

- Vault is deliberately minimal on the *money-movement* surface — `deposit_fee` and `request_payout` remain the only capital-movement entry points from sector programs.
- CPI calls authenticated against the actual invoking program's on-chain identity, checked against `ProductRegistry`, never self-reported. This is the single most important check in the contract — write a comment block above it explaining the impersonation risk.
- Dual bookkeeping: sector programs hold the full "why" trail, vault holds "what and when," reconciled independently.
- Active reconciliation every heartbeat; count mismatch → auto-pause that specific product only. This is a sanctioned exception to "no manual overrides," scoped narrowly to whether a product can transact — never to trade outcomes or payout amounts.
- Heartbeat dispatch: bond maturities and trader payouts have equal priority; pro-rata haircut applied equally across all claims if the pool is short.
- "No admin key on money" as a principle is now qualified, not absolute: `admin_withdraw_marketing_funds` is a deliberate, explicit exception, gated by the 25% floor and 2-of-2 signing — document this exception clearly rather than treating it as contradicting the principle silently.

## Accounts

### `ProductRegistry` (PDA, one per sector program) — extends original Module 1 design

- `product_program_id: Pubkey`
- `challenge_sizes: Vec<ChallengeSize>` (each with a cost) — **new**
- `fee_split_bps: u16` (vault % vs admin %, e.g. 6500)
- `max_payout_count: u64` — **new**, the payout cap for this product
- `active: bool`
- `total_requests_emitted: u64` (for heartbeat reconciliation)

PDA seed scheme: confirm/propose, same approach as original Module 1 (`[b"product_registry", product_program_id.as_ref()]` or similar).

### `TraderState` (PDA, one per wallet + product + challenge) — new

- `trader_wallet: Pubkey`
- `product_id: Pubkey`
- `challenge_id: u64`
- `payout_count: u64`
- `status: TraderStatus` (`Active | Graduated | Failed | Abandoned`)
- `last_activity_timestamp: i64`

PDA seed: `[b"trader_state", trader_wallet.as_ref(), product_id.as_ref(), challenge_id.to_le_bytes().as_ref()]` or similar — confirm before implementing.

A failed or abandoned record is never reused or reset. A new challenge purchase (new `challenge_id`) always creates a fresh record.

### `BondPosition` (PDA per bond, existing design, unchanged)

Seeded by depositor pubkey + deposit_index, independent timer per position.

### `BondCapTracker` (PDA, one per wallet) — new

- `total_bonded: u64`

Seed: `[b"bond_cap_tracker", depositor.as_ref()]`. Read and updated atomically in the same instruction as every bond deposit/withdrawal — this is the $50K per-wallet cap check.

### Marketing withdrawal floor state

Store the 25%-floor watermark value, recalculated and stored once per heartbeat cycle (not derived live on every withdrawal call). Confirm storage location — likely a field on the core vault/payout-pool state account.

## Instructions

| Instruction | Signer | Notes |
|---|---|---|
| `register_product(product_program_id, fee_split_bps, challenge_sizes, max_payout_count)` | 2-of-2 (SL8 + Rov) | Initializes `ProductRegistry`, `active = true` |
| `reactivate_product(product_program_id)` | 2-of-2 | Flips `active = true`, no reconciliation reset |
| `update_product_config(product_id, ...)` | 2-of-2 | Updates challenge sizes, fee split, max payout count on existing `ProductRegistry` |
| `admin_withdraw_marketing_funds(amount)` | 2-of-2 | Destination fixed to SL8 wallet. Rejects if resulting balance < stored 25% floor watermark |
| `deposit_fee(amount, product_id, challenge_id)` | CPI-auth (registry identity check) | Creates `TraderState` if `challenge_id` is new; sets `payout_count = 0`, `status = Active` |
| `request_payout(trader_wallet, amount, product_id, challenge_id, proposed_request_id)` | CPI-auth | Checks `TraderState`: rejects if `status != Active` or `payout_count >= max_payout_count`. Checks `proposed_request_id` against vault's own expected next ID — reject on mismatch. On success: moves funds, increments `payout_count`, updates `last_activity_timestamp`. Auto-flips `status = Graduated` in the same instruction if `payout_count` now equals `max_payout_count` |
| `flag_trader_failed(trader_wallet, product_id, challenge_id)` | CPI-auth | Sets `status = Failed`, terminal for this record |
| Heartbeat instruction(s) | Permissionless (heartbeat bot wallet, holds no funds) | Reconciliation + auto-pause on mismatch; recalculates and stores the 25%-floor watermark; sweeps `TraderState` records for 60-day inactivity → `status = Abandoned`. **Sweep mechanism not yet decided — flag before implementing, don't assume a naive full-scan** |

## Open items — do not silently default

- Vault PDA seed: does it include the admin pubkey? Determines re-init requirement after wallet swap.
- Public reconciliation-status display: build or not?
- Global failure counter: needed or not?
- Heartbeat sweep mechanism for abandonment checks at scale.
- Actual `challenge_sizes` / `max_payout_count` values for any real product — not yet set for lev-trading or any other sector.

Ask before assuming any of the above.
