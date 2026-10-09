//! Tamper tests: every way a hostile preparer or a bit-flip could try to get a signer to
//! authorise something other than what they read. Each test names the single defence it
//! exercises, builds the hostile transaction with CONSISTENT metadata (so only that defence
//! can catch it), and checks that `inspect` flags it and `sign` refuses without prompting.
#![cfg(feature = "localnet")]
mod common;

use anchor_lang::prelude::Pubkey;
use anchor_lang::solana_program::instruction::{AccountMeta, Instruction};
use anchor_lang::solana_program::system_instruction;
use base64::Engine;
use common::*;
use setl8_admin::admin_ix::{AdminIx, Side};
use setl8_admin::cluster::label_for_genesis;
use setl8_admin::constants::{ata_address, MEMO_PROGRAM_ID, SPL_TOKEN_ID};
use setl8_admin::message::{build_instructions, NonceUse, Parts};
use setl8_admin::txfile::{sha256_hex, TxFile};
use sha2::Digest;
use solana_hash::Hash;
use solana_keypair::Keypair;
use solana_message::Message;
use solana_signer::Signer;

/// A valid withdraw plan on disk as `tx.json` (recent blockhash).
fn planned(w: &World) {
    w.fill_pool(&w.usdc, 1_000 * M);
    let (c, h) = w.plan("admin-withdraw", "tx.json", &["--pool", "usdc", "--amount", "100", "--recent-blockhash"]);
    assert_eq!(c, 0, "{}{}", h.out, h.err);
}

fn parts_of(w: &World, admin: AdminIx) -> Parts {
    Parts {
        fee_payer: w.keys.sl8,
        blockhash: w.rpc.svm.borrow().latest_blockhash(),
        nonce: None,
        cu_limit: None,
        cu_price: None,
        genesis: LOCAL_GENESIS.to_string(),
        admin,
    }
}

fn good_withdraw(w: &World) -> AdminIx {
    AdminIx::Withdraw { pool: Side::Usdc, amount: 100 * M, mint: w.usdc }
}

/// Writes a transaction file for `msg` whose metadata is internally consistent with it.
fn forged(w: &World, name: &str, ixs: &[Instruction], fee_payer: &Pubkey) -> TxFile {
    let bh = w.rpc.svm.borrow().latest_blockhash();
    let msg = Message::new_with_blockhash(ixs, Some(fee_payer), &bh);
    let signers: Vec<Pubkey> =
        msg.account_keys.iter().take(msg.header.num_required_signatures as usize).copied().collect();
    let f = TxFile::new(&msg.serialize(), "localnet", LOCAL_GENESIS, "forged", &signers, None);
    f.save(&w.path(name)).unwrap();
    f
}

/// Asserts: inspect exits 2 and says `needle`; sign refuses with 2, never prompts, adds nothing.
fn assert_unsignable(w: &World, name: &str, needle: &str, extra: &[&str]) {
    let (c, h) = w.inspect(name, extra);
    assert_eq!(c, 2, "inspect should flag it:\n{}", h.out);
    assert!(h.out.contains("DO NOT SIGN"), "{}", h.out);
    assert!(h.out.contains(needle), "expected '{needle}' in:\n{}", h.out);
    let before = std::fs::read_to_string(w.path(name)).unwrap();
    let want = sha256_hex(&TxFile::load(&w.path(name)).unwrap().message_bytes().unwrap())[..8].to_string();
    let (c, h) = w.sign_with_answer(name, &w.sl8_key, &want, extra);
    assert_eq!(c, 2, "sign must refuse:\n{}{}", h.out, h.err);
    assert!(h.prompts.is_empty(), "sign must refuse BEFORE asking for confirmation");
    assert_eq!(std::fs::read_to_string(w.path(name)).unwrap(), before, "sign must not touch the file");
}

fn rewrite_message(w: &World, name: &str, f: impl FnOnce(&mut Vec<u8>), fix_metadata: bool) {
    let mut file = TxFile::load(&w.path(name)).unwrap();
    let mut bytes = file.message_bytes().unwrap();
    f(&mut bytes);
    file.message_b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    if fix_metadata {
        file.message_sha256 = sha256_hex(&bytes);
    }
    file.save(&w.path(name)).unwrap();
}

