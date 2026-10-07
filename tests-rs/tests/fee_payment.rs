//! Token movement for deposit_fee and deposit_reset: the same payment rules,
//! so every test below runs against BOTH instructions (`fee` / `reset`).
//!
//! Split: pool = floor(amount * fee_split_bps / 10_000), sl8 = the remainder.
mod common;
use anchor_lang::error::ErrorCode;
use anchor_lang::solana_program::instruction::Instruction;
use anchor_lang::solana_program::pubkey::Pubkey;
use anchor_spl::token::spl_token::{self, state::AccountState};
use common::*;
use core_vault::errors::VaultError;
use litesvm::types::TransactionResult;
use std::collections::BTreeMap;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Fee,
    Reset,
}

/// Registered product (+ funded trader) ready to make one payment of `amount`.
/// Fee:   buys tier (SIZE, cost = amount) as challenge 1.
/// Reset: challenge 1 (tier SIZE) is Failed; resets it into challenge 2 at phase 1.
struct Rig {
    e: Env,
    s: Sector,
    w: Pubkey,
    kind: Kind,
    size: u64,
    amount: u64,
    bps: u16,
}

const SIZE: u64 = 10_000_000_000; // 10,000 coins
const PHASE: u8 = 1;
const PHASE_BPS: u64 = 150;

impl Rig {
    /// `x` = the fee (Fee) or the account size whose phase-1 price is paid (Reset).
    fn new(kind: Kind, split_bps: u16, x: u64) -> Rig {
        let (size, amount) = match kind {
            Kind::Fee => (SIZE, x),
            Kind::Reset => (x, (x as u128 * PHASE_BPS as u128 / 10_000) as u64),
        };
        let cfg = Cfg {
            fee_split_bps: split_bps,
            tiers: vec![ChallengeSize { size, cost: if kind == Kind::Fee { amount } else { 1 } }],
            max_payout: 5,
            reset_bps: vec![100, PHASE_BPS as u16, 450],
        };
        // register_product now refuses fee_split_bps above 10_000, so a bad
        // value is written straight into the registry account AFTER setup
        // (the payment-time guard is what these states exercise).
        let cfg = Cfg { fee_split_bps: split_bps.min(10_000), ..cfg };
        let (mut e, s) = Env::registered(&cfg);
        let w = wallet();
        e.fund_wallet(&w);
        if kind == Kind::Reset {
            e.deposit_tier(&s, &w, 1, (size, 1));
            e.flag(&s, &w, 1);
        }
        if split_bps > 10_000 {
            e.set_registry(&s, |r| r.fee_split_bps = split_bps);
            assert_eq!(e.registry(&s).fee_split_bps, split_bps);
        }
        Rig { e, s, w, kind, size, amount, bps: split_bps }
    }

    fn slots(&self) -> Slots {
        match self.kind {
            Kind::Fee => DF,
            Kind::Reset => DR,
        }
    }

    fn ix(&self, c: Coin) -> Instruction {
        match self.kind {
            Kind::Fee => deposit_fee_ix_coin(&self.e, &self.s, &self.w, 1, self.amount, self.size, c),
            Kind::Reset => reset_ix_coin(&self.e, &self.s, &self.w, 1, 2, self.amount, PHASE, c),
        }
    }

    fn pay(&mut self, c: Coin) -> TransactionResult {
        let ix = self.ix(c);
        self.e.send(ix)
    }

    /// The independent split formula (u128), not the program's code.
    fn expected_split(&self) -> (u64, u64) {
        let pool = (self.amount as u128 * self.bps as u128 / 10_000) as u64;
        (pool, self.amount - pool)
    }

