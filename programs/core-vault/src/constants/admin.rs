//! The two admin keys behind the 2-of-2 multisig that guards every admin instruction.

use anchor_lang::prelude::*;

/// SL8's half of the 2-of-2 admin multisig required on every privileged
/// vault instruction (`register_product`, `reactivate_product`,
/// `update_product_config`).
///
/// PLACEHOLDER — this is a freshly generated throwaway keypair
/// (`/tmp/setl8-vault-keys/sl8-admin.json` at scaffold time, also copied to
/// `tests/fixtures/sl8-admin.json` so the Module 1 tests can sign with it).
/// It does not correspond to any real, funded, or otherwise significant
/// wallet. Replace with the real SL8 protocol admin wallet pubkey before any
/// non-local deploy.
pub const SL8_ADMIN_PUBKEY: Pubkey = pubkey!("9SWJUtUBp1AyqfmAAFffbRfb4Qnr2u6Jqek8vHZMkFKP");

/// Rov's half of the 2-of-2 admin multisig. Never a destination for funds on
/// any instruction — its role is strictly the second required signature.
///
/// PLACEHOLDER — see `SL8_ADMIN_PUBKEY` above; same caveat applies
/// (`tests/fixtures/rov-admin.json`). Replace before any non-local deploy.
pub const ROV_ADMIN_PUBKEY: Pubkey = pubkey!("D1EuhXLMWzz9Ypy9gkkwjpzVhEXi4RoDc6NQZv9og1m6");