// ------------------------------------------------------------------ message changed after signing

#[test]
fn a_flipped_byte_after_the_first_signature_stops_the_second_signer() {
    let w = World::with_vault();
    planned(&w);
    let (c, _) = w.sign("tx.json", &w.sl8_key, &[]);
    assert_eq!(c, 0);
    // flip the lowest bit of the withdrawal amount (the last byte of the message is the last data byte)
    rewrite_message(&w, "tx.json", |b| *b.last_mut().unwrap() ^= 1, true);
    let (c, h) = w.inspect("tx.json", &[]);
    assert_eq!(c, 2, "{}", h.out);
    assert!(
        h.out.contains("does not verify against this message"),
        "the first signature must no longer verify:\n{}",
        h.out
    );
    assert!(h.out.contains("present but INVALID"), "{}", h.out);
    let want = w.tx_file("tx.json").message_sha256[..8].to_string();
    let (c, h) = w.sign_with_answer("tx.json", &w.rov_key, &want, &[]);
    assert_eq!(c, 2, "{}{}", h.out, h.err);
    assert!(h.prompts.is_empty());
    assert_eq!(w.tx_file("tx.json").signatures.len(), 1, "the second signer signed nothing");
}

#[test]
fn a_flipped_byte_with_stale_metadata_is_a_loud_mismatch() {
    let w = World::with_vault();
    planned(&w);
    rewrite_message(&w, "tx.json", |b| *b.last_mut().unwrap() ^= 1, false);
    let (c, h) = w.inspect("tx.json", &[]);
    assert_eq!(c, 2);
    assert!(h.out.contains("THE FILE'S METADATA DISAGREES WITH THE MESSAGE BYTES"), "{}", h.out);
    assert!(h.out.contains("MISMATCH: file says the message hash is"), "{}", h.out);
    assert!(h.err.contains("metadata disagrees"), "{}", h.err);
    // the description said 100 USDC; the bytes now say a different amount
    assert!(h.out.contains("the message actually does"), "{}", h.out);
}

// ------------------------------------------------------------------ metadata lies

#[test]
fn metadata_that_lies_about_the_description_cluster_signers_or_nonce_is_caught_from_the_bytes() {
    let w = World::with_vault();
    planned(&w);
    type Lie = Box<dyn Fn(&mut TxFile)>;
    let lies: Vec<(&str, Lie, &str)> = vec![
        (
            "description",
            Box::new(|f| f.description = "withdraw 1.000000 USDC from the USDC pool".into()),
            "the message actually does",
        ),
        (
            "cluster",
            Box::new(|f| {
                f.cluster = "devnet".into();
                f.genesis_hash = setl8_admin::constants::DEVNET_GENESIS.into();
            }),
            "the message is bound to",
        ),
        ("hash", Box::new(|f| f.message_sha256 = "00".repeat(32)), "the message bytes hash to"),
        ("signers", Box::new(|f| f.required_signers.reverse()), "the message requires"),
        ("nonce", Box::new(|f| f.nonce_account = Some(Pubkey::new_unique().to_string())), "the message uses"),
        ("genesis", Box::new(|f| f.genesis_hash = "SomethingElse".into()), "the message is bound to"),
        ("format", Box::new(|f| f.format = "other/9".into()), "this tool reads"),
    ];
    for (what, lie, needle) in lies {
        let mut f = w.tx_file("tx.json");
        lie(&mut f);
        f.save(&w.path("lie.json")).unwrap();
        let (c, h) = w.inspect("lie.json", &[]);
        assert_eq!(c, 2, "{what} lie not caught:\n{}", h.out);
        assert!(h.out.contains("MISMATCH") && h.out.contains(needle), "{what}: {}", h.out);
        let (c, h) = w.sign_with_answer("lie.json", &w.sl8_key, &w.hash_of("tx.json")[..8], &[]);
        assert_eq!(c, 2, "{what}");
        assert!(h.prompts.is_empty(), "{what}");
    }
}

// ------------------------------------------------------------------ allowlist