    /// A rejected payment changed nothing: no balance moved, no record was
    /// created, and (reset) the predecessor was not consumed.
    fn assert_no_effect(&self, before: &BTreeMap<Pubkey, u64>) {
        assert_snapshot_unchanged(before, &self.e.token_snapshot());
        match self.kind {
            Kind::Fee => assert!(self.e.trader_opt(&self.s, &self.w, 1).is_none(), "no TraderState may be created"),
            Kind::Reset => {
                assert!(self.e.trader_opt(&self.s, &self.w, 2).is_none(), "no new record may be created");
                let prev = self.e.trader(&self.s, &self.w, 1);
                assert!(!prev.reset_used, "a failed reset must not burn reset_used");
                assert_eq!(prev.status, TraderStatus::Failed);
            }
        }
    }

    /// After a successful payment in `c`: exact deltas, other coin untouched,
    /// nothing created or destroyed, record written.
    fn assert_paid(&self, c: Coin, before: &BTreeMap<Pubkey, u64>) {
        let after = self.e.token_snapshot();
        let (pool, sl8) = self.expected_split();
        assert_eq!(pool + sl8, self.amount);
        let (mint, pool_acct, sl8_acct) = self.e.coin(c);
        let trader_ta = self.e.wallet_ta(&self.w, c);
        for (addr, b) in before {
            let a = after[addr];
            let want = if *addr == trader_ta {
                b - self.amount
            } else if *addr == pool_acct {
                b + pool
            } else if *addr == sl8_acct {
                b + sl8
            } else {
                *b
            };
            assert_eq!(a, want, "balance of {addr} (coin {c:?}, amount {}, bps {})", self.amount, self.bps);
        }
        // conservation, per mint
        for m in [self.e.usdc, self.e.usdt] {
            let sum = |snap: &BTreeMap<Pubkey, u64>| -> u128 {
                snap.iter().filter(|(a, _)| self.e.token_state(a).mint == m).map(|(_, v)| *v as u128).sum()
            };
            assert_eq!(sum(before), sum(&after), "tokens must be conserved for mint {m}");
        }
        let _ = mint;
        match self.kind {
            Kind::Fee => {
                let ts = self.e.trader(&self.s, &self.w, 1);
                assert_eq!((ts.status, ts.account_size), (TraderStatus::Active, self.size));
                assert_eq!(ts.trader_wallet, self.w);
                assert_eq!(ts.product_program_id, self.s.id);
            }
            Kind::Reset => {
                let new = self.e.trader(&self.s, &self.w, 2);
                assert_eq!((new.status, new.account_size), (TraderStatus::Active, self.size));
                assert_eq!(new.trader_wallet, self.w);
                assert_eq!(new.product_program_id, self.s.id);
                assert!(self.e.trader(&self.s, &self.w, 1).reset_used);
            }
        }
    }
}

macro_rules! for_both {
    ($name:ident, $f:ident) => {
        mod $name {
            use super::*;
            #[test]
            fn fee() {
                $f(Kind::Fee)
            }
            #[test]
            fn reset() {
                $f(Kind::Reset)
            }
        }
    };
}

// ------------------------------------------------------------------ the split

fn split_usdc(kind: Kind) {
    let mut r = Rig::new(kind, 6500, if kind == Kind::Fee { 1_000_000 } else { 10_000_000_000 });
    let before = r.e.token_snapshot();
    assert_ok(r.pay(Coin::Usdc));
    r.assert_paid(Coin::Usdc, &before);
    let (pool, sl8) = r.expected_split();
    // spot values, so the formula above can't hide a shared mistake
    match kind {
        Kind::Fee => assert_eq!((pool, sl8), (650_000, 350_000)),
        Kind::Reset => assert_eq!((r.amount, pool, sl8), (150_000_000, 97_500_000, 52_500_000)),
    }
}
for_both!(split_usdc_exact_deltas, split_usdc);

fn split_usdt(kind: Kind) {
    let mut r = Rig::new(kind, 6500, if kind == Kind::Fee { 2_500_000 } else { 50_000_000_000 });
    let before = r.e.token_snapshot();
    assert_ok(r.pay(Coin::Usdt));
    r.assert_paid(Coin::Usdt, &before);
    // USDC side completely untouched
    for a in [r.e.usdc_pool, r.e.sl8_usdc, r.e.wallet_ta(&r.w, Coin::Usdc)] {
        assert_eq!(r.e.token_balance(&a), before[&a]);
    }
}
for_both!(split_usdt_exact_deltas, split_usdt);

