# Setl8 — Architecture Update (Sep 11, 2026)

**Supersedes:** the "single Anchor workspace" repo-structure decision and the "25% locked/unavailable" framing in the existing Architecture Decisions doc. Everything else in that doc still stands unless noted below.

This doc is the source of truth for what got locked in today's session. Feed it to Claude Code as context before starting the shared-interfaces repo (build that first — vault and lev-trading both depend on it).

---

## 1. Repo structure — separate repos, not single workspace

Decision reversed from single Anchor workspace to independent repos, one per program:

- `setl8-shared-interfaces` — the CPI interface definitions only. Build this first.
- `setl8-vault` — core vault program (payout + bond).
- `setl8-lev-trading` — leveraged trading sector program.
- Future: `setl8-options`, `setl8-predictions`, etc. — added as their own repos later, possibly by other developers, without touching anything already deployed.

**Why:** phased build (vault first, products bolted on one at a time), independent audit/deploy cycles per product, and room to hand a single product repo to a hired dev later without exposing the rest of the codebase.

**What replaces the compile-time safety net:** `setl8-shared-interfaces` is published as a versioned package (private registry or pinned git tag — pick one before the first consumer repo is created). Every other repo pulls it as a dependency rather than copying code. Bump the version and note the change whenever an instruction signature changes; consumers pin a specific version, so a mismatch fails to compile in the consuming repo at build time, not at runtime on-chain. This repo holds **interface shapes only** — instruction signatures, account struct shapes for cross-program calls. No business logic, no state, no numbers.

---

## 2. Wallet & authorization model

Unchanged from before: SL8 protocol wallet (admin, public-facing) + Rov personal wallet (your second key, never receives funds, never otherwise touches on-chain flows).

**New — 2-of-2 multisig (SL8 + Rov) now applies to every privileged vault instruction, not just `register_product`:**

| Instruction | Signer requirement | Destination of funds/effect |
|---|---|---|
| `register_product` | 2-of-2 | n/a (registers a program) |
| `reactivate_product` | 2-of-2 | n/a (flips `active` flag) |
| `update_product_config` | 2-of-2 | n/a (updates on-chain product data) |
| `admin_withdraw_marketing_funds` | 2-of-2 | SL8 wallet only |

`admin_withdraw_marketing_funds` was initially drafted single-signer, then explicitly bumped to 2-of-2 for consistency with everything else that touches privileged state. Funds always land in the SL8 wallet — the Rov wallet's role stays strictly "second signature," never a fund destination, on any instruction.

---

## 3. Vault fund flow & the 25% floor

Corrected model (not two separate buckets):

- Challenge purchase: 35% → SL8 protocol wallet, 65% → payout vault pool.
- The **entire 65%** is payout-eligible — heartbeat can dispatch against all of it, down to zero, if genuinely needed.
- **75%** of that pool is also admin-withdrawable (via `admin_withdraw_marketing_funds`) for marketing/growth spend.
- **25%** is a hard floor. `admin_withdraw_marketing_funds` can never pull the pool balance below this floor, regardless of marketing need. It exists so queued payouts always have a guaranteed reserve even if admin has drawn the withdrawable portion down aggressively.

**Floor recalculation — confirmed as a watermark, not live-per-withdrawal:** the floor value is recalculated once per heartbeat cycle (~every 5 days) and stored. Withdrawal checks against that stored watermark, not against a freshly-derived live balance on every call. Flag this precisely when building — it's a meaningfully different (and cheaper) check than live recalculation.

---

## 4. Bond vault (unchanged)

No changes from the locked design: $50 min / $50K max per wallet, $600K global cap, 6mo@20% or 9mo@30% (~3.33%/mo accrual), 50/50 capital split on deposit (SL8 wallet / shared pool, commingled), accrual every 6th heartbeat, hard lock floor at half-term, principal-only between floor and maturity, principal + full accrued interest at full maturity.

**New — per-wallet bond cap tracking mechanism (previously open, now resolved):** a dedicated `BondCapTracker` PDA, seeded by depositor pubkey, storing a single running total (`total_bonded: u64`). Every deposit reads it, checks proposed total against the $50K cap, updates it atomically in the same instruction. Withdrawals decrement it back down.

Rejected alternative: summing all of a wallet's `BondPosition` accounts on the spot via remaining-accounts on every deposit. Costs nothing in rent but eats transaction-size budget — roughly 320 bytes for just 10 position references against Solana's ~1,232-byte tx cap, meaning a wallet with a dozen-plus positions could eventually be unable to deposit at all. Tracker account costs a one-time rent of roughly $0.001-equivalent and avoids that ceiling entirely.

