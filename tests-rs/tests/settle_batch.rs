//! MAX_SETTLE_BATCH is proven, not guessed: a full batch fits a legacy transaction
//! (<= 1232 bytes) and a documented compute limit, even in a deliberately bad case.
mod common;
use anchor_lang::solana_program::pubkey::Pubkey;
use common::*;
use core_vault::constants::MAX_SETTLE_BATCH as MAX;
use solana_signer::Signer;

/// Largest legacy transaction (bytes).
const PACKET: usize = 1232;
/// The compute limit the batch is documented to fit in. A full batch of the
/// worst-case wallets below stays well inside it; the default per-instruction
/// limit is 200,000 and typical batches fit that too.
const DOCUMENTED_CU_LIMIT: u64 = 400_000;

/// `find_program_address` costs ~1,500 CU per bump tried, and a trader chooses their
/// own wallet address, so a trader can grind one whose ATAs need many tries. Find
/// wallets whose USDC AND USDT ATAs both need at least 7 tries (bump <= 249): about a
/// 1-in-4,000 draw each.
fn grind_wallet(e: &Env) -> Pubkey {
    loop {
        let w = Pubkey::new_unique();
        let bump = |mint: &Pubkey| {
            Pubkey::find_program_address(
                &[w.as_ref(), anchor_spl::token::spl_token::ID.as_ref(), mint.as_ref()],
                &core_vault::constants::ATA_PROGRAM_ID,
            )
            .1
        };
        if bump(&e.usdc) <= 249 && bump(&e.usdt) <= 249 {
            return w;
        }
    }
}

struct Measured {
    size: usize,
    cu: u64,
}

/// MAX claims for wallets from `pick`; one settle call with a ComputeBudget
/// instruction, a distinct fee payer and caller (two signatures: the largest
/// header), pools large enough that every claim is paid in full (both pools used).
fn full_batch(pick: &dyn Fn(&Env) -> Pubkey) -> Measured {
    let (mut e, s) = Env::registered(&Cfg { max_payout: 9, ..Cfg::default() });
    let mut ts = vec![];
    for _ in 0..MAX {
        let w = pick(&e);
        e.deposit(&s, &w, 1);
        e.payout(&s, &w, 1, 1_000_000, 1);
        e.make_atas(&w);
        ts.push(triple(&e, &s, &w, 1, 1));
    }
    // each claim is paid from both pools: USDC 700k, USDT 300k
    e.set_pool(Coin::Usdc, MAX as u64 * 700_000);
    e.set_pool(Coin::Usdt, MAX as u64 * 300_000 + 10);
    e.begin();
    let caller = e.new_caller();
    let ixs = [compute_limit_ix(DOCUMENTED_CU_LIMIT as u32), settle_ix(&caller.pubkey(), &e, &ts)];
    let fee_payer = dup(&e.payer);
    let size = {
        // two required signatures: fee payer and caller
        let msg = solana_message::Message::new(&ixs, Some(&fee_payer.pubkey()));
        assert_eq!(msg.header.num_required_signatures, 2);
        1 + 64 * 2 + msg.serialize().len()
    };
    let m = assert_ok(e.send_with(&ixs, &fee_payer, &[&caller]));
    assert_eq!(e.vault_state().open_claims_count, 0, "every claim in the batch was paid and closed");
    e.assert_claim_invariant();
    Measured { size, cu: m.compute_units_consumed }
}

#[test]
fn a_full_batch_fits_one_legacy_transaction_and_the_compute_budget() {
    let typical = full_batch(&|_| Pubkey::new_unique());
    let worst = full_batch(&|e| grind_wallet(e));
    println!("MAX_SETTLE_BATCH = {MAX}");
    println!("typical wallets : {} bytes, {} CU", typical.size, typical.cu);
    println!("ground wallets  : {} bytes, {} CU", worst.size, worst.cu);

    for m in [&typical, &worst] {
        // distinct keys for every account is the largest possible message; two signatures
        assert!(m.size <= PACKET, "{} bytes does not fit a packet", m.size);
        assert!(m.size + 100 <= PACKET, "keep >= 100 bytes of headroom for extra instructions");
        assert!(m.cu <= DOCUMENTED_CU_LIMIT, "{} CU exceeds the documented limit", m.cu);
    }
    // the default per-instruction limit covers typical wallets, with room to spare
    assert!(typical.cu <= 200_000 * 3 / 4, "typical full batch: {} CU", typical.cu);
}

