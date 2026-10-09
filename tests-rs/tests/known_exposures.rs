//! Runnable reproductions of the security-review findings that are NOT bugs the code
//! can fix on its own: each needs a founder decision, or is an accepted, documented
//! property. Every test PASSES today and pins the current behaviour; if a decision
//! changes the behaviour, the matching test must be changed on purpose in the same
//! commit. See docs/SECURITY-REVIEW.md (SR-xx) and docs/THREAT-MODEL.md.
mod common;
use anchor_spl::token::spl_token::state::AccountState;
use common::*;
use core_vault::constants::{BOND_6M_LOCK_SECS, HEARTBEAT_MIN_GAP_SECS, PAUSE_RECONCILIATION_DEFICIT};
use core_vault::errors::VaultError;
use core_vault::state::BondTerm;
use solana_signer::Signer;

const M: u64 = 1_000_000;

/// SR-21 (FIXED in module 4b). `request_payout` used to accept any `u64` amount, so ONE
/// request of about `u64::MAX` saturated `open_claims_total` and every later
/// `request_bond_payout` / `request_payout` failed with `MathOverflow`, locking bond
/// principal in. This test used to pin that failure (and `payout_claims::
/// the_total_cannot_overflow_u64` pinned the acceptance of the huge request). It is FLIPPED
/// to assert the fix: the $2.5M ceiling refuses the huge request, so the bond holder can
/// exit and ordinary requests keep working. The boundary tests are in `claims_ceiling.rs`.
/// What remains open is SR-02: a sector can still fill the $2.5M headroom and block new
/// requests until the pool pays the total down.
#[test]
fn sr21_one_huge_request_no_longer_locks_bond_exits_or_other_requests() {
    let (mut e, s) = Env::registered(&Cfg { max_payout: 3, ..Cfg::default() });
    let k = e.new_depositor();
    e.make_atas(&k.pubkey());
    e.bond_deposit(&k, 0, 100 * M, BondTerm::SixMonths, Coin::Usdc);
    let w = wallet();
    e.deposit(&s, &w, 1);
    let ix = payout_ix(&e, &s, &w, 1, u64::MAX - 5, 1);
    assert_vault_err(&e.send(ix), VaultError::ClaimsCeilingExceeded); // refused now
    assert_eq!(e.vault_state().open_claims_total, 0);

    e.advance(BOND_6M_LOCK_SECS);
    e.bond_request(&k, 0); // the bond holder exits
    assert!(e.bond(&k.pubkey(), 0).is_none());
    let w2 = wallet();
    e.deposit(&s, &w2, 1);
    e.payout(&s, &w2, 1, 10, 1); // and an ordinary request still works
}

/// SR-02 stays OPEN (the founder has not decided the sector payout mechanics): a registered
/// sector can still queue a claim up to the $2.5M ceiling, i.e. fill the headroom, and while
/// it stays unpaid every new request, bond exits included, is refused with
/// `ClaimsCeilingExceeded` until a heartbeat pays the total down.
#[test]
fn sr02_a_sector_can_still_fill_the_headroom_and_block_new_requests_until_it_is_paid_down() {
    let (mut e, s) = Env::registered(&Cfg { max_payout: 3, ..Cfg::default() });
    let k = e.new_depositor();
    e.make_atas(&k.pubkey());
    e.bond_deposit(&k, 0, 100 * M, BondTerm::SixMonths, Coin::Usdc);
    let w = wallet();
    e.deposit(&s, &w, 1);
    e.payout(&s, &w, 1, 2_500_000 * M, 1); // the whole ceiling
    e.advance(BOND_6M_LOCK_SECS);
    assert_vault_err(&e.bond_request_result(&k, 0), VaultError::ClaimsCeilingExceeded);
    assert!(e.bond(&k.pubkey(), 0).is_some(), "the principal waits in its position, nothing is lost");
}