fn awkward_amounts(kind: Kind) {
    // (split bps, x): amounts whose split is not a whole number
    let cases: &[(u16, u64)] = &[
        (6500, 12_345_679),
        (6500, 12_345_678_901),
        (3333, 999_999_937),
        (1, 9_999_999),
        (9_999, 1_234_567),
        (6500, 3),
        (6500, 1),
        (7777, u32::MAX as u64),
        (6500, 40_000_000_000_000), // 40M coins
    ];
    for &(bps, x) in cases {
        let mut r = Rig::new(kind, bps, x);
        if r.amount == 0 {
            continue;
        }
        r.e.fund_wallet_with(&r.w.clone(), u64::MAX / 4, u64::MAX / 4);
        let before = r.e.token_snapshot();
        assert_ok(r.pay(Coin::Usdc));
        r.assert_paid(Coin::Usdc, &before);
        let (pool, sl8) = r.expected_split();
        let exact = r.amount as u128 * bps as u128;
        assert_eq!(pool as u128, exact / 10_000, "pool is the floor");
        assert!(sl8 as u128 * 10_000 >= r.amount as u128 * (10_000 - bps as u128), "SL8 gets the rounding remainder");
    }
}
for_both!(awkward_amounts_floor_to_pool_remainder_to_sl8, awkward_amounts);

#[test]
fn the_headline_awkward_fee_12_345_679_at_6500() {
    let mut r = Rig::new(Kind::Fee, 6500, 12_345_679);
    let (b_pool, b_sl8, b_tr) = (
        r.e.token_balance(&r.e.usdc_pool),
        r.e.token_balance(&r.e.sl8_usdc),
        r.e.token_balance(&r.e.wallet_ta(&r.w, Coin::Usdc)),
    );
    assert_ok(r.pay(Coin::Usdc));
    // 12_345_679 * 6500 / 10_000 = 8_024_691.35 -> floor 8_024_691 ; remainder 4_320_988
    assert_eq!(r.e.token_balance(&r.e.usdc_pool) - b_pool, 8_024_691);
    assert_eq!(r.e.token_balance(&r.e.sl8_usdc) - b_sl8, 4_320_988);
    assert_eq!(b_tr - r.e.token_balance(&r.e.wallet_ta(&r.w, Coin::Usdc)), 12_345_679);
}

// ------------------------------------------------------- fee_split_bps edges

fn split_edges(kind: Kind) {
    let x = if kind == Kind::Fee { 1_000_001 } else { 10_000_001 };
    // 0 bps: everything to SL8, the zero-amount pool leg is skipped (not an error)
    let mut r = Rig::new(kind, 0, x);
    let before = r.e.token_snapshot();
    assert_ok(r.pay(Coin::Usdc));
    r.assert_paid(Coin::Usdc, &before);
    assert_eq!(r.expected_split(), (0, r.amount));
    assert_eq!(r.e.token_balance(&r.e.usdc_pool), 0);

    // 10_000 bps: everything to the pool, the zero-amount SL8 leg is skipped
    let mut r = Rig::new(kind, 10_000, x);
    let before = r.e.token_snapshot();
    assert_ok(r.pay(Coin::Usdc));
    r.assert_paid(Coin::Usdc, &before);
    assert_eq!(r.expected_split(), (r.amount, 0));
    assert_eq!(r.e.token_balance(&r.e.sl8_usdc), 0);

    // 6500 bps
    let mut r = Rig::new(kind, 6500, x);
    let before = r.e.token_snapshot();
    assert_ok(r.pay(Coin::Usdt));
    r.assert_paid(Coin::Usdt, &before);

    // tiny amounts at the edges: floor leaves the pool with nothing
    let mut r = Rig::new(kind, 1, if kind == Kind::Fee { 9_999 } else { 6_667 });
    assert!(r.amount > 0 && r.expected_split().0 == 0, "setup: pool share floors to zero");
    let before = r.e.token_snapshot();
    assert_ok(r.pay(Coin::Usdc));
    r.assert_paid(Coin::Usdc, &before);
}
for_both!(fee_split_bps_edge_values, split_edges);

