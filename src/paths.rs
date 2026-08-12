//! Filesystem locations kctx cares about, plus the encoding used for overlay file names.
//!
//! Everything here is pure path arithmetic; nothing touches the network and only
//! [`ensure_private_dir`] writes to disk.

use std::io;
use std::path::{Path, PathBuf};

/// The user's home directory, if it can be determined.
pub fn home_dir() -> Option<PathBuf> {
    std::env::home_dir().filter(|p| !p.as_os_str().is_empty())
}

/// kctx's cache directory: `$XDG_CACHE_HOME/kctx`, falling back to `~/.cache/kctx`.
pub fn cache_dir() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CACHE_HOME") {
        let xdg = PathBuf::from(xdg);
        // Relative XDG paths are invalid per spec and must be ignored.
        if xdg.is_absolute() {
            return Some(xdg.join("kctx"));
        }
    }
    home_dir().map(|home| home.join(".cache").join("kctx"))
}

/// Directory holding the generated `current-context` overlay files.
pub fn overlay_dir() -> Option<PathBuf> {
    cache_dir().map(|dir| dir.join("contexts"))
}

/// Create `dir` (and parents) with owner-only permissions, verifying what is already there.
///
/// A directory that already exists is *checked* rather than trusted: kctx writes the overlay files
/// that decide which cluster a shell talks to, so a cache directory another user can modify is not
/// a usable place to keep them.
pub fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;

    if let Some(existing) = deepest_existing_ancestor(dir) {
        check_shared_ancestor(&existing)?;
    }

    match std::fs::symlink_metadata(dir) {
        Ok(metadata) => check_private_dir(dir, &metadata),
        Err(error) if error.kind() == io::ErrorKind::NotFound => std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir),
        Err(error) => Err(error),
    }
}

/// The closest ancestor of `dir` that already exists, ignoring `dir` itself.
fn deepest_existing_ancestor(dir: &Path) -> Option<PathBuf> {
    dir.ancestors()
        .skip(1)
        .find(|ancestor| ancestor.try_exists().unwrap_or(false))
        .map(Path::to_path_buf)
}

/// Reject a directory kctx will create its own directories inside if others can write to it.
///
/// This deliberately follows symbolic links: users legitimately point `$XDG_CACHE_HOME` or
/// `~/.cache` at another location, and what matters is the permissions of the directory the name
/// actually resolves to.
fn check_shared_ancestor(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;

    let metadata = std::fs::metadata(dir)?;
    if !metadata.is_dir() {
        return Err(io::Error::other(format!(
            "{} is not a directory, so kctx cannot keep its cache below it",
            dir.display()
        )));
    }

    // Sticky directories such as `/tmp` are world-writable by design and are still safe to build
    // inside: the sticky bit is what stops anyone but the owner removing or renaming our entries.
    let mode = metadata.mode();
    if mode & 0o022 != 0 && mode & 0o1000 == 0 {
        return Err(io::Error::other(format!(
            "{} is writable by other users (mode {:o}); kctx will not keep its cache below it",
            dir.display(),
            mode & 0o7777
        )));
    }
    Ok(())
}

/// Require `dir` to be a real directory that only its owner can reach.
fn check_private_dir(dir: &Path, metadata: &std::fs::Metadata) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;

    // Checked before `is_dir`, which is false for a symbolic link however it resolves and would
    // otherwise report a link to a perfectly good directory as "not a directory".
    if metadata.file_type().is_symlink() {
        return Err(io::Error::other(format!(
            "{} is a symbolic link; kctx will not write overlays through one",
            dir.display()
        )));
    }
    if !metadata.is_dir() {
        return Err(io::Error::other(format!(
            "{} exists but is not a directory",
            dir.display()
        )));
    }

    let mode = metadata.mode();
    if mode & 0o077 != 0 {
        return Err(io::Error::other(format!(
            "{} is accessible to other users (mode {:o}); run `chmod 700` on it",
            dir.display(),
            mode & 0o7777
        )));
    }
    Ok(())
}

/// Render a path for humans, abbreviating the home directory to `~`.
pub fn shorten_home(path: &Path) -> String {
    match home_dir() {
        Some(home) => match path.strip_prefix(&home) {
            Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => path.display().to_string(),
        },
        None => path.display().to_string(),
    }
}

/// Longest encoded file stem we are willing to produce before falling back to a hash suffix.
const MAX_STEM_LEN: usize = 180;

/// Encode an arbitrary context name into a safe, deterministic file stem.
///
/// Characters outside `[A-Za-z0-9.-]` (including `_` itself) are escaped as `_XX` with the
/// byte's lowercase hex, which keeps the mapping injective: two different context names can
/// never share an overlay file. Overlong names are truncated at an escape boundary and
/// disambiguated with a hash of the full name.
pub fn encode_file_stem(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut truncated = false;

    for byte in name.as_bytes() {
        if out.len() >= MAX_STEM_LEN {
            truncated = true;
            break;
        }
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'-' => out.push(*byte as char),
            _ => out.push_str(&format!("_{byte:02x}")),
        }
    }

    if truncated {
        out.push_str(&format!("-{:016x}", fnv1a64(name.as_bytes())));
    }

    // Never produce a dotfile, `.` or `..`.
    if out.starts_with('.') {
        out.replace_range(0..1, "_2e");
    }
    if out.is_empty() {
        out.push_str("_empty");
    }
    out
}

/// FNV-1a, used only to disambiguate truncated file names. Deterministic across releases,
/// unlike `DefaultHasher`.
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_plain_names_verbatim() {
        assert_eq!(encode_file_stem("production-eu"), "production-eu");
        assert_eq!(encode_file_stem("kind-kind.1"), "kind-kind.1");
    }

    #[test]
    fn escapes_path_and_shell_relevant_characters() {
        assert_eq!(encode_file_stem("a/b"), "a_2fb");
        assert_eq!(encode_file_stem("a b"), "a_20b");
        assert_eq!(encode_file_stem("a_b"), "a_5fb");
        assert_eq!(encode_file_stem(".."), "_2e.");
        assert_eq!(encode_file_stem("."), "_2e");
    }

    #[test]
    fn encoding_is_injective_for_look_alike_names() {
        // Without escaping `_` these three would collide.
        let a = encode_file_stem("a/b");
        let b = encode_file_stem("a_2fb");
        let c = encode_file_stem("a b");
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_ne!(b, c);
    }

    #[test]
    fn escapes_multibyte_characters_bytewise() {
        // Two different unicode names must not collide and must stay ASCII on disk.
        let one = encode_file_stem("clüster");
        let two = encode_file_stem("cluster");
        assert!(one.is_ascii());
        assert_ne!(one, two);
    }

    #[test]
    fn truncates_overlong_names_with_a_stable_hash() {
        let long = "x".repeat(500);
        let encoded = encode_file_stem(&long);
        assert!(
            encoded.len() <= MAX_STEM_LEN + 17,
            "got {} bytes",
            encoded.len()
        );
        assert_eq!(encoded, encode_file_stem(&long), "must be deterministic");

        let other = format!("{}y", "x".repeat(499));
        assert_ne!(encoded, encode_file_stem(&other), "hash must disambiguate");
    }

    #[test]
    fn truncation_never_splits_an_escape_sequence() {
        let long = " ".repeat(500);
        let encoded = encode_file_stem(&long);
        let stem = encoded.split('-').next().unwrap();
        assert_eq!(
            stem.len() % 3,
            0,
            "escape sequences are three bytes each: {stem}"
        );
    }
}
