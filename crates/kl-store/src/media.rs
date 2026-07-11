//! Filesystem storage backend for media bytes (Stage G: images only).
//!
//! Object-store (S3-compatible) support is a deliberately deferred later
//! increment per the roadmap's explicit scoping ("images first ... video and
//! remote storage as later increments") — this backend only ever writes to a
//! local directory. Swapping in an object store later means adding a second
//! backend behind the same `storage_ref` contract (a caller-opaque locator),
//! not rewriting this one.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

/// Image MIME types accepted in this stage. Video is explicitly out of scope
/// (see roadmap 1.8) — reject anything else rather than sniffing content.
pub const ALLOWED_IMAGE_MIME_TYPES: &[&str] =
    &["image/png", "image/jpeg", "image/webp", "image/gif"];

pub fn is_allowed_mime(mime_type: &str) -> bool {
    ALLOWED_IMAGE_MIME_TYPES.contains(&mime_type)
}

fn extension_for(mime_type: &str) -> &'static str {
    match mime_type {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        _ => "bin",
    }
}

/// Write `bytes` under `root`, named by their SHA-256 content hash rather than
/// a random id: identical uploads (e.g. the same screenshot ingested twice)
/// collapse onto the same file instead of duplicating storage, while distinct
/// content cannot collide. Returns the absolute path written.
pub fn write_media(root: &Path, mime_type: &str, bytes: &[u8]) -> Result<PathBuf> {
    std::fs::create_dir_all(root)
        .with_context(|| format!("creating media dir {}", root.display()))?;
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    let path = root.join(format!("{hex}.{}", extension_for(mime_type)));
    if !path.exists() {
        std::fs::write(&path, bytes)
            .with_context(|| format!("writing media file {}", path.display()))?;
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_media_names_file_by_content_hash_and_extension() {
        let dir = std::env::temp_dir().join(format!("kl-media-test-{}", std::process::id()));
        let bytes = b"fake-png-bytes";
        let path = write_media(&dir, "image/png", bytes).unwrap();
        assert!(path.exists());
        assert_eq!(path.extension().unwrap(), "png");
        assert_eq!(std::fs::read(&path).unwrap(), bytes);

        // Writing the same bytes again is idempotent (same path, no error).
        let path2 = write_media(&dir, "image/png", bytes).unwrap();
        assert_eq!(path, path2);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rejects_non_image_mime() {
        assert!(!is_allowed_mime("video/mp4"));
        assert!(is_allowed_mime("image/webp"));
    }
}
