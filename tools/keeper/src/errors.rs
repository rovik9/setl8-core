//! Mapping of the vault's custom error codes to names, and what a keeper does about each.

use core_vault::errors::VaultError;

macro_rules! table {
    ($($v:ident),* $(,)?) => {
        &[ $( ((VaultError::$v as u32) + 6000, stringify!($v)) ),* ]
    };
}

/// Every `VaultError`, in declaration order, with its on-chain code (6000 + index).
pub const VAULT_ERRORS: &[(u32, &str)] = table![
    Unauthorized,
    ProductNotActive,
    MissingMultisigSignature,
    ProductAlreadyRegistered,
    TooManyChallengeSizes,
    InvalidTraderStatus,
    InvalidChallengeTier,
    WrongAmount,
    PayoutCapReached,
    RequestIdMismatch,
    NotAbandonable,
    ResetNotAllowed,
    InvalidResetPhase,
    TooManyResetPhases,
    ProductAlreadyPaused,
    ZeroAmount,
    MathOverflow,
    InvalidMint,
    DuplicateMint,
    InvalidDecimals,
    WrongTokenProgram,
    InvalidTokenAccount,
    TraderWalletMismatch,
    InsufficientTokenBalance,
    InvalidFeeSplit,
    CycleInProgress,
    NoCycleInProgress,
    HeartbeatTooEarly,
    CycleIncomplete,
    ClaimNotEligible,
    ClaimAlreadySettled,
    InvalidClaim,
    BatchTooLarge,
    EmptyBatch,
    InvalidTally,
    BondTermInvalid,
    BondBelowMinimum,
    BondWalletCapExceeded,
    BondGlobalCapExceeded,
    BondLocked,
    InvalidBondPosition,
    BondIndexMismatch,
    WithdrawalExceedsReserve,
    ClaimsCeilingExceeded,
];

/// `6030` to `ClaimAlreadySettled`; `None` for a code that is not the vault's.
pub fn vault_error_name(code: u32) -> Option<&'static str> {
    VAULT_ERRORS.iter().find(|(c, _)| *c == code).map(|(_, n)| *n)
}

/// A readable name for any custom code (the vault's name, or the raw number).
pub fn describe_code(code: u32) -> String {
    match vault_error_name(code) {
        Some(n) => format!("{n} (Custom {code})"),
        None => format!("Custom {code}"),
    }
}

/// What a failed send means.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    /// Another keeper (or a person) got there first, or the state moved: SR-06. Not a failure: re-read the chain.
    Race,
    /// A wrong address or a malformed input: log loudly and do NOT retry.
    Loud,
    /// Anything else: log it, do not retry in this pass.
    Other,
}

pub fn classify(code: u32) -> Class {
    match vault_error_name(code) {
        Some(
            "ClaimAlreadySettled"
            | "ClaimNotEligible"
            | "CycleInProgress"
            | "NoCycleInProgress"
            | "HeartbeatTooEarly"
            | "CycleIncomplete"
            | "ProductAlreadyPaused",
        ) => Class::Race,
        Some("InvalidTally" | "InvalidTokenAccount" | "InvalidClaim" | "InvalidMint") => Class::Loud,
        _ => Class::Other,
    }
}

/// Errors that identify ONE claim as the problem when a batch fails (so the batch is split).
pub fn is_per_claim(code: u32) -> bool {
    matches!(
        vault_error_name(code),
        Some("ClaimAlreadySettled" | "ClaimNotEligible" | "InvalidClaim" | "InvalidTokenAccount" | "MathOverflow")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_matches_the_real_enum() {
        assert_eq!(VAULT_ERRORS.len(), 44);
        assert_eq!(VAULT_ERRORS.first().unwrap().0, 6000);
        assert_eq!(VAULT_ERRORS.last().unwrap(), &(6043, "ClaimsCeilingExceeded"));
        for (i, (c, _)) in VAULT_ERRORS.iter().enumerate() {
            assert_eq!(*c as usize, 6000 + i, "codes are consecutive");
        }
        // the table agrees with the program's own conversion
        assert_eq!(u32::from(VaultError::ClaimAlreadySettled), 6030);
        assert_eq!(u32::from(VaultError::BondLocked), VAULT_ERRORS.iter().find(|x| x.1 == "BondLocked").unwrap().0);
    }

    #[test]
    fn names_and_classes() {
        assert_eq!(vault_error_name(6030), Some("ClaimAlreadySettled"));
        assert_eq!(vault_error_name(6029), Some("ClaimNotEligible"));
        assert_eq!(vault_error_name(6034), Some("InvalidTally"));
        assert_eq!(vault_error_name(17), None);
        assert_eq!(describe_code(6027), "HeartbeatTooEarly (Custom 6027)");
        assert_eq!(describe_code(17), "Custom 17");
        for race in [6030, 6029, 6025, 6026, 6027, 6028, 6014] {
            assert_eq!(classify(race), Class::Race, "{race}");
        }
        for loud in [6034, 6021, 6031, 6017] {
            assert_eq!(classify(loud), Class::Loud, "{loud}");
        }
        assert_eq!(classify(6016), Class::Other);
        assert_eq!(classify(17), Class::Other);
        assert!(is_per_claim(6030) && is_per_claim(6029) && is_per_claim(6031) && is_per_claim(6021));
        assert!(!is_per_claim(6025) && !is_per_claim(6034));
    }
}