#[test]
fn an_extra_system_transfer_is_flagged_and_named() {
    let w = World::with_vault();
    let p = parts_of(&w, good_withdraw(&w));
    let mut ixs = build_instructions(&w.keys, &p);
    ixs.push(system_instruction::transfer(&w.keys.sl8, &Pubkey::new_unique(), 5_000_000_000));
    forged(&w, "x.json", &ixs, &w.keys.sl8);
    assert_unsignable(
        &w,
        "x.json",
        "is `System Program Transfer of 5000000000 lamports`: only AdvanceNonceAccount of the declared nonce is allowed",
        &[],
    );
}

#[test]
fn an_extra_instruction_of_an_unknown_program_is_flagged() {
    let w = World::with_vault();
    let p = parts_of(&w, good_withdraw(&w));
    let mut ixs = build_instructions(&w.keys, &p);
    let evil = Pubkey::new_unique();
    ixs.push(Instruction { program_id: evil, accounts: vec![AccountMeta::new(w.keys.sl8, true)], data: vec![1, 2, 3] });
    forged(&w, "x.json", &ixs, &w.keys.sl8);
    assert_unsignable(&w, "x.json", &format!("calls program {evil}"), &[]);
}

#[test]
fn a_second_admin_instruction_is_flagged() {
    let w = World::with_vault();
    let p = parts_of(&w, good_withdraw(&w));
    let mut ixs = build_instructions(&w.keys, &p);
    ixs.push(good_withdraw(&w).build(&w.keys));
    forged(&w, "x.json", &ixs, &w.keys.sl8);
    assert_unsignable(&w, "x.json", "exactly ONE instruction to the vault program, it has 2", &[]);
}

#[test]
fn an_spl_token_transfer_is_flagged() {
    let w = World::with_vault();
    let p = parts_of(&w, good_withdraw(&w));
    let mut ixs = build_instructions(&w.keys, &p);
    ixs.push(Instruction {
        program_id: SPL_TOKEN_ID,
        accounts: vec![
            AccountMeta::new(Pubkey::new_unique(), false),
            AccountMeta::new(Pubkey::new_unique(), false),
            AccountMeta::new_readonly(w.keys.sl8, true),
        ],
        data: vec![3, 1, 0, 0, 0, 0, 0, 0, 0],
    });
    forged(&w, "x.json", &ixs, &w.keys.sl8);
    assert_unsignable(&w, "x.json", "not the vault, the System Program (nonce), ComputeBudget or Memo", &[]);
}

#[test]
fn a_different_program_id_in_place_of_the_vault_is_flagged() {
    let w = World::with_vault();
    let p = parts_of(&w, good_withdraw(&w));
    let mut ixs = build_instructions(&w.keys, &p);
    let last = ixs.len() - 1;
    let imposter = Pubkey::new_unique();
    ixs[last].program_id = imposter;
    forged(&w, "x.json", &ixs, &w.keys.sl8);
    assert_unsignable(&w, "x.json", &format!("calls program {imposter}"), &[]);
    let (_, h) = w.inspect("x.json", &[]);
    assert!(h.out.contains("exactly ONE instruction to the vault program, it has 0"), "{}", h.out);
}

#[test]
fn the_program_id_override_must_be_stated_by_the_signer() {
    let w = World::with_vault();
    let other = Pubkey::new_unique();
    let k = w.keys.with_program(other);
    let p = parts_of(&w, good_withdraw(&w));
    let ixs = build_instructions(&k, &p);
    let mut f = forged(&w, "x.json", &ixs, &w.keys.sl8);
    f.description = setl8_admin::inspect::description_text(&p.admin);
    f.save(&w.path("x.json")).unwrap();
    // without --program-id the signer's build says the vault is somewhere else
    assert_unsignable(&w, "x.json", "exactly ONE instruction to the vault program, it has 0", &[]);
    // the signer who knowingly overrides sees it flagged as an override
    let o = other.to_string();
    let (c, h) = w.inspect("x.json", &["--program-id", &o]);
    assert_eq!(c, 0, "{}", h.out);
    assert!(h.out.contains("OVERRIDDEN with --program-id"), "{}", h.out);
}

#[test]
fn an_unknown_vault_discriminator_is_flagged() {
    let w = World::with_vault();
    let p = parts_of(&w, good_withdraw(&w));
    let mut ixs = build_instructions(&w.keys, &p);
    let last = ixs.len() - 1;
    ixs[last].data[..8].copy_from_slice(&[9, 9, 9, 9, 9, 9, 9, 9]);
    forged(&w, "x.json", &ixs, &w.keys.sl8);
    assert_unsignable(&w, "x.json", "unknown vault instruction", &[]);
}

