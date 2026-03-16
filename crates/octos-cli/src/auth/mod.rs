//! Authentication module: OAuth, device code, paste-token, and keychain flows.

pub mod keychain;
pub mod oauth;
pub mod store;
pub mod token;

pub use keychain::KEYCHAIN_MARKER;
pub use store::{AuthCredential, AuthStore};
