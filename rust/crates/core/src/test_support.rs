//! Test-only fixtures shared across modules.

/// A fresh tree under the OS temporary directory, named after `tag` and removed
/// when the returned handle drops.
pub(crate) fn temp_dir(tag: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(&format!("kloop-{tag}-"))
        .tempdir()
        .expect("create a temporary directory")
}