#[test]
fn another_vault_instruction_is_named_and_flagged() {
    let w = World::with_vault();
    let p = parts_of(&w, good_withdraw(&w));
    let mut ixs = build_instructions(&w.keys, &p);
    let last = ixs.len() - 1;
    ixs[last].data = sha2::Sha256::digest(b"global:begin_heartbeat")[..8].to_vec();
    ixs[last].accounts = vec![];
    forged(&w, "x.json", &ixs, &w.keys.sl8);
    assert_unsignable(
        &w,
        "x.json",
        "`begin_heartbeat` instruction, which is NOT one of the six admin instructions",
        &[],
    );
}

#[test]
fn a_wrong_vault_pda_is_flagged() {
    let w = World::with_vault();
    let p = parts_of(&w, good_withdraw(&w));
    let mut ixs = build_instructions(&w.keys, &p);
    let last = ixs.len() - 1;
    let fake_vault = Pubkey::new_unique();
    ixs[last].accounts[2].pubkey = fake_vault;
    forged(&w, "x.json", &ixs, &w.keys.sl8);
    assert_unsignable(&w, "x.json", "account #2 (vault_state)", &[]);
    let (_, h) = w.inspect("x.json", &[]);
    assert!(h.out.contains(&format!("expected {}", w.keys.vault())), "{}", h.out);
}

#[test]
fn a_wrong_pool_account_is_flagged() {
    let w = World::with_vault();
    let p = parts_of(&w, good_withdraw(&w));
    let mut ixs = build_instructions(&w.keys, &p);
    let last = ixs.len() - 1;
    ixs[last].accounts[4].pubkey = Pubkey::new_unique();
    forged(&w, "x.json", &ixs, &w.keys.sl8);
    assert_unsignable(&w, "x.json", "account #4 (pool_token_account)", &[]);
}

#[test]
fn a_withdraw_to_a_non_sl8_destination_is_flagged() {
    let w = World::with_vault();
    let p = parts_of(&w, good_withdraw(&w));
    let attacker = Pubkey::new_unique();
    let mut ixs = build_instructions(&w.keys, &p);
    let last = ixs.len() - 1;
    ixs[last].accounts[5].pubkey = ata_address(&attacker, &w.usdc);
    forged(&w, "x.json", &ixs, &w.keys.sl8);
    assert_unsignable(&w, "x.json", "account #5 (sl8_token_account)", &[]);
    let (_, h) = w.inspect("x.json", &[]);
    assert!(h.out.contains(&format!("expected {}", ata_address(&w.sl8.pubkey(), &w.usdc))), "{}", h.out);
}

#[test]
fn an_sl8_owned_account_that_is_not_the_ata_is_refused_even_though_the_program_would_accept_it() {
    let w = World::with_vault();
    let odd = Pubkey::new_unique();
    w.set_token(&odd, &w.usdc, &w.sl8.pubkey(), 0); // owned by SL8, right mint: the program accepts it
    let p = parts_of(&w, good_withdraw(&w));
    let mut ixs = build_instructions(&w.keys, &p);
    let last = ixs.len() - 1;
    ixs[last].accounts[5].pubkey = odd;
    forged(&w, "x.json", &ixs, &w.keys.sl8);
    assert_unsignable(&w, "x.json", "account #5 (sl8_token_account)", &[]);
}

#[test]
fn a_demoted_signer_flag_is_flagged() {
    let w = World::with_vault();
    let p = parts_of(&w, good_withdraw(&w));
    let mut ixs = build_instructions(&w.keys, &p);
    let last = ixs.len() - 1;
    ixs[last].accounts[1].is_signer = false; // ROV is no longer asked to sign
    forged(&w, "x.json", &ixs, &w.keys.sl8);
    assert_unsignable(&w, "x.json", "account #1 (rov_admin): must be a signer", &[]);
}