fn split_above_100_percent_fails_closed(kind: Kind) {
    let mut r = Rig::new(kind, 10_001, if kind == Kind::Fee { 1_000_000 } else { 10_000_000_000 });
    let before = r.e.token_snapshot();
    assert_vault_err(&r.pay(Coin::Usdc), VaultError::InvalidFeeSplit);
    r.assert_no_effect(&before);
}
for_both!(fee_split_bps_above_10000_is_rejected, split_above_100_percent_fails_closed);

fn zero_amount_moves_nothing(kind: Kind) {
    // Fee: tier cost 0. Reset: price floors to 0 (99 * 150 / 10_000 = 1.485 -> 1? use 66 -> 0.99 -> 0)
    let mut r = Rig::new(kind, 6500, if kind == Kind::Fee { 0 } else { 66 });
    assert_eq!(r.amount, 0);
    r.e.fund_wallet_with(&r.w.clone(), 0, 0); // even an empty wallet can "pay" nothing
    let before = r.e.token_snapshot();
    assert_ok(r.pay(Coin::Usdc));
    assert_eq!(r.e.token_snapshot(), before, "no token moved");
    match kind {
        Kind::Fee => assert_eq!(r.e.trader(&r.s, &r.w, 1).status, TraderStatus::Active),
        Kind::Reset => assert_eq!(r.e.trader(&r.s, &r.w, 2).status, TraderStatus::Active),
    }
}
for_both!(zero_amount_payment_succeeds_without_transfers, zero_amount_moves_nothing);

// -------------------------------------------------------------- rejections

/// Runs `corrupt` on the instruction, expects `check` on the result, and that
/// nothing at all changed.
fn rejected(kind: Kind, corrupt: impl FnOnce(&mut Rig, &mut Instruction, Slots), check: impl FnOnce(&TransactionResult)) {
    let mut r = Rig::new(kind, 6500, if kind == Kind::Fee { 1_000_000 } else { 10_000_000_000 });
    let before = r.e.token_snapshot();
    let mut ix = r.ix(Coin::Usdc);
    let sl = r.slots();
    corrupt(&mut r, &mut ix, sl);
    let res = r.e.send(ix);
    check(&res);
    // snapshot again (corrupt() may have created extra token accounts)
    let after = r.e.token_snapshot();
    for (k, v) in &before {
        assert_eq!(after[k], *v, "balance of {k} changed on a rejected payment");
    }
    r.assert_no_effect(&after);
}

fn third_mint(kind: Kind) {
    rejected(
        kind,
        |r, ix, sl| {
            let m3 = Pubkey::new_unique();
            r.e.set_mint(&m3, 6, spl_token::ID);
            let w = r.w;
            let ta = r.e.new_token_account(&m3, &w, WALLET_START);
            ix.accounts[sl.mint].pubkey = m3;
            ix.accounts[sl.trader_ta].pubkey = ta;
        },
        |res| assert_vault_err(res, VaultError::InvalidMint),
    );
}
for_both!(rejects_a_valid_mint_that_is_not_the_vaults, third_mint);

fn pool_of_wrong_mint(kind: Kind) {
    rejected(
        kind,
        |r, ix, sl| ix.accounts[sl.pool].pubkey = r.e.usdt_pool, // USDC mint, USDT pool
        |res| assert_vault_err(res, VaultError::InvalidTokenAccount),
    );
}
for_both!(rejects_the_usdt_pool_for_a_usdc_payment, pool_of_wrong_mint);

