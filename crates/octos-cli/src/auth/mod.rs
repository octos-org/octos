//! Authentication module: OAuth, device code, paste-token, and keychain flows.

pub use octos_services::auth::{keychain, store};
pub mod oauth;
pub mod token;

pub use keychain::KEYCHAIN_MARKER;
pub use store::{AuthCredential, AuthStore};
