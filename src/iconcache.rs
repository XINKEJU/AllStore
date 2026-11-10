//! Disk cache for card logos.
//!
//! The in-memory bitmap cache dies with the process, so every cold start used
//! to re-download ~850 icons through the six-thread pool. Files now persist
//! under `%LOCALAPPDATA%\AllStore\icons\{fnv64(url)}.img`.
//!
//! Only real images are written. An earlier revision also persisted empty
//! files as "this host has no icon" markers, which turned a single bad fetch
//! (network not up yet, proxy hiccup, blocked CDN) into a *permanent* blank
//! card with no way to recover short of deleting the cache by hand. A miss is
//! now simply a miss: the next launch retries. Legacy empty markers are
//! recognised and removed on read.

use std::path::PathBuf;

/// FNV-1a, same spirit as the `.part` key derivation: stable across runs and
/// independent of the process-default hasher.
fn key(url: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in url.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn path(url: &str) -> Option<PathBuf> {
    let base = std::env::var("LOCALAPPDATA").ok()?;
    Some(
        PathBuf::from(base)
            .join("AllStore")
            .join("icons")
            .join(format!("{:016x}.img", key(url))),
    )
}

/// Cheap signature check so an HTML error page served with HTTP 200 can never
/// be cached as an icon. WIC decodes PNG/JPEG/GIF/BMP/ICO/WEBP — the formats
/// every source actually serves.
fn looks_like_image(b: &[u8]) -> bool {
    b.len() >= 12
        && (b.starts_with(&[0x89, b'P', b'N', b'G'])
            || b.starts_with(&[0xFF, 0xD8, 0xFF])
            || b.starts_with(b"GIF8")
            || b.starts_with(b"BM")
            || b.starts_with(&[0x00, 0x00, 0x01, 0x00])
            || (b.starts_with(b"RIFF") && &b[8..12] == b"WEBP"))
}

/// `Some(bytes)` only when a usable image was cached for this URL. Anything
/// else (missing, truncated, empty legacy marker) is a cache miss.
pub fn read(url: &str) -> Option<Vec<u8>> {
    let p = path(url)?;
    let bytes = std::fs::read(&p).ok()?;
    if looks_like_image(&bytes) {
        Some(bytes)
    } else {
        // Self-heal: drop legacy failure markers and partial writes.
        let _ = std::fs::remove_file(p);
        None
    }
}

/// Persist an image. Failures are deliberately *not* recorded, so a transient
/// network error is retried on the next launch instead of being frozen in.
pub fn write(url: &str, bytes: &[u8]) {
    if !looks_like_image(bytes) {
        return;
    }
    let Some(p) = path(url) else { return };
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // Temp-then-rename: a crash mid-write must not leave a half image that
    // shadows a later successful fetch.
    let tmp = p.with_extension("img.tmp");
    if std::fs::write(&tmp, bytes).is_ok() {
        let _ = std::fs::rename(&tmp, &p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_stable_and_url_dependent() {
        assert_eq!(key("https://a.test/x.png"), key("https://a.test/x.png"));
        assert_ne!(key("https://a.test/a.png"), key("https://a.test/b.png"));
    }

    #[test]
    fn only_image_payloads_are_cacheable() {
        assert!(looks_like_image(&[
            0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0
        ]));
        assert!(looks_like_image(b"GIF89a\x00\x00\x00\x00\x00\x00"));
        assert!(!looks_like_image(b"<!DOCTYPE html>"));
        assert!(!looks_like_image(b""));
        assert!(!looks_like_image(b"<html>oops</html>"));
    }
}