fn pool_is_some_other_account(kind: Kind) {
    rejected(
        kind,
        |r, ix, sl| {
            let atk = Pubkey::new_unique();
            let usdc = r.e.usdc;
            ix.accounts[sl.pool].pubkey = r.e.new_token_account(&usdc, &atk, 0);
        },
        |res| assert_vault_err(res, VaultError::InvalidTokenAccount),
    );
}
for_both!(rejects_a_non_pool_account_as_pool, pool_is_some_other_account);

fn sl8_owned_by_someone_else(kind: Kind) {
    rejected(
        kind,
        |r, ix, sl| {
            let atk = Pubkey::new_unique();
            let usdc = r.e.usdc;
            ix.accounts[sl.sl8_ta].pubkey = r.e.new_token_account(&usdc, &atk, 0);
        },
        |res| assert_vault_err(res, VaultError::InvalidTokenAccount),
    );
}
for_both!(rejects_an_sl8_account_owned_by_someone_else, sl8_owned_by_someone_else);

fn sl8_wrong_mint(kind: Kind) {
    rejected(
        kind,
        |r, ix, sl| ix.accounts[sl.sl8_ta].pubkey = r.e.sl8_usdt, // right owner, wrong mint
        |res| assert_vault_err(res, VaultError::InvalidTokenAccount),
    );
}
for_both!(rejects_an_sl8_account_of_the_wrong_mint, sl8_wrong_mint);

fn sl8_is_the_pool(kind: Kind) {
    rejected(
        kind,
        |r, ix, sl| ix.accounts[sl.sl8_ta].pubkey = r.e.usdc_pool, // owner = vault PDA, not sl8_wallet
        |res| assert_vault_err(res, VaultError::InvalidTokenAccount),
    );
}
for_both!(rejects_the_pool_account_in_the_sl8_slot, sl8_is_the_pool);

fn trader_ta_owned_by_someone_else(kind: Kind) {
    let victim_balance = std::cell::Cell::new(0u64);
    rejected(
        kind,
        |r, ix, sl| {
            // an unrelated wallet's funded account; the real trader signs
            let victim = Pubkey::new_unique();
            let usdc = r.e.usdc;
            let ta = r.e.new_token_account(&usdc, &victim, 5_000_000_000);
            victim_balance.set(5_000_000_000);
            ix.accounts[sl.trader_ta].pubkey = ta;
        },
        |res| assert_vault_err(res, VaultError::InvalidTokenAccount),
    );
    assert_eq!(victim_balance.get(), 5_000_000_000);
}
for_both!(rejects_a_trader_account_owned_by_someone_else, trader_ta_owned_by_someone_else);

fn trader_ta_wrong_mint(kind: Kind) {
    rejected(
        kind,
        |r, ix, sl| ix.accounts[sl.trader_ta].pubkey = r.e.wallet_ta(&r.w, Coin::Usdt), // USDT account, USDC mint
        |res| assert_vault_err(res, VaultError::InvalidTokenAccount),
    );
}
for_both!(rejects_a_trader_account_of_the_wrong_mint, trader_ta_wrong_mint);

fn trader_not_a_signer(kind: Kind) {
    rejected(
        kind,
        |_, ix, sl| ix.accounts[sl.trader].is_signer = false,
        |res| assert_anchor_err(res, ErrorCode::AccountNotSigner),
    );
}
for_both!(rejects_an_unsigned_trader, trader_not_a_signer);

fn trader_differs_from_wallet_arg(kind: Kind) {
    rejected(
        kind,
        |r, ix, sl| {
            // a different wallet that signs and owns a funded account of its own
            let other = Pubkey::new_unique();
            let usdc = r.e.usdc;
            let ta = r.e.new_token_account(&usdc, &other, WALLET_START);
            ix.accounts[sl.trader].pubkey = other;
            ix.accounts[sl.trader_ta].pubkey = ta;
        },
        |res| assert_vault_err(res, VaultError::TraderWalletMismatch),
    );
}
for_both!(rejects_a_signer_that_is_not_the_trader_wallet_arg, trader_differs_from_wallet_arg);

