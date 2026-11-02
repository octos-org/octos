//! Authentication credential storage and platform secret resolution.

pub mod keychain;
pub mod store;

pub use keychain::KEYCHAIN_MARKER;
pub use store::{AuthCredential, AuthStore};
