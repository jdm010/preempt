use crate::history::HistoryEntry;
use crate::{Suggestion, Tier};

const HALF_LIFE_SECS: f64 = 7.0 * 24.0 * 60.0 * 60.0;

#[derive(Debug, Clone)]
struct Merged {
    command: String,
    count: u64,
    last_used: i64,
}

#[derive(Debug)]
pub struct PrefixIndex {
    sorted: Vec<Merged>,
    now: i64,
}

impl PrefixIndex {
    pub fn build(entries: &[HistoryEntry], now: i64) -> Self {
        let mut map: std::collections::HashMap<&str, Merged> = std::collections::HashMap::new();
        for entry in entries {
            let key = entry.command.trim();
            if key.is_empty() {
                continue;
            }
            let ts = entry.timestamp.unwrap_or(now);
            match map.get_mut(key) {
                Some(m) => {
                    m.count += 1;
                    if ts > m.last_used {
                        m.last_used = ts;
                    }
                }
                None => {
                    map.insert(
                        key,
                        Merged {
                            command: key.to_string(),
                            count: 1,
                            last_used: ts,
                        },
                    );
                }
            }
        }
        let mut sorted: Vec<Merged> = map.into_values().collect();
        sorted.sort_by(|a, b| a.command.cmp(&b.command));
        PrefixIndex { sorted, now }
    }

    pub fn suggest(&self, prefix: &str, max: usize) -> Vec<Suggestion> {
        if prefix.is_empty() {
            return Vec::new();
        }
        let start = self.lower_bound(prefix);
        let mut candidates: Vec<(f64, &Merged)> = Vec::new();
        for merged in &self.sorted[start..] {
            if !merged.command.starts_with(prefix) {
                break;
            }
            let age = (self.now - merged.last_used).max(0) as f64;
            let score = merged.count as f64 * (-age / HALF_LIFE_SECS).exp();
            candidates.push((score, merged));
        }
        candidates.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
        candidates
            .into_iter()
            .take(max)
            .map(|(score, m)| Suggestion {
                completion: m.command[prefix.len()..].to_string(),
                full_command: m.command.clone(),
                score,
                tier: Tier::T0Prefix,
            })
            .collect()
    }

    fn lower_bound(&self, prefix: &str) -> usize {
        let mut lo = 0usize;
        let mut hi = self.sorted.len();
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.sorted[mid].command.as_str() < prefix {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_700_000_000;

    fn entry(cmd: &str, ts: Option<i64>) -> HistoryEntry {
        HistoryEntry { command: cmd.to_string(), timestamp: ts }
    }

    #[test]
    fn empty_prefix_yields_nothing() {
        let index = PrefixIndex::build(&[entry("git status", Some(NOW))], NOW);
        assert!(index.suggest("", 5).is_empty());
    }

    #[test]
    fn returns_ghost_completion_suffix() {
        let index = PrefixIndex::build(&[entry("cargo build --release", Some(NOW))], NOW);
        let s = index.suggest("cargo b", 1);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].completion, "uild --release");
        assert_eq!(s[0].full_command, "cargo build --release");
    }

    #[test]
    fn merges_duplicates_and_ranks_by_frecency() {
        let entries = vec![
            entry("git status", Some(NOW - 100)),
            entry("git status", Some(NOW - 200)),
            entry("git status", Some(NOW - 50)),
            entry("git stash", Some(NOW - 60 * 60 * 24 * 365)),
        ];
        let index = PrefixIndex::build(&entries, NOW);
        let s = index.suggest("git ", 5);
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].full_command, "git status");
    }

    #[test]
    fn recent_beats_frequent_when_stale() {
        let entries = vec![
            entry("npm run dev", Some(NOW - 60 * 60 * 24 * 90)),
            entry("npm run dev", Some(NOW - 60 * 60 * 24 * 91)),
            entry("npm run build", Some(NOW)),
        ];
        let index = PrefixIndex::build(&entries, NOW);
        let s = index.suggest("npm run ", 5);
        assert_eq!(s[0].full_command, "npm run build");
    }

    #[test]
    fn respects_max_results() {
        let entries: Vec<HistoryEntry> = (0..20)
            .map(|i| entry(&format!("git branch-{i}"), Some(NOW - i)))
            .collect();
        let index = PrefixIndex::build(&entries, NOW);
        assert_eq!(index.suggest("git ", 3).len(), 3);
    }

    #[test]
    fn prefix_must_match_from_start() {
        let index = PrefixIndex::build(&[entry("digit", Some(NOW))], NOW);
        assert!(index.suggest("git", 5).is_empty());
    }

    #[test]
    fn missing_timestamp_treated_as_now() {
        let entries = vec![
            entry("kubectl get pods", None),
            entry("kubectl get svc", Some(NOW - 60 * 60 * 24 * 365)),
        ];
        let index = PrefixIndex::build(&entries, NOW);
        let s = index.suggest("kubectl get ", 5);
        assert_eq!(s[0].full_command, "kubectl get pods");
    }
}
