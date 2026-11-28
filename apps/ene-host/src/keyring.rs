//! Entry creation for the platform credential store on a blocking thread.

#[cfg(target_os = "macos")]
use apple_native_keyring_store::keychain::Store as PlatformStore;
use keyring_core::api::CredentialStoreApi as _;
use keyring_core::{Entry, Error};
#[cfg(windows)]
use windows_native_keyring_store::Store as PlatformStore;
#[cfg(target_os = "linux")]
use zbus_secret_service_keyring_store::Store as PlatformStore;

pub(crate) fn entry(account: &str) -> Result<Entry, Error> {
    // The entry retains the native connection. Dropping it may also block.
    PlatformStore::new()?.build("ene-host", account, None)
}