/// SR-01. ONE key (the SL8 admin key) turns its own bond into pool money. SL8's token
/// account is both the bond's source and SL8's destination, so on deposit it gets half
/// the principal (and the whole fee) straight back; after the hard lock the claim
/// returns the full principal from the pool. Net: the SL8 key gains about half the
/// principal and the pool loses the same, with no second signature and no 25% reserve.
#[test]
fn sr01_one_key_recycles_its_own_bond_into_pool_money() {
    let mut e = Env::new();
    e.set_pool(Coin::Usdc, 1_000 * M); // "other people's" trader-fee money
    let sl8 = e.sl8.pubkey();
    let sl8_usdc = e.sl8_usdc;
    let usdc = e.usdc;
    e.set_token_account(&sl8_usdc, &usdc, &sl8, 10_000 * M);
    e.make_atas(&sl8);
    e.fund_wallet(&sl8);
    let before = e.token_balance(&sl8_usdc) + e.token_balance(&ata(&sl8, &usdc));
    let pool_before = e.pools().0;

    let sl8_kp = dup(&e.sl8);
    let mut ix = deposit_bond_ix(&e, &sl8, 0, 1_000 * M, BondTerm::SixMonths, Coin::Usdc);
    let own_ta = e.wallet_ta(&sl8, Coin::Usdc);
    swap_account(&mut ix, &own_ta, &sl8_usdc); // pay from SL8's own destination account
    assert_ok(e.send_as(ix, &sl8_kp));
    assert_eq!(e.pools().0, pool_before + 500 * M, "the pool received half of the principal");
    assert_eq!(e.token_balance(&sl8_usdc), 10_000 * M - 500 * M, "SL8 net cost of the deposit is only half the principal");

    e.advance(BOND_6M_LOCK_SECS);
    e.bond_request(&sl8_kp, 0);
    e.begin();
    e.settle(&[(bond_claim_pda(&sl8, 0).0, ata(&sl8, &usdc), ata(&sl8, &e.usdt))]);

    let after = e.token_balance(&sl8_usdc) + e.token_balance(&ata(&sl8, &usdc));
    assert_eq!(after - before, 498 * M, "SL8 ends up 498 USDC richer (half the principal less the 0.4% fees)");
    assert_eq!(e.pools().0, pool_before + 500 * M - 998 * M + 0, "...and the pool is 498 USDC poorer, paid out of other people's fees");
}

/// SR-02. The vault trusts the sector's `amount`. A registered sector (or a bug in
/// it) can queue a claim far above the pool; the pro-rata ratio then hands almost the
/// whole pool to that claim and starves an honest one. `reconcile_product` agrees
/// (the sector's own tally matches) so it does not help.
#[test]
fn sr02_a_registered_sector_can_queue_an_arbitrary_amount() {
    let (mut e, s) = Env::registered(&Cfg { max_payout: 3, ..Cfg::default() });
    let (greedy, honest) = (wallet(), wallet());
    e.deposit(&s, &greedy, 1);
    e.deposit(&s, &honest, 1);
    e.make_atas(&greedy);
    e.make_atas(&honest);
    e.set_pool(Coin::Usdc, 1_000 * M);
    e.set_pool(Coin::Usdt, 0);
    e.payout(&s, &honest, 1, 100 * M, 1);
    e.payout(&s, &greedy, 1, 1_000_000 * M, 1); // 1,000x the pool
    let reg = e.registry(&s);
    e.set_tally(&s, reg.total_requests_emitted, reg.total_requested_amount);
    e.reconcile(&s);
    assert!(e.registry(&s).active, "a consistent over-report is invisible to reconciliation");

    e.begin();
    e.settle(&[triple(&e, &s, &honest, 1, 1), triple(&e, &s, &greedy, 1, 1)]);
    let (g, h) = (e.token_balance(&ata(&greedy, &e.usdc)), e.token_balance(&ata(&honest, &e.usdc)));
    assert!(g > 999 * M, "the inflated claim took almost the whole pool: {g}");
    assert!(h < M / 5, "the honest 100 USDC claim got {h} base units");
}

