//! Input-facing adapter for the terminal's prediction overlay.
//!
//! The shell integration supplies the current command line and cursor state.
//! Raw terminal key events are not enough to track shell editing reliably.

use std::sync::{Arc, OnceLock};

use preempt_predict::t2::{T2Prefetcher, T2Result};
use preempt_predict::{engine::Engine, history, Suggestion, Tier};
pub use preempt_safety::{RiskAssessment, RiskLevel};
use preempt_store::{FeedbackWriter, HistoryStore, StoreError, SuggestionPersonalizer};

const MAX_CANDIDATES: usize = 5;
const MAX_RERANK_CANDIDATES: usize = 64;

fn shared_t2_prefetcher() -> Option<Arc<T2Prefetcher>> {
    static PREFETCHER: OnceLock<Option<Arc<T2Prefetcher>>> = OnceLock::new();

    PREFETCHER
        .get_or_init(|| {
            let model_path = preempt_predict::t2::default_model_path()?;
            if !preempt_predict::t2::model_file_exists(&model_path) {
                return None;
            }
            T2Prefetcher::new(model_path).ok().map(Arc::new)
        })
        .clone()
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FeedbackCounts {
    pub t0_accepted: u64,
    pub t0_rejected: u64,
    pub t1_accepted: u64,
    pub t1_rejected: u64,
    pub t2_accepted: u64,
    pub t2_rejected: u64,
    pub t3_accepted: u64,
    pub t3_rejected: u64,
}

impl FeedbackCounts {
    fn record(&mut self, tier: Tier, accepted: bool) {
        match (tier, accepted) {
            (Tier::T0Prefix, true) => self.t0_accepted += 1,
            (Tier::T0Prefix, false) => self.t0_rejected += 1,
            (Tier::T1Ngram, true) => self.t1_accepted += 1,
            (Tier::T1Ngram, false) => self.t1_rejected += 1,
            (Tier::T2LocalLlm, true) => self.t2_accepted += 1,
            (Tier::T2LocalLlm, false) => self.t2_rejected += 1,
            (Tier::T3Cloud, true) => self.t3_accepted += 1,
            (Tier::T3Cloud, false) => self.t3_rejected += 1,
        }
    }
}

#[derive(Debug, Clone)]
struct PendingFeedback {
    input: String,
    completion: String,
    tier: Tier,
}

/// Maintains the suggestions shown for the shell's current input line.
pub struct PredictionOverlay {
    engine: Engine,
    enabled: bool,
    candidates: Vec<Suggestion>,
    selected: usize,
    current_input: Option<String>,
    pending_feedback: Option<PendingFeedback>,
    feedback: FeedbackCounts,
    feedback_writer: Option<FeedbackWriter>,
    personalizer: Option<SuggestionPersonalizer>,
    t2_prefetcher: Option<Arc<T2Prefetcher>>,
    t2_result_handler: Option<Arc<dyn Fn(T2Result) + Send + Sync>>,
    pending_t2_request: Option<u64>,
}

impl PredictionOverlay {
    pub fn new(engine: Engine) -> Self {
        Self {
            engine,
            enabled: true,
            candidates: Vec::new(),
            selected: 0,
            current_input: None,
            pending_feedback: None,
            feedback: FeedbackCounts::default(),
            feedback_writer: None,
            personalizer: None,
            t2_prefetcher: shared_t2_prefetcher(),
            t2_result_handler: None,
            pending_t2_request: None,
        }
    }

    pub fn from_default_history(now: i64) -> std::io::Result<Self> {
        let entries = history::load(&history::default_history_path())?;
        Ok(Self::new(Engine::build(&entries, now)))
    }

    /// Build from encrypted history, importing the current zsh history file
    /// through the store's redactor and attaching asynchronous feedback storage.
    pub fn from_encrypted_default_history(now: i64) -> Result<Self, StoreError> {
        let mut store = HistoryStore::open_default_with_keyring()?;
        if let Ok(entries) = history::load(&history::default_history_path()) {
            store.replace_shell_history(&entries)?;
        }
        let entries = store.prediction_history(100_000)?;
        let personalizer = store.suggestion_personalizer()?;
        let feedback_writer = store.into_feedback_writer()?;
        let mut overlay = Self::new(Engine::build(&entries, now));
        overlay.feedback_writer = Some(feedback_writer);
        overlay.personalizer = Some(personalizer);
        Ok(overlay)
    }

    pub fn empty(now: i64) -> Self {
        Self::new(Engine::build(&[], now))
    }

    /// Refresh candidates from the shell-provided input line.
    ///
    /// Suggestions are hidden when the cursor is not at the end of the line,
    /// since the suffix could not be drawn or accepted at the cursor position.
    pub fn update(&mut self, input_line: &str, cursor_at_end: bool) {
        self.resolve_pending_feedback(input_line, cursor_at_end);

        if !cursor_at_end {
            self.cancel_t2_request();
            self.candidates.clear();
            self.selected = 0;
            self.current_input = None;
            return;
        }

        if self.current_input.as_deref() == Some(input_line) {
            return;
        }

        self.cancel_t2_request();
        self.candidates.clear();
        self.selected = 0;
        self.current_input = Some(input_line.to_owned());

        if !self.enabled || input_line.is_empty() {
            return;
        }

        let mut suggestions = self.engine.suggest(input_line, MAX_RERANK_CANDIDATES);
        if let Some(personalizer) = &self.personalizer {
            for suggestion in &mut suggestions {
                suggestion.score *=
                    personalizer.score_multiplier(&suggestion.full_command, suggestion.tier);
            }
            suggestions.sort_by(|left, right| right.score.total_cmp(&left.score));
        }
        self.candidates = suggestions
            .into_iter()
            .filter(|suggestion| {
                !suggestion.completion.is_empty()
                    && !suggestion.completion.chars().any(char::is_control)
            })
            .take(MAX_CANDIDATES)
            .collect();

        self.arm_pending_feedback();
        self.request_t2(input_line);
    }

    pub fn has_suggestion(&self) -> bool {
        self.completion().is_some()
    }

    pub fn completion(&self) -> Option<&str> {
        self.candidates
            .get(self.selected)
            .map(|suggestion| suggestion.completion.as_str())
    }

    pub fn selected_command(&self) -> Option<&str> {
        self.candidates
            .get(self.selected)
            .map(|suggestion| suggestion.full_command.as_str())
    }

    /// Risk hint for the currently selected command suggestion.
    pub fn selected_risk(&self) -> Option<RiskAssessment> {
        self.selected_command().map(preempt_safety::assess)
    }

    pub fn cycle_next(&mut self) -> bool {
        if self.candidates.len() > 1 {
            self.selected = (self.selected + 1) % self.candidates.len();
            self.arm_pending_feedback();
            true
        } else {
            self.has_suggestion()
        }
    }

    pub fn cycle_previous(&mut self) -> bool {
        if self.candidates.len() > 1 {
            self.selected = (self.selected + self.candidates.len() - 1) % self.candidates.len();
            self.arm_pending_feedback();
            true
        } else {
            self.has_suggestion()
        }
    }

    /// Return the selected suffix for the shell to insert, then clear the overlay.
    pub fn accept(&mut self) -> Option<String> {
        let suggestion = self.candidates.get(self.selected)?;
        let completion = suggestion.completion.to_owned();
        let command = suggestion.full_command.to_owned();
        let tier = suggestion.tier;
        self.record_feedback(tier, true, &command);
        self.clear();
        Some(completion)
    }

    /// Take an asynchronous feedback persistence error for application logging.
    pub fn take_persistence_error(&self) -> Option<String> {
        self.feedback_writer.as_ref()?.take_error()
    }

    /// Attach the window event bridge used to deliver completed T2 predictions.
    pub fn set_t2_result_handler(&mut self, handler: Arc<dyn Fn(T2Result) + Send + Sync>) {
        self.t2_result_handler = Some(handler);
    }

    /// Apply speculative completions only if they still belong to the current input.
    pub fn apply_t2_result(
        &mut self,
        request_id: u64,
        input: &str,
        completions: Result<Vec<String>, String>,
    ) -> Result<bool, String> {
        if self.pending_t2_request != Some(request_id)
            || self.current_input.as_deref() != Some(input)
        {
            return Ok(false);
        }
        self.pending_t2_request = None;

        let had_candidates = !self.candidates.is_empty();
        let mut model_candidates = Vec::new();
        for completion in completions?.into_iter().take(3) {
            if completion.is_empty() || completion.chars().any(char::is_control) {
                continue;
            }
            let full_command = format!("{input}{completion}");
            if self
                .candidates
                .iter()
                .any(|suggestion| suggestion.full_command == full_command)
                || model_candidates
                    .iter()
                    .any(|suggestion: &Suggestion| suggestion.full_command == full_command)
            {
                continue;
            }
            let score = self.personalizer.as_ref().map_or(0.0, |personalizer| {
                personalizer.preference(&full_command, Tier::T2LocalLlm)
            });
            model_candidates.push(Suggestion {
                completion,
                full_command,
                score,
                tier: Tier::T2LocalLlm,
            });
        }
        model_candidates.sort_by(|left, right| right.score.total_cmp(&left.score));
        let added = !model_candidates.is_empty();
        self.candidates.extend(model_candidates);
        if added && !had_candidates {
            self.arm_pending_feedback();
        }
        Ok(added)
    }

    /// Read and reset the local aggregate feedback counters.
    pub fn take_feedback_counts(&mut self) -> FeedbackCounts {
        std::mem::take(&mut self.feedback)
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if !enabled {
            self.clear();
        }
    }

    pub fn clear(&mut self) {
        self.cancel_t2_request();
        self.candidates.clear();
        self.selected = 0;
        self.current_input = None;
        self.pending_feedback = None;
    }

    fn arm_pending_feedback(&mut self) {
        self.pending_feedback = self.candidates.get(self.selected).and_then(|suggestion| {
            Some(PendingFeedback {
                input: self.current_input.as_ref()?.clone(),
                completion: suggestion.completion.clone(),
                tier: suggestion.tier,
            })
        });
    }

    fn resolve_pending_feedback(&mut self, input_line: &str, cursor_at_end: bool) {
        let Some(pending) = self.pending_feedback.take() else {
            return;
        };
        if !cursor_at_end || input_line == pending.input {
            self.pending_feedback = Some(pending);
            return;
        }

        if let Some(typed_suffix) = input_line.strip_prefix(&pending.input) {
            if typed_suffix == pending.completion {
                let command = format!("{}{}", pending.input, pending.completion);
                self.record_feedback(pending.tier, true, &command);
            } else if !pending.completion.starts_with(typed_suffix) {
                let command = format!("{}{}", pending.input, pending.completion);
                self.record_feedback(pending.tier, false, &command);
            } else {
                // Keep the original prediction while the user types its matching prefix.
                self.pending_feedback = Some(pending);
            }
        } else {
            let command = format!("{}{}", pending.input, pending.completion);
            self.record_feedback(pending.tier, false, &command);
        }
    }

    fn record_feedback(&mut self, tier: Tier, accepted: bool, command: &str) {
        self.feedback.record(tier, accepted);
        if let Some(personalizer) = &mut self.personalizer {
            personalizer.record(command, tier, accepted);
        }
        if let Some(writer) = &self.feedback_writer {
            writer.record(tier, accepted);
            writer.record_suggestion(command, tier, accepted);
        }
    }

    fn request_t2(&mut self, input_line: &str) {
        if input_line.trim().len() < 3 || input_line.len() > 512 {
            return;
        }
        let (Some(prefetcher), Some(handler)) = (&self.t2_prefetcher, &self.t2_result_handler)
        else {
            return;
        };
        self.pending_t2_request =
            Some(prefetcher.request(input_line.to_owned(), Arc::clone(handler)));
    }

    fn cancel_t2_request(&mut self) {
        if let (Some(prefetcher), Some(request_id)) =
            (&self.t2_prefetcher, self.pending_t2_request.take())
        {
            prefetcher.cancel(request_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use preempt_predict::history::HistoryEntry;

    use super::*;

    const NOW: i64 = 1_700_000_000;

    fn overlay(commands: &[&str]) -> PredictionOverlay {
        let entries: Vec<_> = commands
            .iter()
            .map(|command| HistoryEntry {
                command: (*command).to_owned(),
                timestamp: Some(NOW),
            })
            .collect();
        PredictionOverlay::new(Engine::build(&entries, NOW))
    }

    #[test]
    fn uses_ngram_fallback_and_records_acceptance() {
        let mut overlay = overlay(&["echo git status --short"]);

        overlay.update("git st", true);

        assert_eq!(overlay.completion(), Some("atus"));
        assert_eq!(overlay.accept().as_deref(), Some("atus"));
        let feedback = overlay.take_feedback_counts();
        assert_eq!(feedback.t1_accepted, 1);
        assert_eq!(feedback.t1_rejected, 0);
    }

    #[test]
    fn records_rejection_when_input_diverges_from_suggestion() {
        let mut overlay = overlay(&["git status"]);

        overlay.update("git ", true);
        overlay.update("git x", true);

        let feedback = overlay.take_feedback_counts();
        assert_eq!(feedback.t0_rejected, 1);
        assert_eq!(feedback.t0_accepted, 0);
    }

    #[test]
    fn repeated_redraw_preserves_cycled_candidate() {
        let mut overlay = overlay(&["git status", "git stash"]);

        overlay.update("git ", true);
        assert!(overlay.cycle_next());
        let selected = overlay.selected_command().map(str::to_owned);

        overlay.update("git ", true);

        assert_eq!(overlay.selected_command(), selected.as_deref());
    }

    #[test]
    fn hides_suggestion_when_cursor_moves_before_input_end() {
        let mut overlay = overlay(&["git status"]);

        overlay.update("git ", true);
        assert!(overlay.has_suggestion());

        overlay.update("git ", false);

        assert!(!overlay.has_suggestion());
    }
}