#[test]
fn a_memo_with_other_text_or_a_second_memo_is_flagged() {
    let w = World::with_vault();
    let p = parts_of(&w, good_withdraw(&w));
    let mut ixs = build_instructions(&w.keys, &p);
    ixs.insert(0, Instruction { program_id: MEMO_PROGRAM_ID, accounts: vec![], data: b"hello".to_vec() });
    forged(&w, "x.json", &ixs, &w.keys.sl8);
    assert_unsignable(&w, "x.json", "is a Memo that is not exactly", &[]);
    let mut ixs = build_instructions(&w.keys, &p);
    ixs.push(ixs[ixs.len() - 2].clone());
    forged(&w, "y.json", &ixs, &w.keys.sl8);
    assert_unsignable(&w, "y.json", "more than one memo", &[]);
}

#[test]
fn a_missing_genesis_memo_is_flagged() {
    let w = World::with_vault();
    let p = parts_of(&w, good_withdraw(&w));
    let mut ixs = build_instructions(&w.keys, &p);
    ixs.retain(|i| i.program_id != MEMO_PROGRAM_ID);
    forged(&w, "x.json", &ixs, &w.keys.sl8);
    assert_unsignable(&w, "x.json", "not bound to a cluster", &[]);
}

#[test]
fn reordered_instructions_are_not_canonical() {
    let w = World::with_vault();
    let p = parts_of(&w, good_withdraw(&w));
    let mut ixs = build_instructions(&w.keys, &p);
    let n = ixs.len();
    ixs.swap(n - 1, n - 2); // vault first, memo last
    forged(&w, "x.json", &ixs, &w.keys.sl8);
    assert_unsignable(&w, "x.json", "not byte-for-byte the canonical transaction", &[]);
}

#[test]
fn a_compute_budget_heap_request_is_flagged() {
    let w = World::with_vault();
    let p = parts_of(&w, good_withdraw(&w));
    let mut ixs = build_instructions(&w.keys, &p);
    ixs.insert(
        0,
        Instruction {
            program_id: setl8_admin::constants::COMPUTE_BUDGET_ID,
            accounts: vec![],
            data: vec![1, 0, 0, 4, 0],
        },
    );
    forged(&w, "x.json", &ixs, &w.keys.sl8);
    assert_unsignable(&w, "x.json", "other than SetComputeUnitLimit / SetComputeUnitPrice", &[]);
}

#[test]
fn a_nonce_advance_that_is_not_first_or_for_an_undeclared_nonce_is_flagged() {
    let w = World::with_vault();
    let nonce = Pubkey::new_unique();
    let mut p = parts_of(&w, good_withdraw(&w));
    p.nonce = Some(NonceUse { account: nonce, authority: w.keys.sl8 });
    let ixs = build_instructions(&w.keys, &p);
    forged(&w, "x.json", &ixs, &w.keys.sl8);
    // the signer declared a different nonce
    let other = Pubkey::new_unique().to_string();
    assert_unsignable(&w, "x.json", "was declared", &["--nonce-account", &other]);
    // advance placed after the memo
    let mut ixs = build_instructions(&w.keys, &p);
    ixs.swap(0, 1);
    forged(&w, "y.json", &ixs, &w.keys.sl8);
    assert_unsignable(&w, "y.json", "must be the FIRST instruction", &[]);
}

#[test]
fn a_declared_nonce_that_the_message_does_not_advance_is_flagged() {
    let w = World::with_vault();
    planned(&w); // recent blockhash, no nonce
    let declared = Pubkey::new_unique().to_string();
    assert_unsignable(&w, "tx.json", "does not advance it", &["--nonce-account", &declared]);
}

#[test]
fn a_nonce_authority_that_is_nobody_we_know_is_flagged() {
    let w = World::with_vault();
    let mut p = parts_of(&w, good_withdraw(&w));
    p.nonce = Some(NonceUse { account: Pubkey::new_unique(), authority: Pubkey::new_unique() });
    forged(&w, "x.json", &build_instructions(&w.keys, &p), &w.keys.sl8);
    assert_unsignable(&w, "x.json", "is neither an admin key nor the fee payer", &[]);
}

// ------------------------------------------------------------------ fee payer

#[test]
fn an_unexpected_fee_payer_is_flagged() {
    let w = World::with_vault();
    let stranger = Pubkey::new_unique();
    let mut p = parts_of(&w, good_withdraw(&w));
    p.fee_payer = stranger;
    forged(&w, "x.json", &build_instructions(&w.keys, &p), &stranger);
    assert_unsignable(&w, "x.json", &format!("fee payer is {stranger}"), &[]);
}

