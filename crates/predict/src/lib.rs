pub mod engine;
pub mod history;
pub mod index;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    T0Prefix,
    T1Ngram,
    T2LocalLlm,
    T3Cloud,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Suggestion {
    pub completion: String,
    pub full_command: String,
    pub score: f64,
    pub tier: Tier,
}
