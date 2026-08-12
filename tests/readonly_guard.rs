//! Mechanical enforcement of kctx's read-only guarantee.
//!
//! The design intent is that mutation is *impossible to reach*: only `src/kubernetes/read.rs`
//! may construct a `kube::Api`, and that wrapper exposes nothing but `get` and `list`. Comments
//! and reviews are not enough to keep that true over time, so these tests check the source.
//!
//! If a test here fails, the fix is not to relax the test.

use std::path::{Path, PathBuf};

/// The single module allowed to touch `kube::Api`.
const GATEWAY: &str = "kubernetes/read.rs";

/// API methods the gateway may call on the wrapped handle.
const ALLOWED_API_METHODS: &[&str] = &["get", "list"];

/// Markers that only appear in code that mutates cluster state. `PostParams`, `PatchParams` and
/// `DeleteParams` are the useful canaries: `kube` cannot mutate anything without one of them.
const MUTATION_MARKERS: &[&str] = &[
    "PostParams",
    "PatchParams",
    "DeleteParams",
    "Patch::",
    "delete_collection",
    "patch_status",
    "replace_status",
    "patch_metadata",
    "create_subresource",
    "replace_subresource",
    "patch_subresource",
    "Method::POST",
    "Method::PUT",
    "Method::PATCH",
    "Method::DELETE",
    "SelfSubjectReview",
    "SelfSubjectAccessReview",
];

/// Strip `//`-style comments so prose about forbidden operations does not trip the checks.
///
/// The crate uses only line comments, so this is sufficient — and being conservative here is
/// safe: anything left in a string literal is still caught.
fn code_only(contents: &str) -> String {
    contents
        .lines()
        .map(|line| line.split("//").next().unwrap_or(""))
        .collect::<Vec<&str>>()
        .join("\n")
}

/// Every `.rs` file under `src/`, as `(relative path, contents)`.
fn sources() -> Vec<(PathBuf, String)> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    collect(&root, &root, &mut files);
    assert!(
        files.len() > 5,
        "expected to find the crate sources, found {}",
        files.len()
    );
    files
}

/// Recursively gather Rust sources.
fn collect(root: &Path, dir: &Path, out: &mut Vec<(PathBuf, String)>) {
    for entry in std::fs::read_dir(dir)
        .expect("src must be readable")
        .flatten()
    {
        let path = entry.path();
        if path.is_dir() {
            collect(root, &path, out);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            let relative = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
            let contents = std::fs::read_to_string(&path).expect("source must be readable");
            out.push((relative, contents));
        }
    }
}

#[test]
fn only_the_gateway_module_constructs_a_kubernetes_api_handle() {
    for (path, contents) in sources() {
        if path.to_string_lossy() == GATEWAY {
            continue;
        }
        for (number, line) in contents.lines().enumerate() {
            let code = line.split("//").next().unwrap_or(line);
            assert!(
                !(code.contains("Api::") || code.contains("Api<") || code.contains("kube::api::")),
                "{}:{} reaches for kube::Api directly; go through kubernetes::read instead:\n  {}",
                path.display(),
                number + 1,
                line.trim()
            );
        }
    }
}

#[test]
fn the_gateway_only_calls_read_operations() {
    let (_, contents) = sources()
        .into_iter()
        .find(|(path, _)| path.to_string_lossy() == GATEWAY)
        .expect("the gateway module must exist");

    let contents = code_only(&contents);
    let mut called = Vec::new();
    for (index, _) in contents.match_indices("self.api.") {
        let rest = &contents[index + "self.api.".len()..];
        let method: String = rest
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        called.push(method);
    }

    assert!(
        !called.is_empty(),
        "the gateway should actually call the API"
    );
    for method in &called {
        assert!(
            ALLOWED_API_METHODS.contains(&method.as_str()),
            "kubernetes::read calls kube::Api::{method}, which is not a read operation"
        );
    }
}

#[test]
fn no_source_file_mentions_a_mutating_operation() {
    for (path, contents) in sources() {
        let code = code_only(&contents);
        for marker in MUTATION_MARKERS {
            assert!(
                !code.contains(marker),
                "{} contains {marker:?}: kctx must never mutate cluster state",
                path.display()
            );
        }
    }
}

#[test]
fn the_gateway_keeps_its_handle_private() {
    let (_, contents) = sources()
        .into_iter()
        .find(|(path, _)| path.to_string_lossy() == GATEWAY)
        .expect("the gateway module must exist");

    assert!(
        contents.contains("    api: Api<K>,"),
        "the wrapped handle must stay a private field, or callers could reach mutating methods"
    );
}
