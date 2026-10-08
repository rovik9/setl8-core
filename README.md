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
5. **Money out is queued.** The sector CPIs `request_payout`. It validates the request exactly as before (active product, status, inactivity, payout cap, request id) but moves **no tokens**: it records a `PayoutClaim` (the amount owed to the trader, in 6-decimal dollar units, USDC = USDT = $1) and bumps `open_claims_count` / `open_claims_total` on `VaultState`. A stale challenge still flips to `Abandoned` and creates no claim.

   Claims are paid by a permissionless **heartbeat**, at most once every 5 days (`HEARTBEAT_MIN_GAP_SECS = 432_000`, measured between cycle starts):

   1. `begin_heartbeat` opens a cycle and snapshots the total owed and the total available in the two pools.
   2. `settle_claims` is called in batches. Every eligible claim is paid the same pro-rata share, `min(available, owed) / owed`, of what it is owed, from the larger pool first and topped up from the other. Destinations are the trader's associated token accounts; a claim whose accounts are unusable is skipped and stays owed. Whatever is unpaid stays owed and carries over, with no priority by age and no expiry. A claim paid in full is closed and its rent goes to the caller.
   3. `finalize_heartbeat` ends the cycle once every eligible claim has been processed, and sets each pool's reserve floor to 25% of its balance. Floors only constrain a future admin withdrawal; they never limit claim settlement.

### Who may call what

| Group | Instructions | Authority |
|---|---|---|
| `admin/` | `init_vault`, `register_product`, `update_product_config`, `pause_product`, `reactivate_product` | Both admin signatures (SL8 + Rov) |
| `sector/` | `deposit_fee`, `deposit_reset`, `record_activity`, `request_payout`, `flag_trader_failed` | A registered sector program via CPI, authenticated by its `sector_authority` PDA |
| `permissionless/` | `mark_abandoned`, `reconcile_product`, `begin_heartbeat`, `settle_claims`, `finalize_heartbeat` | Anyone |

The CPI-auth check is `utils::assert_sector_authority`. It verifies the caller's on-chain identity against the `ProductRegistry` and never trusts a self-reported program ID.

### Accounts (PDAs)

| Account | Seeds | Holds |
|---|---|---|
| `VaultState` | `["vault_state", SL8_ADMIN, ROV_ADMIN]` | USDC/USDT mints, pool addresses |
| Pool token account | `["pool", vault_state, mint]` | The USDC / USDT funds |
| `ProductRegistry` | `["product_registry", product_program_id]` | Per-sector config and `active` flag |
| `TraderState` | per wallet + product + challenge | Trader status, activity clock, reset history |
| `PayoutClaim` | `["payout_claim", trader_state, request_id]` | An amount owed to a trader, until a heartbeat cycle pays it |

## Repository layout

```
programs/core-vault/src/
  lib.rs              the program: one thin wrapper per instruction, grouped by who calls it
  constants/          seeds.rs  admin.rs  limits.rs  tokens.rs
  errors.rs           VaultError
  state/              on-chain accounts: vault_state, product_registry, trader_state, payout_claim
  instructions/       one file per instruction (Accounts struct + handler)
    admin/              init_vault, register_product, update_product_config, pause_product, reactivate_product
    sector/             deposit_fee, deposit_reset, record_activity, request_payout, flag_trader_failed
    permissionless/     mark_abandoned, reconcile_product, begin_heartbeat, settle_claims, finalize_heartbeat
  utils/              auth.rs (sector CPI-auth check), token_payment.rs (fee split + transfers),
                      settlement.rs (pro-rata arithmetic), destination.rs (ATA checks), reconciliation.rs (tally check),
                      pda_account.rs
tests-rs/             LiteSVM integration tests (own Cargo workspace)
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

> **Open pre-deploy decision: SL8 revenue currently lands in token accounts owned by the SL8 admin key.** `init_vault` sets `sl8_wallet = SL8_ADMIN_PUBKEY`. Consider a separate treasury set via `init_vault` instead. This is deliberately unchanged so far.

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

`tests-rs` is a separate Cargo workspace with its own `Cargo.lock`, so LiteSVM's dependency tree never touches the program's lockfile. It exercises every instruction with exact-error assertions: admin signatures, CPI authentication, fee splits, the pause-adjusted inactivity clock, and the payout queue with its pro-rata heartbeat settlement. It enables the `localnet` feature and refuses to run against a `.so` that does not embed the same admin keys. Set `CORE_VAULT_SO=/path/to/other.so` to run it against a different build (used for mutation testing).

Don't use plain `anchor test`: it would load the real-key build, which the suite cannot sign for.

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
- **Known limit.** Both sides' counts only ever go up. A sector that *under*-reports can catch up and then match again after an admin reactivates it. A sector whose tally *over*-reports (it counted a request the vault never accepted) can never match again: the vault's books cannot be lowered to meet it, so every reconcile re-pauses it. Such a product is permanently unusable and must be replaced by registering a new product.

## Design notes

- **Minimal money surface.** Only `deposit_fee`, `deposit_reset` (tokens in) and `settle_claims` (tokens out) move tokens. `request_payout` only records a claim.
- **Settlement batches.** `settle_claims` takes at most `MAX_SETTLE_BATCH = 6` claims per call: a full batch is 1,106 bytes (limit 1,232) and about 130,000 compute units (about 235,000 for wallets ground to make the token-account derivation expensive), so add a `SetComputeUnitLimit` of 400,000. Any single claim can always be settled alone. `tests-rs/tests/settle_batch.rs` measures all of this.
- **Unusable destinations are skipped, wrong ones are errors.** A claim whose associated token accounts are missing, frozen, re-owned, or of the wrong mint is skipped and stays owed forever. Passing an address that is not the trader's associated token account reverts the whole transaction.
- **Stale paths return `Ok`.** An inactivity timeout has to persist the `Abandoned` state, so the stale path of `record_activity` and `request_payout` returns success plus return data (`ActivityOutcome` / `PayoutOutcome`) instead of an error, which would roll the state change back.
- **Fee split is validated.** `fee_split_bps` above 10,000 is rejected in `register_product` and `update_product_config`.
- **Large accounts are boxed.** Wide `Accounts` structs use `Box<Account<…>>` to stay under the SBF stack limit.
