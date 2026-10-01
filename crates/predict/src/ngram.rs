use std::collections::HashMap;

use crate::history::HistoryEntry;
use crate::{Suggestion, Tier};

const MAX_CONTEXT: usize = 3;
const HALF_LIFE_SECS: f64 = 7.0 * 24.0 * 60.0 * 60.0;

#[derive(Debug, Default)]
struct NgramStat {
    count: u64,
    last_used: i64,
}

/// Token-level backoff index for contexts that do not match a full history line.
#[derive(Debug)]
pub struct NgramIndex {
    continuations: HashMap<Vec<String>, HashMap<String, NgramStat>>,
    now: i64,
}

impl NgramIndex {
    pub fn build(entries: &[HistoryEntry], now: i64) -> Self {
        let mut continuations: HashMap<Vec<String>, HashMap<String, NgramStat>> = HashMap::new();

        for entry in entries {
            let tokens: Vec<&str> = entry.command.split_whitespace().collect();
            let timestamp = entry.timestamp.unwrap_or(now);

            for index in 1..tokens.len() {
                for context_len in 1..=index.min(MAX_CONTEXT) {
                    let context = tokens[index - context_len..index]
                        .iter()
                        .map(|token| (*token).to_owned())
                        .collect();
                    let stat = continuations
                        .entry(context)
                        .or_default()
                        .entry(tokens[index].to_owned())
                        .or_default();
                    stat.count += 1;
                    stat.last_used = stat.last_used.max(timestamp);
                }
            }
        }

        Self { continuations, now }
    }

    /// Rank next tokens from suffix contexts up to three words long, using shorter contexts as backoff.
    pub fn suggest(&self, input: &str, max: usize) -> Vec<Suggestion> {
        if input.is_empty() {
            return Vec::new();
        }

        let (context, partial, trailing_space) = split_input(input);
        if context.is_empty() {
            return Vec::new();
        }

        let mut candidates: HashMap<&str, f64> = HashMap::new();
        for context_len in (1..=context.len().min(MAX_CONTEXT)).rev() {
            let key: Vec<String> = context[context.len() - context_len..]
                .iter()
                .map(|token| (*token).to_owned())
                .collect();
            let Some(continuations) = self.continuations.get(&key) else {
                continue;
            };

            for (token, stat) in continuations {
                if !token.starts_with(partial) {
                    continue;
                }

                let suffix = &token[partial.len()..];
                if suffix.is_empty() || suffix.chars().any(char::is_control) {
                    continue;
                }

                let age = (self.now - stat.last_used).max(0) as f64;
                let recency = (-age / HALF_LIFE_SECS).exp();
                let specificity = context_len as f64;
                *candidates.entry(token).or_default() += specificity * stat.count as f64 * recency;
            }
        }

        let mut candidates: Vec<(&str, f64)> = candidates.into_iter().collect();
        candidates.sort_by(|(a_token, a_score), (b_token, b_score)| {
            b_score
                .partial_cmp(a_score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a_token.cmp(b_token))
        });

        candidates
            .into_iter()
            .take(max)
            .map(|(token, score)| {
                let suffix = &token[partial.len()..];
                let completion = if trailing_space {
                    token.to_owned()
                } else {
                    suffix.to_owned()
                };
                Suggestion {
                    full_command: format!("{input}{completion}"),
                    completion,
                    score,
                    tier: Tier::T1Ngram,
                }
            })
            .collect()
    }
}

fn split_input(input: &str) -> (Vec<&str>, &str, bool) {
    let trailing_space = input.chars().last().is_some_and(char::is_whitespace);
    if trailing_space {
        (input.split_whitespace().collect(), "", true)
    } else {
        match input.rfind(char::is_whitespace) {
            Some(separator) => {
                let separator_width = input[separator..].chars().next().map_or(0, char::len_utf8);
                (
                    input[..separator].split_whitespace().collect(),
                    &input[separator + separator_width..],
                    false,
                )
            }
            None => (Vec::new(), input, false),
        }
    }
}