/// SR-03 (FIXED in module 4b). If the issuer freezes a payout POOL token account, the
/// heartbeat treats it as empty instead of reverting: `begin_heartbeat` leaves it out of the
/// available snapshot and `settle_claims` pays from the other pool only (both frozen -> a
/// zero payment). This test used to pin the wedge (settle reverted with AccountFrozen, the
/// cycle could never finish); it is FLIPPED to assert the fix. The full matrix (USDC, USDT,
/// both, before begin, between begin and settle, between batches, thaw) is in
/// `frozen_pools.rs`.
#[test]
fn sr03_a_frozen_pool_no_longer_wedges_the_heartbeat() {
    let (mut e, s) = Env::registered(&Cfg::default());
    let (w, _) = e.queue_claim(&s, 100 * M);
    e.make_atas(&w);
    let t = triple(&e, &s, &w, 1, 1);
    e.set_pool(Coin::Usdc, 1_000 * M);
    let pool = e.usdc_pool;
    e.edit_token_account(&pool, |a| a.state = AccountState::Frozen);
    e.begin();
    e.settle(&[t]); // no revert: the frozen pool counts as empty, the claim carries over
    assert_eq!(e.claim(&s, &w, 1, 1).owed, 100 * M);
    e.finalize(); // the cycle finishes
    assert!(!e.vault_state().cycle_active);
}

/// SR-04. A claim needs BOTH of the trader's associated accounts to be usable even
/// when only one pool would pay. A trader with a USDC account but no USDT account is
/// skipped every cycle (counted processed, stays owed) until they create the second.
#[test]
fn sr04_both_associated_accounts_must_exist() {
    let (mut e, s) = Env::registered(&Cfg::default());
    let (w, _) = e.queue_claim(&s, 100 * M);
    e.make_ata(&w, Coin::Usdc, 0); // only USDC
    e.set_pool(Coin::Usdc, 1_000 * M);
    e.begin();
    e.settle(&[triple(&e, &s, &w, 1, 1)]);
    assert_eq!(e.claim(&s, &w, 1, 1).owed, 100 * M, "skipped: nothing was paid although the USDC pool could pay it all");
    e.finalize();
    e.advance(HEARTBEAT_MIN_GAP_SECS);
    e.make_ata(&w, Coin::Usdt, 0); // the fix is on the trader's side
    e.begin();
    e.settle(&[triple(&e, &s, &w, 1, 1)]);
    assert!(e.claim_opt(&s, &w, 1, 1).is_none(), "paid in full one cycle later");
}

/// SR-05. The vault's request counters only ever grow and no instruction repairs them.
/// A sector whose tally over-reports can be reactivated by the admins but is paused
/// again by the very next reconcile: the product is permanently unusable.
#[test]
fn sr05_an_over_reporting_sector_can_never_reconcile_again() {
    let (mut e, s) = Env::registered(&Cfg::default());
    e.set_tally(&s, 5, 500); // the sector claims 5 requests; the vault accepted none
    e.reconcile(&s);
    assert_eq!(e.registry(&s).pause_reason, PAUSE_RECONCILIATION_DEFICIT);
    for _ in 0..3 {
        e.resume(&s); // 2-of-2 reactivation
        assert!(e.registry(&s).active);
        e.reconcile(&s); // anyone
        assert!(!e.registry(&s).active, "re-paused immediately");
    }
}

/// SR-06. A claim is processed once per cycle and an already-processed claim is a hard
/// error, so a keeper whose batch includes a claim another keeper just handled has its
/// whole transaction reverted and must rebuild it.
#[test]
fn sr06_a_front_runner_reverts_a_keepers_batch() {
    let (mut e, s) = Env::registered(&Cfg::default());
    let (wx, wy) = (e.queue_claim(&s, 100 * M).0, e.queue_claim(&s, 100 * M).0);
    e.make_atas(&wx);
    e.make_atas(&wy);
    e.set_pool(Coin::Usdc, 50 * M); // ratio 1/4: both claims stay open after being processed
    e.begin();
    let tx = triple(&e, &s, &wx, 1, 1);
    let ty = triple(&e, &s, &wy, 1, 1);
    e.settle(&[tx]); // keeper A
    assert_vault_err(&e.settle_result(&[tx, ty]), VaultError::ClaimAlreadySettled); // keeper B
    assert_eq!(e.claim(&s, &wy, 1, 1).last_settled_cycle, 0, "B's second claim was left untouched too");
    e.settle(&[ty]);
    e.finalize();
}
