# setl8-core

The Setl8 **core vault** — an [Anchor](https://www.anchor-lang.com/) (Solana) program that holds the USDC/USDT payout pools and the per-trader bookkeeping for Setl8 sector programs (leveraged trading, options, …).

Sector programs never touch the pools directly. They call the vault over CPI, and the vault checks, against an on-chain registry, that the caller really is a registered sector program before it moves any money.

| | |
|---|---|
| Program name | `core_vault` (`programs/core-vault`) |
| Program ID (localnet) | `2Z6WNsj4hNhKhmK9Cj3sXV5San9VYhh8gwtyvBfpP6ft` |
| Anchor | 0.32.1 |
| Tokens | classic SPL Token only, 6-decimal USDC and USDT (Token-2022 is rejected) |
| Shared types | [`setl8-shared-interfaces`](https://github.com/rovik9/setl8-turbo) pinned at tag `v0.3.1` |

> **Status: pre-audit, not deployed.** See [Keys and builds](#keys-and-builds) before building anything you intend to deploy.

## How it works

1. **Setup.** Both admins (SL8 and Rov, a 2-of-2 multisig) call `init_vault` once. It records the USDC and USDT mints and creates one pool token account per mint, as PDAs.
2. **Register a sector.** The admins call `register_product` for a sector program. This stores a `ProductRegistry` PDA with its challenge sizes, its fee split (`fee_split_bps`), its reset price and its payout cap.
3. **Money in.** When a trader pays, the sector program CPIs `deposit_fee` (new challenge) or `deposit_reset` (reset). The vault pulls the payment from the trader and splits it: `pool = floor(amount × fee_split_bps / 10_000)` goes to the matching pool and the remainder goes to the SL8 token account.
4. **Activity tracking.** The sector calls `record_activity` to prove a trader is still active. A trader who is inactive for **more than 7 days** is stale, and `mark_abandoned` (callable by anyone) can mark them `Abandoned`. Pausing a product freezes the inactivity clock.
5. **Money out.** The sector CPIs `request_payout`. The vault pays out of whichever pool holds more (a tie goes to USDC). It never mixes the two pools, and fails with `InsufficientPoolBalance` if that pool is too small.

### Who may call what

| Group | Instructions | Authority |
|---|---|---|
| `admin/` | `init_vault`, `register_product`, `update_product_config`, `pause_product`, `reactivate_product` | Both admin signatures (SL8 + Rov) |
| `sector/` | `deposit_fee`, `deposit_reset`, `record_activity`, `request_payout`, `flag_trader_failed` | A registered sector program via CPI, authenticated by its `sector_authority` PDA |
| `permissionless/` | `mark_abandoned` | Anyone |

The CPI-auth check is `utils::assert_sector_authority`. It verifies the caller's on-chain identity against the `ProductRegistry` and never trusts a self-reported program ID.

### Accounts (PDAs)

| Account | Seeds | Holds |
|---|---|---|
| `VaultState` | `["vault_state", SL8_ADMIN, ROV_ADMIN]` | USDC/USDT mints, pool addresses |
| Pool token account | `["pool", vault_state, mint]` | The USDC / USDT funds |
| `ProductRegistry` | `["product_registry", product_program_id]` | Per-sector config and `active` flag |
| `TraderState` | per wallet + product + challenge | Trader status, activity clock, reset history |

## Repository layout

```
programs/core-vault/src/
  lib.rs              the program: one thin wrapper per instruction, grouped by who calls it
  constants/          seeds.rs  admin.rs  limits.rs  tokens.rs
  errors.rs           VaultError
  state/              on-chain accounts: vault_state, product_registry, trader_state
  instructions/       one file per instruction (Accounts struct + handler)
    admin/              init_vault, register_product, update_product_config, pause_product, reactivate_product
    sector/             deposit_fee, deposit_reset, record_activity, request_payout, flag_trader_failed
    permissionless/     mark_abandoned
  utils/              auth.rs (sector CPI-auth check), token_payment.rs (fee split + transfers)
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

`tests-rs` is a separate Cargo workspace with its own `Cargo.lock`, so LiteSVM's dependency tree never touches the program's lockfile. It exercises every instruction with exact-error assertions: admin signatures, CPI authentication, fee splits, the pause-adjusted inactivity clock, and payouts from the larger pool. It enables the `localnet` feature and refuses to run against a `.so` that does not embed the same admin keys. Set `CORE_VAULT_SO=/path/to/other.so` to run it against a different build (used for mutation testing).

Don't use plain `anchor test`: it would load the real-key build, which the suite cannot sign for.

## Design notes

- **Minimal money surface.** Only `deposit_fee`, `deposit_reset` and `request_payout` move tokens.
- **Stale paths return `Ok`.** An inactivity timeout has to persist the `Abandoned` state, so the stale path of `record_activity` and `request_payout` returns success plus return data (`ActivityOutcome` / `PayoutOutcome`) instead of an error, which would roll the state change back.
- **Fee split is validated.** `fee_split_bps` above 10,000 is rejected in `register_product` and `update_product_config`.
- **Large accounts are boxed.** Wide `Accounts` structs use `Box<Account<…>>` to stay under the SBF stack limit.
