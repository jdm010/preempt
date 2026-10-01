use crate::history::HistoryEntry;
use crate::index::PrefixIndex;
use crate::ngram::NgramIndex;
use crate::Suggestion;

pub struct Engine {
    index: PrefixIndex,
    ngrams: NgramIndex,
    now: i64,
}

impl Engine {
    pub fn build(entries: &[HistoryEntry], now: i64) -> Self {
        Engine {
            index: PrefixIndex::build(entries, now),
            ngrams: NgramIndex::build(entries, now),
            now,
        }
    }

    pub fn from_zsh_text(text: &str, now: i64) -> Self {
        let entries = crate::history::parse_zsh(text);
        Engine::build(&entries, now)
    }

    pub fn suggest(&self, input: &str, max: usize) -> Vec<Suggestion> {
        let prefix_matches = self.index.suggest(input, max);
        if prefix_matches.is_empty() {
            self.ngrams.suggest(input, max)
        } else {
            prefix_matches
        }
    }

    pub fn top(&self, input: &str) -> Option<Suggestion> {
        self.suggest(input, 1).into_iter().next()
    }

    pub fn now(&self) -> i64 {
        self.now
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_700_000_000;

    #[test]
    fn end_to_end_from_zsh_text() {
        let text = ": 1699990000:0;git checkout main\n: 1699999000:0;git status\nplain git log\n";
        let engine = Engine::from_zsh_text(text, NOW);
        let top = engine.top("git ").expect("expected a suggestion");
        assert_eq!(top.full_command, "git status");
        assert_eq!(top.tier, crate::Tier::T0Prefix);
        assert_eq!(top.completion, "status");
    }

    #[test]
    fn no_suggestion_for_unknown_prefix() {
        let engine = Engine::from_zsh_text("ls\ncd /tmp\n", NOW);
        assert!(engine.top("zzz").is_none());
    }

    #[test]
    fn suggest_is_stable_across_calls() {
        let engine = Engine::from_zsh_text("cargo build\n", NOW);
        assert_eq!(engine.suggest("cargo", 5), engine.suggest("cargo", 5));
    }
}
