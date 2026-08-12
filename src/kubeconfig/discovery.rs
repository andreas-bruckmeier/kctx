//! Where to look for kubeconfig files.
//!
//! Discovery is deliberately a plain data struct so additional directories become a field
//! assignment rather than a code change.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use crate::paths;

/// Directories that never contain kubeconfigs, only kubectl's caches.
const SKIPPED_DIRS: &[&str] = &["cache", "http-cache", "oidc-login", "kubelet-plugins"];

/// Extensions that are certainly not kubeconfigs, used to avoid parsing certificate blobs.
const SKIPPED_EXTENSIONS: &[&str] = &[
    "crt", "cert", "key", "pem", "pub", "csr", "json", "lock", "log", "db", "txt", "md", "sh",
    "bak",
];

/// Ordered set of locations to search for kubeconfig files.
#[derive(Debug, Clone)]
pub struct DiscoverySources {
    /// Files listed in `$KUBECONFIG`, in order. Searched first, so they win merge conflicts.
    pub kubeconfig_env: Vec<PathBuf>,
    /// The conventional `~/.kube/config`.
    pub default_config: Option<PathBuf>,
    /// Directories scanned for further kubeconfig-like files.
    pub scan_dirs: Vec<PathBuf>,
    /// How deep to recurse into `scan_dirs` (1 = only files directly inside).
    pub max_depth: usize,
    /// Files larger than this are never parsed.
    pub max_file_size: u64,
}

impl Default for DiscoverySources {
    fn default() -> Self {
        Self {
            kubeconfig_env: Vec::new(),
            default_config: None,
            scan_dirs: Vec::new(),
            max_depth: 2,
            max_file_size: 5 * 1024 * 1024,
        }
    }
}

impl DiscoverySources {
    /// Build the default source set from the process environment.
    pub fn from_env() -> Self {
        Self::new(
            std::env::var_os("KUBECONFIG").as_deref(),
            paths::home_dir().as_deref(),
        )
    }

    /// Build a source set from explicit inputs. Used by [`Self::from_env`] and by tests.
    pub fn new(kubeconfig_env: Option<&OsStr>, home: Option<&Path>) -> Self {
        let kubeconfig_env = kubeconfig_env
            .map(|value| {
                std::env::split_paths(value)
                    .filter(|p| !p.as_os_str().is_empty())
                    .collect()
            })
            .unwrap_or_default();
        let kube_dir = home.map(|home| home.join(".kube"));

        Self {
            kubeconfig_env,
            default_config: kube_dir.as_ref().map(|dir| dir.join("config")),
            scan_dirs: kube_dir.into_iter().collect(),
            ..Self::default()
        }
    }

    /// Every candidate file, in priority order, deduplicated by canonical path.
    ///
    /// `$KUBECONFIG` entries are returned even when they do not exist, so the caller can report
    /// them as inaccessible — the user asked for them explicitly. A missing `~/.kube/config` and
    /// implausible scanned files are filtered out silently.
    pub fn candidates(&self) -> Vec<PathBuf> {
        let mut seen = HashSet::new();
        let mut out = Vec::new();

        for path in &self.kubeconfig_env {
            push_unique(&mut out, &mut seen, path.clone());
        }
        if let Some(default_config) = &self.default_config
            && default_config.is_file()
        {
            push_unique(&mut out, &mut seen, default_config.clone());
        }
        for dir in &self.scan_dirs {
            let mut scanned = Vec::new();
            self.scan(dir, 1, &mut scanned);
            for path in scanned {
                push_unique(&mut out, &mut seen, path);
            }
        }
        out
    }

    /// Recursively collect plausible kubeconfig files below `dir`.
    fn scan(&self, dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
        if depth > self.max_depth {
            return;
        }
        let Ok(read_dir) = std::fs::read_dir(dir) else {
            tracing::debug!(dir = %dir.display(), "directory not readable, skipping");
            return;
        };

        let mut files = Vec::new();
        let mut subdirs = Vec::new();
        for entry in read_dir.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') {
                continue;
            }
            // `metadata` follows symlinks, so a symlinked kubeconfig is still discovered.
            let Ok(metadata) = std::fs::metadata(&path) else {
                continue;
            };
            if metadata.is_dir() {
                if !SKIPPED_DIRS.contains(&name.as_ref()) {
                    subdirs.push(path);
                }
            } else if metadata.is_file() && self.is_plausible_file(&path, metadata.len()) {
                files.push(path);
            }
        }

        // `read_dir` order is unspecified; sort for a stable, predictable context list.
        files.sort();
        subdirs.sort();
        out.append(&mut files);
        for subdir in subdirs {
            self.scan(&subdir, depth + 1, out);
        }
    }

    /// Cheap pre-filter applied before a file is handed to the YAML parser.
    fn is_plausible_file(&self, path: &Path, size: u64) -> bool {
        if size == 0 || size > self.max_file_size {
            return false;
        }
        match path.extension().and_then(OsStr::to_str) {
            Some(extension) => {
                !SKIPPED_EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str())
            }
            None => true,
        }
    }
}