fn insufficient_balance(kind: Kind) {
    let mut r = Rig::new(kind, 6500, if kind == Kind::Fee { 1_000_000 } else { 10_000_000_000 });
    let w = r.w;
    r.e.fund_wallet_with(&w, r.amount - 1, 0);
    let before = r.e.token_snapshot();
    assert_vault_err(&r.pay(Coin::Usdc), VaultError::InsufficientTokenBalance);
    r.assert_no_effect(&before);
    // exactly the amount is enough
    r.e.fund_wallet_with(&w, r.amount, 0);
    let before = r.e.token_snapshot();
    assert_ok(r.pay(Coin::Usdc));
    r.assert_paid(Coin::Usdc, &before);
    assert_eq!(r.e.token_balance(&r.e.wallet_ta(&w, Coin::Usdc)), 0);
}
for_both!(rejects_insufficient_balance_but_accepts_exactly_enough, insufficient_balance);

fn wrong_token_program(kind: Kind) {
    rejected(
        kind,
        |_, ix, sl| ix.accounts[sl.token_program].pubkey = TOKEN_2022_ID,
        |res| assert_anchor_err(res, ErrorCode::InvalidProgramId),
    );
}
for_both!(rejects_the_token_2022_program, wrong_token_program);

fn token_2022_mint(kind: Kind) {
    rejected(
        kind,
        |r, ix, sl| {
            let m22 = Pubkey::new_unique();
            r.e.set_mint(&m22, 6, TOKEN_2022_ID);
            ix.accounts[sl.mint].pubkey = m22;
        },
        |res| assert_anchor_err(res, ErrorCode::AccountOwnedByWrongProgram),
    );
}
for_both!(rejects_a_token_2022_owned_mint, token_2022_mint);

fn bogus_vault_state(kind: Kind) {
    rejected(
        kind,
        |_, ix, sl| ix.accounts[sl.vault].pubkey = Pubkey::new_unique(),
        |res| assert_anchor_err(res, ErrorCode::AccountNotInitialized),
    );
}
for_both!(rejects_a_vault_state_that_does_not_exist, bogus_vault_state);

fn frozen_trader_account(kind: Kind) {
    // Passes every vault check, then the token program refuses inside the CPI
    // (AccountFrozen = 17). All the earlier state writes must roll back.
    rejected(
        kind,
        |r, _, _| {
            let ta = r.e.wallet_ta(&r.w, Coin::Usdc);
            r.e.edit_token_account(&ta, |a| a.state = AccountState::Frozen);
        },
        |res| assert_custom_code(res, 17, "SPL Token AccountFrozen"),
    );
}
for_both!(a_token_program_failure_rolls_back_the_record, frozen_trader_account);

fn frozen_pool_account_rolls_back_too(kind: Kind) {
    // The first leg succeeds, the SECOND leg fails: balances must still be untouched.
    rejected(
        kind,
        |r, _, _| {
            let sl8 = r.e.sl8_usdc;
            r.e.edit_token_account(&sl8, |a| a.state = AccountState::Frozen);
        },
        |res| assert_custom_code(res, 17, "SPL Token AccountFrozen (second leg)"),
    );
}
for_both!(a_failing_second_transfer_undoes_the_first, frozen_pool_account_rolls_back_too);

// ------------------------------------------------ pre-existing rule failures
// pay nothing, too.

#[test]
fn fee_tier_mismatch_and_duplicate_id_move_no_tokens() {
    let mut r = Rig::new(Kind::Fee, 6500, 1_000_000);
    let before = r.e.token_snapshot();
    let bad = deposit_fee_ix_coin(&r.e, &r.s, &r.w, 1, 1_000_001, SIZE, Coin::Usdc);
    assert_vault_err(&r.e.send(bad), VaultError::InvalidChallengeTier);
    r.assert_no_effect(&before);

    assert_ok(r.pay(Coin::Usdc));
    let after_first = r.e.token_snapshot();
    assert_already_in_use(&r.pay(Coin::Usdc));
    assert_snapshot_unchanged(&after_first, &r.e.token_snapshot());
}

