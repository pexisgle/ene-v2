//! Temporary provider bundles must satisfy the same ACL checks as installed code.
pub(crate) fn package_directory() -> tempfile::TempDir {
    let directory = tempfile::tempdir().unwrap();
    #[cfg(windows)]
    {
        // Recreate this fresh, empty fixture through the private storage API.
        // Keep TempDir's ownership so all contents are removed after the test.
        std::fs::remove_dir(directory.path()).unwrap();
        ene_platform::storage::create_private_dir_all(directory.path()).unwrap();
    }
    directory
}
