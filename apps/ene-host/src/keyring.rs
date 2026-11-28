//! Entry creation for the platform credential store.

#[cfg(target_os = "macos")]
use apple_native_keyring_store::keychain::Store as PlatformStore;
use keyring_core::api::CredentialStoreApi as _;
use once_cell::sync::OnceCell;
use std::{fmt, sync::Arc};
#[cfg(windows)]
use windows_native_keyring_store::Store as PlatformStore;
#[cfg(target_os = "linux")]
use zbus_secret_service_keyring_store::Store as PlatformStore;

pub use keyring_core::{Entry, Error};

/// Lazily initializes the native store, retaining only successful initialization.
#[derive(Default)]
pub struct Keyring(OnceCell<Arc<PlatformStore>>);

impl Keyring {
    /// Creates a handle without contacting the platform service.
    pub const fn new() -> Self {
        Self(OnceCell::new())
    }

    /// Opens an entry for the caller's service and account. May block on native I/O.
    pub fn entry(&self, service: &str, account: &str) -> Result<Entry, Error> {
        self.0
            .get_or_try_init(PlatformStore::new)?
            .build(service, account, None)
    }
}

impl fmt::Debug for Keyring {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("Keyring").finish_non_exhaustive()
    }
}
