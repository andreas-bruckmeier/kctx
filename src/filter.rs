//! Search/filter scoring for the context list.
//!
//! Pure functions over the context model: no I/O, no terminal, no Kubernetes. The matcher is a
//! case-insensitive subsequence matcher with a small ranking scheme — plenty for the handful of
//! contexts a kubeconfig set contains, and cheap enough to re-run on every keystroke.

use crate::kubeconfig::ContextEntry;

/// Fields are ranked before match quality, so a weak hit on the context name still beats a
/// perfect hit on a cluster or a file name.
const WEIGHT_NAME: i32 = 4000;
const WEIGHT_CLUSTER: i32 = 3000;
const WEIGHT_NAMESPACE: i32 = 2000;
const WEIGHT_SOURCE: i32 = 1000;

/// A context that matched, with its ranking score.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Match {
    /// Index into the entry slice that was searched.
    pub index: usize,
    /// Higher is better.
    pub score: i32,
}

/// Rank `entries` against `query`, best first.
///
/// An empty query matches everything in the original order. Ties keep discovery order, which
/// means `$KUBECONFIG` entries stay ahead of scanned ones.
pub fn filter(entries: &[ContextEntry], query: &str) -> Vec<Match> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return entries
            .iter()
            .enumerate()
            .map(|(index, _)| Match { index, score: 0 })
            .collect();
    }

    let mut matches: Vec<Match> = entries
        .iter()
        .enumerate()
        .filter_map(|(index, entry)| {
            score_entry(entry, &needle).map(|score| Match { index, score })
        })
        .collect();

    // Stable sort: equal scores preserve discovery order.
    matches.sort_by_key(|entry| std::cmp::Reverse(entry.score));
    matches
}

/// Best score across the searchable fields of one context.
fn score_entry(entry: &ContextEntry, needle: &str) -> Option<i32> {
    let source_name = entry
        .source
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();

    let candidates = [
        (entry.name.as_str(), WEIGHT_NAME),
        (entry.cluster.as_str(), WEIGHT_CLUSTER),
        (entry.namespace.as_deref().unwrap_or(""), WEIGHT_NAMESPACE),
        (source_name.as_str(), WEIGHT_SOURCE),
    ];

    candidates
        .into_iter()
        .filter_map(|(field, weight)| field_score(field, needle).map(|score| weight + score))
        .max()
}

/// Score a single field, or `None` when it does not match at all.
///
/// Quality tiers: exact, prefix, contiguous substring, then subsequence — each penalised by how
/// late the match starts and, for subsequences, by how scattered it is.
fn field_score(field: &str, needle: &str) -> Option<i32> {
    if field.is_empty() {
        return None;
    }
    let field = field.to_lowercase();

    if field == needle {
        return Some(900);
    }
    if let Some(position) = field.find(needle) {
        let penalty = i32::try_from(position).unwrap_or(i32::MAX).min(100);
        return Some(if position == 0 {
            700 - penalty
        } else {
            500 - penalty
        });
    }
    subsequence_score(&field, needle).map(|score| 300 + score)
}

/// Greedy subsequence match: every needle character must appear in order.
///
/// Returns a small negative-or-zero adjustment reflecting how tightly packed the match is, so
/// `pr-eu` scores better against `prod-eu` than against `p-r-o-b-e-u`.
fn subsequence_score(field: &str, needle: &str) -> Option<i32> {
    let mut field_chars = field.chars();
    let mut gaps = 0i32;
    let mut first = None;

    for (position, needle_char) in needle.chars().enumerate() {
        let mut skipped = 0i32;
        loop {
            let candidate = field_chars.next()?;
            if candidate == needle_char {
                if position == 0 {
                    first = Some(skipped);
                }
                gaps += skipped;
                break;
            }
            skipped += 1;
        }
    }

    let start_penalty = first.unwrap_or(0).min(50);
    Some(-(gaps.min(150)) - start_penalty)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kubeconfig::AuthMethod;
    use std::path::Path;
    use std::sync::Arc;

    fn entry(name: &str, cluster: &str, namespace: Option<&str>, source: &str) -> ContextEntry {
        ContextEntry {
            name: name.to_string(),
            cluster: cluster.to_string(),
            user: None,
            namespace: namespace.map(str::to_string),
            server: None,
            cluster_missing: false,
            source: Arc::from(Path::new(source)),
            current_in_source: false,
            active: false,
            ambiguous: false,
            auth_method: AuthMethod::Unspecified,
        }
    }

    fn fixtures() -> Vec<ContextEntry> {
        vec![
            entry(
                "production-eu",
                "prod-cluster",
                Some("payments"),
                "/k/prod.yaml",
            ),
            entry(
                "staging",
                "staging-cluster",
                Some("development"),
                "/k/staging.yaml",
            ),
            entry("kind-local", "kind-kind", None, "/k/config"),
        ]
    }

    fn names(entries: &[ContextEntry], query: &str) -> Vec<String> {
        filter(entries, query)
            .into_iter()
            .map(|m| entries[m.index].name.clone())
            .collect()
    }

    #[test]
    fn empty_query_keeps_every_entry_in_order() {
        let entries = fixtures();
        assert_eq!(
            names(&entries, ""),
            vec!["production-eu", "staging", "kind-local"]
        );
        assert_eq!(
            names(&entries, "   "),
            vec!["production-eu", "staging", "kind-local"]
        );
    }

    #[test]
    fn matches_substrings_case_insensitively() {
        let entries = fixtures();
        assert_eq!(names(&entries, "PROD"), vec!["production-eu"]);
        assert_eq!(names(&entries, "stag"), vec!["staging"]);
    }

    #[test]
    fn matches_subsequences() {
        let entries = fixtures();
        assert_eq!(names(&entries, "pdeu"), vec!["production-eu"]);
        assert_eq!(names(&entries, "kndlcl"), vec!["kind-local"]);
    }

    #[test]
    fn no_match_returns_nothing() {
        let entries = fixtures();
        assert!(filter(&entries, "zzz").is_empty());
    }

    #[test]
    fn context_name_matches_outrank_other_fields() {
        let entries = vec![
            entry("alpha", "payments-cluster", None, "/k/a.yaml"),
            entry("payments-ctx", "other", None, "/k/b.yaml"),
        ];
        // "payments" hits alpha's cluster exactly, but a name match still wins.
        assert_eq!(names(&entries, "payments"), vec!["payments-ctx", "alpha"]);
    }

    #[test]
    fn exact_and_prefix_matches_outrank_late_ones() {
        let entries = vec![
            entry("eu-prod", "c", None, "/k/a.yaml"),
            entry("prod", "c", None, "/k/b.yaml"),
            entry("prod-eu", "c", None, "/k/c.yaml"),
        ];
        assert_eq!(names(&entries, "prod"), vec!["prod", "prod-eu", "eu-prod"]);
    }

    #[test]
    fn tighter_subsequences_rank_higher() {
        let entries = vec![
            entry("p-r-o-b-e-u", "c", None, "/k/a.yaml"),
            entry("prodeu", "c", None, "/k/b.yaml"),
        ];
        assert_eq!(names(&entries, "peu"), vec!["prodeu", "p-r-o-b-e-u"]);
    }

    #[test]
    fn namespace_and_source_file_are_searchable() {
        let entries = fixtures();
        assert_eq!(names(&entries, "development"), vec!["staging"]);
        assert_eq!(names(&entries, "config"), vec!["kind-local"]);
    }

    #[test]
    fn missing_namespace_never_matches() {
        let entries = vec![entry("a", "c", None, "/k/a.yaml")];
        assert!(filter(&entries, "-").is_empty());
    }
}
