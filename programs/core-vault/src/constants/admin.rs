//! The two admin keys behind the 2-of-2 multisig that guards every admin instruction.
//!
//! Which keys are compiled in is decided by the `localnet` cargo feature:
//!
//! | build                         | keys                                   |
//! |-------------------------------|----------------------------------------|
//! | default (`anchor build`)      | the REAL founder-held admin pubkeys    |
//! | `--features localnet`         | PUBLIC TEST keys (private halves are committed in `tests/fixtures/`) |
//!
//! The default is the real-key build on purpose: a test run that forgets the
//! feature fails loudly (the tests-rs harness asserts fixture/const equality)
//! instead of a deployable build silently carrying keys anyone can sign with.
//! Before any deploy run `scripts/verify-deploy-build.sh`.
//!
//! Both keys are part of the `VaultState` PDA seeds, so the real build and the
//! localnet build derive DIFFERENT vault PDAs. Nothing may hardcode a vault PDA.
//!
//! **RESOLVED (revenue destination), by founder decision:** `init_vault` sets
//! `VaultState::sl8_wallet = SL8_ADMIN_PUBKEY`, so SL8's share of every fee lands
//! in token accounts owned by the SL8 admin key itself, and the revenue address
//! stays that key (SR-18, accepted). There is no treasury argument and no
//! `set_treasury` instruction.
//!
//! **Consequence:** whoever holds that one key holds the revenue (the fee
//! remainder and all bond fees) AND, through SR-01, half of every bond (SL8's
//! half of each principal lands in its token accounts, and a bond withdrawal is
//! paid back in full from the pool). Protect it accordingly: hardware wallet,
//! tested backup, routine sweeps to cold storage. The SL8 admin key's USDC and
//! USDT token accounts must exist BEFORE any fee arrives, or `deposit_fee`,
//! `deposit_reset` and `deposit_bond` fail (docs/DEPLOY-CHECKLIST.md).

use anchor_lang::prelude::*;

/// SL8's half of the 2-of-2 admin multisig required on every privileged vault
/// instruction (`init_vault`, `register_product`, `update_product_config`,
/// `pause_product`, `reactivate_product`, `admin_withdraw_marketing_funds`).
///
/// REAL key: founder-held, hardware/phone wallet. Must never be a funds
/// destination except SL8 as `sl8_wallet` (see the resolved decision and its
/// consequence in the module docs).
#[cfg(not(feature = "localnet"))]
pub const SL8_ADMIN_PUBKEY: Pubkey = pubkey!("SL89fcsKAuWYtkEJah86WLLBxCiHd1DeNczpsDjSHDJ");

/// SL8's half of the 2-of-2 admin multisig.
///
/// PUBLIC TEST KEY - the private key is committed in
/// `tests/fixtures/sl8-admin.json`; `localnet` feature only.
#[cfg(feature = "localnet")]
pub const SL8_ADMIN_PUBKEY: Pubkey = pubkey!("9SWJUtUBp1AyqfmAAFffbRfb4Qnr2u6Jqek8vHZMkFKP");

/// Rov's half of the 2-of-2 admin multisig. Never a destination for funds on
/// any instruction; its role is strictly the second required signature.
///
/// REAL key: founder-held, hardware/phone wallet. Must never be a funds
/// destination (SL8's revenue goes to `sl8_wallet`, not here).
#[cfg(not(feature = "localnet"))]
pub const ROV_ADMIN_PUBKEY: Pubkey = pubkey!("RovSQZxURnNgNkf7WkzRJTnhSBuN6tdfG2CQxp3rzKZ");

/// Rov's half of the 2-of-2 admin multisig.
///
/// PUBLIC TEST KEY - the private key is committed in
/// `tests/fixtures/rov-admin.json`; `localnet` feature only.
#[cfg(feature = "localnet")]
pub const ROV_ADMIN_PUBKEY: Pubkey = pubkey!("D1EuhXLMWzz9Ypy9gkkwjpzVhEXi4RoDc6NQZv9og1m6");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_two_admin_keys_differ() {
        assert_ne!(SL8_ADMIN_PUBKEY, ROV_ADMIN_PUBKEY);
    }

    /// Pins the exact real addresses so an accidental edit cannot slip through.
    #[cfg(not(feature = "localnet"))]
    #[test]
    fn default_build_has_the_real_admin_keys() {
        assert_eq!(SL8_ADMIN_PUBKEY.to_string(), "SL89fcsKAuWYtkEJah86WLLBxCiHd1DeNczpsDjSHDJ");
        assert_eq!(ROV_ADMIN_PUBKEY.to_string(), "RovSQZxURnNgNkf7WkzRJTnhSBuN6tdfG2CQxp3rzKZ");
    }

    /// The localnet build must carry the test keys (and so none of the real ones).
    #[cfg(feature = "localnet")]
    #[test]
    fn localnet_build_has_the_test_admin_keys() {
        assert_eq!(SL8_ADMIN_PUBKEY.to_string(), "9SWJUtUBp1AyqfmAAFffbRfb4Qnr2u6Jqek8vHZMkFKP");
        assert_eq!(ROV_ADMIN_PUBKEY.to_string(), "D1EuhXLMWzz9Ypy9gkkwjpzVhEXi4RoDc6NQZv9og1m6");
    }
}