// ------------------------------------------------------------------ keys and signatures

#[test]
fn a_key_that_is_not_a_required_signer_cannot_sign() {
    let w = World::with_vault();
    planned(&w);
    let stranger = Keypair::new();
    let k = write_key(&w.dir, "stranger.json", &stranger);
    let before = std::fs::read_to_string(w.path("tx.json")).unwrap();
    let (c, h) = w.sign("tx.json", &k, &[]);
    assert_eq!(c, 2, "{}{}", h.out, h.err);
    assert!(h.err.contains("is not one of this transaction's required signers"), "{}", h.err);
    assert!(h.prompts.is_empty());
    assert_eq!(std::fs::read_to_string(w.path("tx.json")).unwrap(), before);
}

#[test]
fn signing_twice_is_refused_and_leaves_one_signature() {
    let w = World::with_vault();
    planned(&w);
    let (c, _) = w.sign("tx.json", &w.sl8_key, &[]);
    assert_eq!(c, 0);
    let (c, h) = w.sign("tx.json", &w.sl8_key, &[]);
    assert_eq!(c, 2, "{}{}", h.out, h.err);
    assert!(h.err.contains("has already signed"), "{}", h.err);
    assert_eq!(w.tx_file("tx.json").signatures.len(), 1);
}

fn sign_bytes(kp: &Keypair, bytes: &[u8]) -> String {
    kp.try_sign_message(bytes).unwrap().to_string()
}

#[test]
fn add_signature_accepts_a_good_one_and_rejects_every_bad_one() {
    let w = World::with_vault();
    planned(&w);
    let name = w.s(&w.path("tx.json"));
    let bytes = w.tx_file("tx.json").message_bytes().unwrap();
    let sl8 = w.sl8.pubkey().to_string();
    let add = |pk: &str, sig: &str| w.run(&[], &["add-signature", &name, "--pubkey", pk, "--signature", sig]);

    // a signature of a DIFFERENT message by the right key
    let (c, h) = add(&sl8, &sign_bytes(&w.sl8, b"some other message"));
    assert_eq!(c, 1, "{}{}", h.out, h.err);
    assert!(h.err.contains("does not verify"), "{}", h.err);
    // random garbage that parses as a signature
    let (c, _) = add(&sl8, &solana_signature::Signature::from([7u8; 64]).to_string());
    assert_eq!(c, 1);
    // right message, wrong key claimed
    let (c, h) = add(&w.rov.pubkey().to_string(), &sign_bytes(&w.sl8, &bytes));
    assert_eq!(c, 1);
    assert!(h.err.contains("does not verify"), "{}", h.err);
    // a valid signature by a key that is not a required signer
    let stranger = Keypair::new();
    let (c, h) = add(&stranger.pubkey().to_string(), &sign_bytes(&stranger, &bytes));
    assert_eq!(c, 1);
    assert!(h.err.contains("not a required signer"), "{}", h.err);
    // not base58 at all
    let (c, h) = add(&sl8, "not-a-signature");
    assert_eq!(c, 1);
    assert!(h.err.contains("not a valid base58 signature"), "{}", h.err);
    assert!(w.tx_file("tx.json").signatures.is_empty(), "no bad signature may be stored");

    // the genuine one, produced elsewhere
    let good = sign_bytes(&w.sl8, &bytes);
    let (c, h) = add(&sl8, &good);
    assert_eq!(c, 0, "{}{}", h.out, h.err);
    assert!(h.out.contains("1 of 2 required signatures"), "{}", h.out);
    // adding the identical one again changes nothing (duplicate signature)
    let (c, h) = add(&sl8, &good);
    assert_eq!(c, 0);
    assert!(h.out.contains("nothing changed"), "{}", h.out);
    assert_eq!(w.tx_file("tx.json").signatures.len(), 1);
}

