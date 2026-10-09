# devnet-sector: mock sector program

**TEST SCAFFOLDING. INSECURE BY DESIGN. NEVER USE ON MAINNET.**

A deliberately tiny native (no Anchor) Solana program that stands in for a real sector program (lev-trading,
options, ...) so the core-vault program can be rehearsed on devnet. The vault's sector-only instructions
(`deposit_fee`, `request_payout`) can only be reached by CPI from a registered sector program; this is that program.

It is insecure on purpose:

- **Anyone can call `SetTally`** and write any count/total into the payout tally (that is the "deliberately wrong
  tally" switch for rehearsing `reconcile_product`).
- **The CPI target is whatever program account the caller passes last.** Nothing is baked in, so it works against
  any vault deployment, and equally against a malicious one.
- There is no authorisation of any kind: no admin, no allow-list, no signer checks beyond what the vault itself
  enforces on the CPI.

Register it in a vault only on devnet/localnet, with throwaway funds. Do not deploy it with a real upgrade authority
you care about, and never register it on a mainnet vault.

Layout:

```
tools/devnet-sector/
  Cargo.toml        tests package + the workspace (own Cargo.lock, own target/)
  program/          the mock sector program (crate "mock-sector", lib name mock_sector)
  tests/            LiteSVM integration tests against the real core_vault.so
```

## Build and test

```sh
# 1. the vault, localnet (public test admin keys) build -> target/test-deploy/core_vault.so
scripts/build-test-so.sh

# 2. the mock sector -> tools/devnet-sector/target/deploy/mock_sector.so
cargo build-sbf --manifest-path tools/devnet-sector/program/Cargo.toml --sbf-out-dir tools/devnet-sector/target/deploy

# 3. tests (from tools/devnet-sector; --offline works once the git dep is cached)
cd tools/devnet-sector && cargo test
cargo clippy --all-targets -p mock-sector -p devnet-sector-tests
```

`cargo build-sbf` also drops `mock_sector-keypair.json` next to the `.so`: a throwaway program keypair under the
git-ignored `target/`. For a devnet deployment use your own devnet program keypair (`solana program deploy
--program-id <keypair>`), whose pubkey becomes the `product_program_id` you register in the vault.

The program id is not compiled in (the program uses the `program_id` it is invoked with), so one `.so` works at any
address. The `sector_authority` and tally PDAs are derived from whatever address it is deployed at.

## Instruction data

A Borsh enum: 1 byte tag, then little-endian fields (no 8-byte Anchor discriminator). Rust type: `mock_sector::SectorIx`
(build the program with `features = ["no-entrypoint"]` to use it from a client).

| tag | variant                                                                  | bytes after the tag                           |
|-----|--------------------------------------------------------------------------|-----------------------------------------------|
| 0   | `InitTally`                                                              | none (total 1 byte)                           |
| 1   | `SetTally { count: u64, total: u64 }`                                    | `count` u64 LE, `total` u64 LE (17 bytes)     |
| 2   | `DepositFee { amount: u64, challenge_id: u64, account_size: u64 }`       | three u64 LE (25 bytes)                       |
| 3   | `RequestPayout { amount: u64, challenge_id: u64, proposed_request_id: u64 }` | three u64 LE (25 bytes)                   |

Trailing bytes are rejected. There is **no `trader_wallet` field in `RequestPayout`**: the mock reads the trader
wallet from the `trader_state` account it is given (first field of the vault's `TraderState`, bytes `8..40` after
the Anchor discriminator). The vault re-derives the `trader_state` PDA from that wallet, so a wrong account just
fails there.

PDAs (both under the mock's own program id):

- `sector_authority = find_program_address(&[b"setl8_sector_authority"], &mock)` (`si::derive_sector_authority`)
- `tally = find_program_address(&[b"payout_tally"], &mock)` (`si::derive_payout_tally`), owned by the mock, 25 bytes:
  `"SL8TALLY"`, version 1, `requested_count` u64 LE, `requested_total` u64 LE.

## Accounts

`R` read-only, `W` writable, `S` signer. Positions are exact; the program rejects other lengths for 2 and 3.

### 0 `InitTally`

| # | account            | flags | notes |
|---|--------------------|-------|-------|
| 0 | payer              | W, S  | pays rent |
| 1 | tally PDA          | W     | must equal `derive_payout_tally(mock)` |
| 2 | system program     | R     | |

Creates the tally (owner = mock, 25 bytes) with count 0 / total 0. A pre-funded address is handled (top-up to rent
exemption, allocate, assign). Fails with `AccountAlreadyInitialized` if the tally already has data.

### 1 `SetTally`

| # | account   | flags | notes |
|---|-----------|-------|-------|
| 0 | tally PDA | W     | must equal `derive_payout_tally(mock)`, owned by the mock |

Overwrites count/total. No authorisation.

### 2 `DepositFee` (13 accounts)

The first 12 are exactly the accounts of the `setl8-shared-interfaces` v0.4.1 `deposit_fee` builder, in its order;
the 13th is the vault program.

| #  | account               | flags | notes |
|----|-----------------------|-------|-------|
| 0  | sector_authority PDA  | R     | **not a signer** in the outer transaction; the mock signs for it via `invoke_signed` |
| 1  | product_registry      | W     | vault PDA `[b"product_registry", mock]` |
| 2  | trader_state          | W     | vault PDA `[b"trader_state", mock, trader, challenge_id LE]`, created by the vault |
| 3  | payer                 | W, S  | pays the TraderState rent |
| 4  | system program        | R     | |
| 5  | vault_state           | R     | |
| 6  | trader                | R, S  | the paying trader; its key is passed as `trader_wallet` |
| 7  | trader_token_account  | W     | trader's USDC/USDT account |
| 8  | mint                  | R     | |
| 9  | pool_token_account    | W     | |
| 10 | sl8_token_account     | W     | |
| 11 | token_program         | R     | classic SPL Token |
| 12 | core-vault PROGRAM    | R     | CPI target = this account's key (not hardcoded) |

The CPI carries `product_program_id` = the mock's program id, `trader_wallet` = account 6, and the `amount`,
`challenge_id`, `account_size` from the instruction data. Account flags for 2..=11 are copied from the outer
instruction; the vault validates all of them.

### 3 `RequestPayout` (9 accounts)

The first 7 are exactly the accounts of the `si::request_payout` builder, in its order; then the tally and the vault
program.

| # | account               | flags | notes |
|---|-----------------------|-------|-------|
| 0 | sector_authority PDA  | R     | **not a signer** in the outer transaction; signed by the mock |
| 1 | product_registry      | W     | |
| 2 | trader_state          | W     | the trader wallet is read from its data |
| 3 | vault_state           | W     | |
| 4 | payout_claim          | W     | vault PDA `[b"payout_claim", trader_state, proposed_request_id LE]`, created by the vault |
| 5 | payer                 | W, S  | pays the claim rent |
| 6 | system program        | R     | |
| 7 | tally PDA             | W     | must equal `derive_payout_tally(mock)`, owned by the mock |
| 8 | core-vault PROGRAM    | R     | CPI target |

After the CPI succeeds the mock reads the return data (must come from the account passed at 8):

- `PayoutOutcome::Paid` (0): `requested_count += 1`, `requested_total += amount` (checked arithmetic, fails on overflow).
- `PayoutOutcome::Abandoned` (1): the tally is not touched (the vault created no claim).
- anything else, or no/foreign return data: the transaction fails (`MockError::MissingVaultReturnData`).

If the vault CPI fails the whole transaction fails with the vault's own error code (so the tally is untouched).

## Errors

Vault errors come through unchanged as `Custom(6000+n)`. The mock's own are `Custom(0x5300..)`:

| code   | `MockError`              | meaning |
|--------|--------------------------|---------|
| 0x5300 | `WrongSectorAuthority`   | account 0 is not the mock's `sector_authority` PDA |
| 0x5301 | `WrongTallyAddress`      | the tally account is not the mock's tally PDA |
| 0x5302 | `MissingVaultReturnData` | the vault returned no / unreadable / foreign return data |
| 0x5303 | `TallyOverflow`          | a tally addition would overflow u64 |

## Typical devnet rehearsal flow

1. Deploy `mock_sector.so`; note its program id `M`.
2. Vault admins (2-of-2) `register_product` with `product_program_id = M`.
3. `InitTally` (any payer). Until then a missing tally counts as 0/0 for the vault.
4. Trader signs `DepositFee`; then `RequestPayout` (payer + nobody else must sign; the sector authority PDA is
   signed by the mock). Anyone runs the vault's `reconcile_product` (match).
5. `SetTally` to a wrong value, run `reconcile_product`: the product is paused with the reconciliation reason, and
   the call still returns Ok.