/// Append `path` unless an equivalent path was already collected.
fn push_unique(out: &mut Vec<PathBuf>, seen: &mut HashSet<PathBuf>, path: PathBuf) {
    // Canonicalize so `~/.kube/config` and a symlink to it are recognised as one file;
    // fall back to the literal path for files that do not exist yet.
    let key = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
    if seen.insert(key) {
        out.push(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    const MINIMAL: &str = "apiVersion: v1\nkind: Config\ncontexts: []\n";

    #[test]
    fn env_entries_come_first_and_in_order() {
        let home = tempfile::tempdir().unwrap();
        let a = home.path().join("a.yaml");
        let b = home.path().join("b.yaml");
        write(&a, MINIMAL);
        write(&b, MINIMAL);
        write(&home.path().join(".kube/config"), MINIMAL);

        let env = format!("{}:{}", b.display(), a.display());
        let sources = DiscoverySources::new(Some(OsStr::new(&env)), Some(home.path()));
        let candidates = sources.candidates();

        assert_eq!(candidates[0], b);
        assert_eq!(candidates[1], a);
        assert_eq!(candidates[2], home.path().join(".kube/config"));
    }

    #[test]
    fn missing_env_entries_are_kept_for_error_reporting() {
        let home = tempfile::tempdir().unwrap();
        let missing = home.path().join("nope.yaml");
        let env = missing.display().to_string();

        let sources = DiscoverySources::new(Some(OsStr::new(&env)), Some(home.path()));
        assert_eq!(sources.candidates(), vec![missing]);
    }

    #[test]
    fn empty_env_entries_are_ignored() {
        let home = tempfile::tempdir().unwrap();
        let sources = DiscoverySources::new(Some(OsStr::new("::")), Some(home.path()));
        assert!(sources.candidates().is_empty());
    }

    #[test]
    fn scans_kube_dir_recursively_and_skips_caches() {
        let home = tempfile::tempdir().unwrap();
        let kube = home.path().join(".kube");
        write(&kube.join("config"), MINIMAL);
        write(&kube.join("prod.yaml"), MINIMAL);
        write(&kube.join("clusters/staging.conf"), MINIMAL);
        write(&kube.join("cache/discovery/thing.yaml"), MINIMAL);
        write(&kube.join("http-cache/x.yaml"), MINIMAL);
        write(&kube.join("ca.crt"), "not a kubeconfig");
        write(&kube.join(".hidden.yaml"), MINIMAL);
        write(&kube.join("empty.yaml"), "");

        let sources = DiscoverySources::new(None, Some(home.path()));
        let candidates = sources.candidates();

        assert!(candidates.contains(&kube.join("config")));
        assert!(candidates.contains(&kube.join("prod.yaml")));
        assert!(candidates.contains(&kube.join("clusters/staging.conf")));
        assert!(!candidates.iter().any(|p| p.starts_with(kube.join("cache"))));
        assert!(
            !candidates
                .iter()
                .any(|p| p.starts_with(kube.join("http-cache")))
        );
        assert!(!candidates.contains(&kube.join("ca.crt")));
        assert!(!candidates.contains(&kube.join(".hidden.yaml")));
        assert!(!candidates.contains(&kube.join("empty.yaml")));
    }

    #[test]
    fn depth_limit_is_respected() {
        let home = tempfile::tempdir().unwrap();
        let kube = home.path().join(".kube");
        write(&kube.join("a/b/deep.yaml"), MINIMAL);

        let mut sources = DiscoverySources::new(None, Some(home.path()));
        sources.max_depth = 2;
        assert!(sources.candidates().is_empty());

        sources.max_depth = 3;
        assert_eq!(sources.candidates(), vec![kube.join("a/b/deep.yaml")]);
    }

    #[test]
    fn the_same_file_is_only_listed_once() {
        let home = tempfile::tempdir().unwrap();
        let config = home.path().join(".kube/config");
        write(&config, MINIMAL);

        // Reached via $KUBECONFIG, via the default path, and via the directory scan.
        let env = config.display().to_string();
        let sources = DiscoverySources::new(Some(OsStr::new(&env)), Some(home.path()));
        assert_eq!(sources.candidates(), vec![config]);
    }

    #[test]
    fn symlinks_to_the_same_file_are_deduplicated() {
        let home = tempfile::tempdir().unwrap();
        let kube = home.path().join(".kube");
        let real = kube.join("config");
        write(&real, MINIMAL);
        std::os::unix::fs::symlink(&real, kube.join("alias.yaml")).unwrap();

        let sources = DiscoverySources::new(None, Some(home.path()));
        assert_eq!(sources.candidates(), vec![real]);
    }

    #[test]
    fn extra_scan_dirs_are_honoured() {
        let home = tempfile::tempdir().unwrap();
        let extra = tempfile::tempdir().unwrap();
        write(&extra.path().join("team.yaml"), MINIMAL);

        let mut sources = DiscoverySources::new(None, Some(home.path()));
        sources.scan_dirs.push(extra.path().to_path_buf());

        assert_eq!(sources.candidates(), vec![extra.path().join("team.yaml")]);
    }

    #[test]
    fn oversized_files_are_skipped() {
        let home = tempfile::tempdir().unwrap();
        let kube = home.path().join(".kube");
        write(&kube.join("huge.yaml"), &"x".repeat(4096));

        let mut sources = DiscoverySources::new(None, Some(home.path()));
        sources.max_file_size = 1024;
        assert!(sources.candidates().is_empty());
    }
}