---

## 5. Vault's expanded scope — product config + trader state

This is the biggest structural change from today. The vault is no longer just a dumb capital mover — it becomes the source of truth for per-product payout caps and per-trader payout progress, specifically so a bug or state desync in a sector program can never push a payout past the real cap. This extends (not replaces) the original `ProductRegistry` from the Module 1 design.

### 5a. Product config (extends `ProductRegistry`)

New fields, updated via `update_product_config` (2-of-2):

- `product_id`
- `challenge_sizes: Vec<ChallengeSize>` — each with its cost
- `fee_split_bps` — vault % vs. admin % (this is the existing field from Module 1, e.g. 6500 for 65%)
- `max_payout_count` — the payout cap for this product (e.g. hypothetically 5 for lev-trading, 6 for options — **actual numbers not yet set for any real product**, see open items)
- `active` / `paused` status (existing field)

### 5b. Trader state (new account)

One record per **wallet + product_id + challenge_id** — not just per wallet+product. A trader buying a new challenge after a failed one gets a brand-new record starting fresh; the old failed record persists permanently as history, never reused or reset.

Fields:
- `trader_wallet`, `product_id`, `challenge_id`
- `payout_count: u64`
- `status`: `Active` / `Graduated` / `Failed` / `Abandoned`
- `last_activity_timestamp: i64`

**Creation:** `deposit_fee` arriving with a new `challenge_id` creates the record at `payout_count = 0`, `status = Active`. The fee itself is the proof of purchase — no fee, no record, nothing to game.

**Payout check:** `request_payout` reads this record before moving any funds — checks `payout_count` against the product's `max_payout_count`, checks status isn't `Failed`/`Graduated`/`Abandoned`. Rejects (transaction reverts, no global pause) if any check fails.

**Auto-graduation:** when `payout_count` hits `max_payout_count`, the vault flips status to `Graduated` automatically inside the same `request_payout` instruction — no separate call needed. No live-side hook yet (live side isn't built); this is a future integration point for whoever builds the graduation bridge.

**Failure:** sector program calls a dedicated instruction (e.g. `flag_trader_failed`) to mark a specific challenge's record `Failed`. Terminal for that `challenge_id` — never reused. A retry is always a new challenge purchase with its own fresh record.

**Abandonment (new rule):** no deadline on payout requests themselves — that stays infinite. But a challenge with **60 days of zero activity** (no trade/interaction updating `last_activity_timestamp`) auto-fails as `Abandoned`. Checked by the heartbeat sweep, since heartbeat is the only recurring on-chain trigger available (programs don't run on their own). **Open engineering question, not decided today:** how the heartbeat instruction efficiently iterates trader-state accounts to check this at scale — flag for the build, don't assume a naive full-scan approach without checking cost at realistic trader counts.

---

## 6. Request ID generation (previously open, now resolved)

Mutual-agreement model: the sector program proposes a request ID when calling `request_payout` (derived from its own per-trader nonce/counter). The vault independently checks that proposed ID against what it expects as the next valid ID for that trader-state record (effectively `payout_count + 1`). Mismatch → reject. Neither side unilaterally decides; the vault holds final authority since it won't accept an ID it didn't independently expect.

---

## 7. Known PDAs — stale, will regenerate

`VaultState` and `Payout Vault` PDAs listed in the original Architecture Decisions doc are **stale**: the SL8 admin wallet is being changed, and this is now a separate deployment under the new repo structure. Do not carry those addresses forward — regenerate and verify at actual deploy time.

---

## 8. Open items — still genuinely unresolved

Carried forward, not addressed today:
- Whether the vault PDA seed includes the admin pubkey (determines whether re-init is required after a wallet swap)
- Whether a public reconciliation-status display surface exists
- Whether a global failure counter is needed

New from today:
- Heartbeat sweep mechanics for the 60-day abandonment check at realistic scale (engineering decision, not a design decision — resolve during build)
- Actual challenge sizes, costs, and `max_payout_count` for lev-trading itself — not yet defined (today's "five" and "six" payout numbers were illustrative examples, not locked specs for any real product)
- Exact versioning mechanism for `setl8-shared-interfaces` (private registry vs. pinned git tags) — directionally agreed to version it, mechanism not chosen

Do not silently default any of these when building — flag and confirm first.
