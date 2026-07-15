//! Loading `--image <path>` files into content blocks at startup: refuse
//! remote URLs (only local files, keeping the SSRF surface narrow like codex),
//! read the bytes, and validate them through core's shared image ingestion.
//! The resulting blocks attach to the first user turn.

use std::path::Path;
use std::path::PathBuf;

use anyhow::bail;
use anyhow::Context;
use anyhow::Result;

use kloop_core::image::image_block_from_bytes;
use kloop_protocol::ContentBlock;

/// Read and validate every `--image` path into an Image content block. A path
/// that looks like a remote URL is refused; a read or validation failure
/// aborts startup with the offending path in context.
pub(crate) fn load_images(paths: &[PathBuf]) -> Result<Vec<ContentBlock>> {
    paths.iter().map(|path| load_one(path)).collect()
}

fn load_one(path: &Path) -> Result<ContentBlock> {
    if is_remote_url(path) {
        bail!(
            "--image {}: remote URLs are not supported; pass a local file path",
            path.display()
        );
    }
    let bytes = std::fs::read(path)
        .with_context(|| format!("--image {}: cannot read file", path.display()))?;
    image_block_from_bytes(&bytes).with_context(|| format!("--image {}", path.display()))
}

/// A path whose leading component is an http(s) scheme — refused so `--image`
/// never fetches over the network.
fn is_remote_url(path: &Path) -> bool {
    path.to_str().is_some_and(|s| {
        let lower = s.to_ascii_lowercase();
        lower.starts_with("http://") || lower.starts_with("https://")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_remote_urls() {
        assert!(is_remote_url(Path::new("http://example.com/a.png")));
        assert!(is_remote_url(Path::new("HTTPS://example.com/a.png")));
        assert!(!is_remote_url(Path::new("/local/a.png")));
        assert!(!is_remote_url(Path::new("a.png")));
    }

    #[test]
    fn remote_url_and_missing_file_are_refused() {
        assert!(load_one(Path::new("https://example.com/a.png")).is_err());
        assert!(load_one(Path::new("/no/such/file/really.png")).is_err());
    }
}
