//! Runtime knobs.

use std::path::PathBuf;

use crate::alerts::DEFAULT_MIN_PAYER_BALANCE;

#[derive(Clone, Debug)]
pub struct Config {
    /// Print what would be sent; send nothing.
    pub dry_run: bool,
    /// Hard cap on transactions submitted in one process run (one `once`, or the whole life of `run`).
    pub max_sends: u32,
    /// Hard cap on the estimated fees paid in one process run, in lamports.
    pub max_fee_lamports: u64,
    /// Priority fee in micro-lamports per compute unit (0 = none, and then no ComputeBudget instruction is sent).
    pub priority_fee_micro: u64,
    /// Extra attempts after a transient failure (rate limit, network), with backoff.
    pub max_retries: u32,
    pub confirm_timeout_secs: u64,
    pub min_payer_balance: u64,
    /// Fallback list of claim addresses (one per line) for providers that disable `getProgramAccounts`.
    pub claims_file: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            dry_run: false,
            max_sends: 100,
            max_fee_lamports: 100_000_000,
            priority_fee_micro: 0,
            max_retries: 4,
            confirm_timeout_secs: 90,
            min_payer_balance: DEFAULT_MIN_PAYER_BALANCE,
            claims_file: None,
        }
    }
}
