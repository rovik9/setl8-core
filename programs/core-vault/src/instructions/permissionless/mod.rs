//! Permissionless instructions: anyone may call them; the program itself checks the
//! conditions (no admin or sector signature needed).

mod mark_abandoned;

pub use mark_abandoned::*;