#[test]
fn one_more_than_the_maximum_would_not_be_safe_to_allow() {
    // The program refuses MAX + 1 outright (see settle_claims.rs). This documents
    // why the bound exists: with a distinct key per account, the packet limit caps
    // the batch near 8 claims, and CU grows with each claim.
    let (mut e, s) = Env::registered(&Cfg { max_payout: 9, ..Cfg::default() });
    let mut ts = vec![];
    for _ in 0..MAX + 1 {
        let w = Pubkey::new_unique();
        e.deposit(&s, &w, 1);
        e.payout(&s, &w, 1, 10, 1);
        e.make_atas(&w);
        ts.push(triple(&e, &s, &w, 1, 1));
    }
    e.set_pool(Coin::Usdc, 10_000);
    e.begin();
    let ix = settle_ix(&e.payer.pubkey(), &e, &ts);
    assert_vault_err(&e.send(ix), core_vault::errors::VaultError::BatchTooLarge);
}

#[test]
fn a_batch_of_one_ground_wallet_is_cheap_enough_to_settle_alone() {
    // If a trader grinds a wallet whose ATAs are expensive to derive, that claim can
    // still be settled on its own (and with a raised compute limit if ever needed).
    let (mut e, s) = Env::registered(&Cfg::default());
    let w = grind_wallet(&e);
    e.deposit(&s, &w, 1);
    e.payout(&s, &w, 1, 500, 1);
    e.make_atas(&w);
    e.set_pool(Coin::Usdc, 10_000);
    e.begin();
    let t = triple(&e, &s, &w, 1, 1);
    let m = e.settle(&[t]);
    println!("one ground claim: {} CU", m.compute_units_consumed);
    assert!(m.compute_units_consumed < 100_000);
}

// ------------------------------------------------------------ bond claims (kind 1)

/// Bond claims are no bigger on the wire than trader claims, and re-deriving a
/// kind-1 address costs the same `create_program_address`. Measured for a full
/// batch of ground wallets: six bond claims, then three bond + three trader claims.
fn bond_batch(bonds: usize) -> Measured {
    use core_vault::state::BondTerm;
    let (mut e, s) = Env::registered(&Cfg { max_payout: 9, ..Cfg::default() });
    let mut ts = vec![];
    for i in 0..MAX {
        let w = grind_wallet(&e);
        e.fund(&w);
        e.fund_wallet(&w);
        let fee_payer = dup(&e.payer);
        if i < bonds {
            let ix = deposit_bond_ix(&e, &w, 0, 100_000_000, BondTerm::SixMonths, Coin::Usdc);
            assert_ok(e.send_with(&[ix], &fee_payer, &[]));
        } else {
            e.deposit(&s, &w, 1);
            e.payout(&s, &w, 1, 100_000_000, 1);
        }
        e.make_atas(&w);
        ts.push((w, i < bonds));
    }
    e.advance(core_vault::constants::BOND_6M_LOCK_SECS);
    let mut triples = vec![];
    for (w, is_bond) in &ts {
        if *is_bond {
            let fee_payer = dup(&e.payer);
            let ix = request_bond_payout_ix(&e, w, 0);
            assert_ok(e.send_with(&[ix], &fee_payer, &[]));
            triples.push((bond_claim_pda(w, 0).0, ata(w, &e.usdc), ata(w, &e.usdt)));
        } else {
            triples.push(triple(&e, &s, w, 1, 1));
        }
    }
    // large pools: every claim paid in full from both pools
    e.set_pool(Coin::Usdc, MAX as u64 * 70_000_000);
    e.set_pool(Coin::Usdt, MAX as u64 * 40_000_000);
    e.begin();
    let caller = e.new_caller();
    let ixs = [compute_limit_ix(DOCUMENTED_CU_LIMIT as u32), settle_ix(&caller.pubkey(), &e, &triples)];
    let fee_payer = dup(&e.payer);
    let size = {
        let msg = solana_message::Message::new(&ixs, Some(&fee_payer.pubkey()));
        1 + 64 * msg.header.num_required_signatures as usize + msg.serialize().len()
    };
    let m = assert_ok(e.send_with(&ixs, &fee_payer, &[&caller]));
    assert_eq!(e.vault_state().open_claims_count, 0, "every claim was paid and closed");
    Measured { size, cu: m.compute_units_consumed }
}

#[test]
fn a_full_batch_with_bond_claims_still_fits() {
    let all_bonds = bond_batch(MAX);
    let mixed = bond_batch(MAX / 2);
    println!("six bond claims (ground wallets): {} bytes, {} CU", all_bonds.size, all_bonds.cu);
    println!("three bond + three trader claims : {} bytes, {} CU", mixed.size, mixed.cu);
    for m in [&all_bonds, &mixed] {
        assert!(m.size + 100 <= PACKET, "{} bytes", m.size);
        assert!(m.cu <= DOCUMENTED_CU_LIMIT, "{} CU", m.cu);
    }
}