#[test]
fn reset_wrong_amount_and_second_use_move_no_tokens() {
    let mut r = Rig::new(Kind::Reset, 6500, 10_000_000_000);
    let before = r.e.token_snapshot();
    let bad = reset_ix_coin(&r.e, &r.s, &r.w, 1, 2, r.amount + 1, PHASE, Coin::Usdc);
    assert_vault_err(&r.e.send(bad), VaultError::WrongAmount);
    r.assert_no_effect(&before);

    assert_ok(r.pay(Coin::Usdc));
    let after_first = r.e.token_snapshot();
    let again = reset_ix_coin(&r.e, &r.s, &r.w, 1, 3, r.amount, PHASE, Coin::Usdc);
    assert_vault_err(&r.e.send(again), VaultError::ResetNotAllowed);
    assert_snapshot_unchanged(&after_first, &r.e.token_snapshot());
}

#[test]
fn a_rejected_reset_can_be_retried_successfully() {
    // The strongest form of "a failed reset must not burn reset_used".
    let mut r = Rig::new(Kind::Reset, 6500, 10_000_000_000);
    let w = r.w;
    r.e.fund_wallet_with(&w, r.amount - 1, 0);
    assert_vault_err(&r.pay(Coin::Usdc), VaultError::InsufficientTokenBalance);
    assert!(!r.e.trader(&r.s, &w, 1).reset_used);
    r.e.fund_wallet_with(&w, r.amount, 0);
    let before = r.e.token_snapshot();
    assert_ok(r.pay(Coin::Usdc));
    r.assert_paid(Coin::Usdc, &before);
}

fn huge_amounts_need_wide_math(kind: Kind) {
    // amount * 6500 overflows u64 (1.8e19) long before amount does.
    let x = if kind == Kind::Fee { 1_000_000_000_000_000_000 } else { 10_000_000_000_000_000_000 };
    let mut r = Rig::new(kind, 6500, x);
    assert!(r.amount as u128 * 6500 > u64::MAX as u128, "setup: the product must not fit in u64");
    let w = r.w;
    r.e.fund_wallet_with(&w, u64::MAX / 4, 0);
    let before = r.e.token_snapshot();
    assert_ok(r.pay(Coin::Usdc));
    r.assert_paid(Coin::Usdc, &before);
}
for_both!(huge_amounts_are_split_with_wide_math, huge_amounts_need_wide_math);

fn zero_legs_are_skipped(kind: Kind) {
    // SPL Token rejects even a 0-amount transfer that touches a frozen account.
    // So a skipped zero leg is observable: with a frozen destination whose share
    // is exactly zero, the payment must still go through.
    let x = if kind == Kind::Fee { 1_000_000 } else { 10_000_000_000 };

    // 10_000 bps: SL8's share is 0 -> a frozen SL8 account must not matter
    let mut r = Rig::new(kind, 10_000, x);
    let sl8 = r.e.sl8_usdc;
    r.e.edit_token_account(&sl8, |a| a.state = AccountState::Frozen);
    let before = r.e.token_snapshot();
    assert_ok(r.pay(Coin::Usdc));
    r.assert_paid(Coin::Usdc, &before);

    // 0 bps: the pool's share is 0 -> a frozen pool account must not matter
    let mut r = Rig::new(kind, 0, x);
    let pool = r.e.usdc_pool;
    r.e.edit_token_account(&pool, |a| a.state = AccountState::Frozen);
    let before = r.e.token_snapshot();
    assert_ok(r.pay(Coin::Usdc));
    r.assert_paid(Coin::Usdc, &before);
}
for_both!(zero_amount_legs_are_skipped_not_sent, zero_legs_are_skipped);