#[test]
fn duplicate_signature_entries_do_not_count_twice() {
    let w = World::with_vault();
    planned(&w);
    let (c, _) = w.sign("tx.json", &w.sl8_key, &[]);
    assert_eq!(c, 0);
    let mut f = w.tx_file("tx.json");
    let dup = f.signatures[0].clone();
    f.signatures.push(dup); // two entries, one signer: 2 == number of required signers
    f.save(&w.path("tx.json")).unwrap();
    let (c, h) = w.inspect("tx.json", &[]);
    assert_eq!(c, 2, "{}", h.out);
    assert!(h.out.contains("appears 2 times"), "{}", h.out);
    assert!(h.out.contains("MISSING"), "ROV is still missing:\n{}", h.out);
    let (c, h) = w.send("tx.json", &[]);
    assert_eq!(c, 2, "{}{}", h.out, h.err);
    assert_eq!(w.rpc.sends.get(), 0);
}

#[test]
fn a_signature_entry_for_a_stranger_is_flagged() {
    let w = World::with_vault();
    planned(&w);
    let mut f = w.tx_file("tx.json");
    f.signatures.push(setl8_admin::txfile::SigEntry {
        pubkey: Pubkey::new_unique().to_string(),
        signature: solana_signature::Signature::from([7u8; 64]).to_string(),
    });
    f.save(&w.path("tx.json")).unwrap();
    let (c, h) = w.inspect("tx.json", &[]);
    assert_eq!(c, 2);
    assert!(h.out.contains("not a required signer"), "{}", h.out);
}

#[test]
fn a_corrupt_signature_string_counts_as_invalid_not_as_missing() {
    let w = World::with_vault();
    planned(&w);
    let mut f = w.tx_file("tx.json");
    f.signatures
        .push(setl8_admin::txfile::SigEntry { pubkey: w.sl8.pubkey().to_string(), signature: "garbage".into() });
    f.save(&w.path("tx.json")).unwrap();
    let (c, h) = w.inspect("tx.json", &[]);
    assert_eq!(c, 2);
    assert!(h.out.contains("present but INVALID"), "{}", h.out);
}

// ------------------------------------------------------------------ garbage in

#[test]
fn garbage_files_are_errors_not_panics() {
    let w = World::with_vault();
    planned(&w);
    let name = |n: &str| w.s(&w.path(n));
    // not json
    write_file(&w.dir, "junk.json", "not json at all");
    let (c, h) = w.run(&[], &["inspect", &name("junk.json")]);
    assert_eq!(c, 1);
    assert!(h.err.contains("is not a setl8-admin transaction file"), "{}", h.err);
    // missing file
    let (c, h) = w.run(&[], &["inspect", &name("nope.json")]);
    assert_eq!(c, 1);
    assert!(h.err.contains("cannot read"), "{}", h.err);
    // bad base64
    let mut f = w.tx_file("tx.json");
    f.message_b64 = "!!!not base64!!!".into();
    f.save(&w.path("b64.json")).unwrap();
    let (c, h) = w.run(&[], &["inspect", &name("b64.json")]);
    assert_eq!(c, 1);
    assert!(h.err.contains("not valid base64"), "{}", h.err);
    // a versioned message prefix / truncated / trailing bytes
    for (n, edit) in [
        ("v0.json", Box::new(|b: &mut Vec<u8>| b[0] = 0x80) as Box<dyn FnOnce(&mut Vec<u8>)>),
        ("trunc.json", Box::new(|b: &mut Vec<u8>| b.truncate(20))),
        ("trail.json", Box::new(|b: &mut Vec<u8>| b.push(7))),
        ("empty.json", Box::new(|b: &mut Vec<u8>| b.clear())),
    ] {
        let mut f = w.tx_file("tx.json");
        let mut bytes = f.message_bytes().unwrap();
        edit(&mut bytes);
        f.message_b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        f.save(&w.path(n)).unwrap();
        let (c, h) = w.run(&[], &["inspect", &name(n)]);
        assert_eq!(c, 1, "{n}: {}{}", h.out, h.err);
        assert!(h.err.starts_with("error:"), "{n}: {}", h.err);
    }
    // an unknown field in the file
    let mut v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(w.path("tx.json")).unwrap()).unwrap();
    v["extra"] = serde_json::json!(1);
    write_file(&w.dir, "extra.json", &v.to_string());
    let (c, _) = w.run(&[], &["inspect", &name("extra.json")]);
    assert_eq!(c, 1);
    let _ = (label_for_genesis, Hash::default());
}
