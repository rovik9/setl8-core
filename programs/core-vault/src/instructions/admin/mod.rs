//! Admin instructions: every one needs BOTH admin signatures (SL8 + Rov, a 2-of-2 multisig).

mod admin_withdraw_marketing_funds;
mod init_vault;
mod register_product;
mod update_product_config;
mod pause_product;
mod reactivate_product;

pub use admin_withdraw_marketing_funds::*;
pub use init_vault::*;
pub use register_product::*;
pub use update_product_config::*;
pub use pause_product::*;
pub use reactivate_product::*;
