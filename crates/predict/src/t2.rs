//! Local GGUF completion and debounced speculative prefetch.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use llama_cpp_2::context::params::LlamaContextParams;
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::model::{AddBos, LlamaChatMessage, LlamaModel};
use llama_cpp_2::sampling::LlamaSampler;

use crate::{t2_user_prompt, T2_SYSTEM_PROMPT};

const DEBOUNCE: Duration = Duration::from_millis(220);
const CONTEXT_TOKENS: u32 = 768;
const MAX_PROMPT_TOKENS: usize = 640;
const MAX_OUTPUT_TOKENS: usize = 48;
const MAX_COMPLETION_CHARS: usize = 256;
const DEFAULT_T2_CANDIDATES: usize = 2;
const MAX_T2_CANDIDATES: usize = 3;

#[derive(Debug, Clone)]
pub struct T2Result {
    pub request_id: u64,
    pub input: String,
    pub completions: Result<Vec<String>, String>,
}

type ResultHandler = Arc<dyn Fn(T2Result) + Send + Sync + 'static>;

struct Request {
    id: u64,
    sample_seed: u32,
    input: String,
    handler: ResultHandler,
}

#[derive(Default)]
struct State {
    request_id: u64,
    pending: Option<Request>,
    stopping: bool,
}

struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

/// One background model worker shared by terminal windows in this process.
/// New keystrokes replace pending work and cancel generation for stale input.
pub struct T2Prefetcher {
    shared: Arc<Shared>,
}

impl T2Prefetcher {
    pub fn new(model_path: impl Into<PathBuf>) -> std::io::Result<Self> {
        Self::with_candidate_limit(model_path, DEFAULT_T2_CANDIDATES)
    }

    /// Create a worker with a smaller candidate budget for latency evaluation.
    pub fn with_candidate_limit(
        model_path: impl Into<PathBuf>,
        candidate_limit: usize,
    ) -> std::io::Result<Self> {
        if !(1..=MAX_T2_CANDIDATES).contains(&candidate_limit) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("candidate limit must be between 1 and {MAX_T2_CANDIDATES}"),
            ));
        }
        let model_path = model_path.into();
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
        });
        let worker_shared = Arc::clone(&shared);
        thread::Builder::new()
            .name("preempt-t2-prefetch".to_owned())
            .spawn(move || run_worker(model_path, worker_shared, candidate_limit))?;
        Ok(Self { shared })
    }

    /// Queue candidate generation for the latest input; the callback runs on the model worker.
    pub fn request(&self, input: String, handler: ResultHandler) -> u64 {
        self.queue_request(input, None, handler)
    }

    /// Queue candidate generation with a caller-provided sampling seed.
    ///
    /// This is useful for replaying the same input set under different policies:
    /// sampling then stays attached to each example instead of request order.
    pub fn request_with_seed(
        &self,
        input: String,
        sample_seed: u32,
        handler: ResultHandler,
    ) -> u64 {
        self.queue_request(input, Some(sample_seed), handler)
    }

    fn queue_request(
        &self,
        input: String,
        sample_seed: Option<u32>,
        handler: ResultHandler,
    ) -> u64 {
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        state.request_id = state.request_id.wrapping_add(1).max(1);
        let id = state.request_id;
        state.pending = Some(Request {
            id,
            sample_seed: sample_seed.unwrap_or(id as u32),
            input,
            handler,
        });
        self.shared.changed.notify_one();
        id
    }

    /// Cancel a request if it is still the most recent one.
    pub fn cancel(&self, request_id: u64) {
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if state.request_id == request_id {
            state.request_id = state.request_id.wrapping_add(1).max(1);
            state.pending = None;
            self.shared.changed.notify_one();
        }
    }
}

/// Resolve the model path used by the application and local evaluation tools.
pub fn default_model_path() -> Option<PathBuf> {
    if let Some(path) =
        std::env::var_os("PREEMPT_GGUF").or_else(|| std::env::var_os("AUTO_TERMINAL_GGUF"))
    {
        return Some(PathBuf::from(path));
    }

    #[cfg(target_os = "macos")]
    {
        // Preserve the pre-rename app-data path to reuse locally installed weights.
        return std::env::var_os("HOME").map(|home| {
            PathBuf::from(home)
                .join("Library")
                .join("Application Support")
                .join("auto-terminal")
                .join("model.gguf")
        });
    }

    #[cfg(target_os = "windows")]
    {
        return std::env::var_os("APPDATA").map(|app_data| {
            PathBuf::from(app_data)
                .join("auto-terminal")
                .join("model.gguf")
        });
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share"))
            })
            .map(|data| data.join("auto-terminal").join("model.gguf"))
    }
}

impl Drop for T2Prefetcher {
    fn drop(&mut self) {
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        state.stopping = true;
        self.shared.changed.notify_one();
    }
}

fn run_worker(model_path: PathBuf, shared: Arc<Shared>, candidate_limit: usize) {
    let backend = LlamaBackend::init().map_err(|error| error.to_string());
    let model = backend
        .as_ref()
        .map_err(|error| error.clone())
        .and_then(|backend| {
            LlamaModel::load_from_file(backend, &model_path, &LlamaModelParams::default())
                .map_err(|error| error.to_string())
        });
    let mut context = model
        .as_ref()
        .map_err(|error| error.clone())
        .and_then(|model| {
            let backend = backend.as_ref().map_err(|error| error.clone())?;
            let threads = thread::available_parallelism().map_or(2, |threads| {
                i32::try_from(threads.get().min(4)).unwrap_or(2)
            });
            let params = LlamaContextParams::default()
                .with_n_ctx(std::num::NonZeroU32::new(CONTEXT_TOKENS))
                .with_n_batch(CONTEXT_TOKENS)
                .with_n_threads(threads)
                .with_n_threads_batch(threads);
            model
                .new_context(backend, params)
                .map_err(|error| error.to_string())
        });

    while let Some(request) = next_request(&shared) {
        let is_cancelled = || is_stale(&shared, request.id);
        let completion = match (&model, &mut context) {
            (Ok(model), Ok(context)) => complete(
                model,
                context,
                &request.input,
                request.sample_seed,
                candidate_limit,
                &is_cancelled,
            ),
            (Err(error), _) => Err(error.clone()),
            (_, Err(error)) => Err(error.clone()),
        };

        if !is_cancelled() {
            let completions = completion.map(|completions| completions.unwrap_or_default());
            (request.handler)(T2Result {
                request_id: request.id,
                input: request.input,
                completions,
            });
        }
    }
}

fn next_request(shared: &Shared) -> Option<Request> {
    let mut state = shared.state.lock().ok()?;
    loop {
        if state.stopping {
            return None;
        }
        if let Some(mut request) = state.pending.take() {
            let mut deadline = Instant::now() + DEBOUNCE;
            loop {
                if state.stopping {
                    return None;
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Some(request);
                }
                let (next_state, timeout) = shared.changed.wait_timeout(state, remaining).ok()?;
                state = next_state;
                if let Some(newer) = state.pending.take() {
                    request = newer;
                    deadline = Instant::now() + DEBOUNCE;
                } else if timeout.timed_out() {
                    return Some(request);
                }
            }
        }
        state = shared.changed.wait(state).ok()?;
    }
}

fn is_stale(shared: &Shared, request_id: u64) -> bool {
    shared.state.lock().map_or(true, |state| {
        state.stopping || state.request_id != request_id
    })
}

fn complete(
    model: &LlamaModel,
    context: &mut llama_cpp_2::context::LlamaContext<'_>,
    input: &str,
    sample_seed: u32,
    candidate_limit: usize,
    is_cancelled: &impl Fn() -> bool,
) -> Result<Option<Vec<String>>, String> {
    let template = model
        .chat_template(None)
        .map_err(|error| format!("model chat template unavailable: {error}"))?;
    let has_thinking_option = template
        .to_string()
        .map_err(|error| error.to_string())?
        .contains("enable_thinking");
    let messages = [
        LlamaChatMessage::new("system".to_owned(), T2_SYSTEM_PROMPT.to_owned())
            .map_err(|error| error.to_string())?,
        LlamaChatMessage::new("user".to_owned(), t2_user_prompt(input))
            .map_err(|error| error.to_string())?,
    ];
    let prompt = model
        .apply_chat_template(&template, &messages, true)
        .map_err(|error| error.to_string())?;
    // llama.cpp's Rust chat-template API cannot pass Qwen's `enable_thinking`
    // template argument. Close the template's open thinking block so command
    // completion starts directly, without spending the token budget on analysis.
    let prompt = if has_thinking_option {
        if let Some(prefix) = prompt.strip_suffix("<think>\n") {
            format!("{prefix}<think>\n\n</think>\n\n")
        } else if prompt.ends_with("</think>\n\n") {
            prompt
        } else {
            format!("{prompt}<think>\n\n</think>\n\n")
        }
    } else {
        prompt
    };
    let tokens = model
        .str_to_token(&prompt, AddBos::Never)
        .map_err(|error| error.to_string())?;
    if tokens.len() > MAX_PROMPT_TOKENS {
        return Ok(None);
    }

    let mut completions = Vec::with_capacity(candidate_limit);
    for candidate_index in 0..candidate_limit {
        if is_cancelled() {
            return Ok(None);
        }

        context.clear_kv_cache();
        let mut batch = LlamaBatch::new(tokens.len().max(1), 1);
        batch
            .add_sequence(&tokens, 0, false)
            .map_err(|error| error.to_string())?;
        context
            .decode(&mut batch)
            .map_err(|error| error.to_string())?;

        let mut sampler = if candidate_index == 0 {
            LlamaSampler::greedy()
        } else {
            let seed = sample_seed.wrapping_add((candidate_index as u32).wrapping_mul(0x9E37_79B9));
            LlamaSampler::chain_simple([
                LlamaSampler::top_k(40),
                LlamaSampler::temp(0.8),
                LlamaSampler::dist(seed),
            ])
        };
        let mut decoder = encoding_rs::UTF_8.new_decoder();
        let mut output = String::new();
        let mut position = i32::try_from(tokens.len()).map_err(|error| error.to_string())?;
        for _ in 0..MAX_OUTPUT_TOKENS {
            if is_cancelled() {
                return Ok(None);
            }
            let token = sampler.sample(context, -1);
            if model.is_eog_token(token) {
                break;
            }
            let piece = model
                .token_to_piece(token, &mut decoder, false, None)
                .map_err(|error| error.to_string())?;
            if piece.contains('\n') || piece.contains('\r') {
                break;
            }
            output.push_str(&piece);

            batch.clear();
            batch
                .add(token, position, &[0], true)
                .map_err(|error| error.to_string())?;
            context
                .decode(&mut batch)
                .map_err(|error| error.to_string())?;
            position += 1;
        }

        if let Some(completion) = normalize_completion(input, &output) {
            if !completions.contains(&completion) {
                completions.push(completion);
            }
        }
    }

    Ok((!completions.is_empty()).then_some(completions))
}

fn normalize_completion(input: &str, output: &str) -> Option<String> {
    let line = output.lines().next()?;
    if line.contains("<think>") || line.contains("</think>") {
        return None;
    }
    let line = line
        .strip_prefix("Append:")
        .map(|line| line.strip_prefix(' ').unwrap_or(line))
        .unwrap_or(line);
    let suffix = line.strip_prefix(input).unwrap_or(line);
    if suffix.is_empty()
        || suffix.trim().is_empty()
        || suffix.chars().count() > MAX_COMPLETION_CHARS
        || suffix.chars().any(char::is_control)
        || suffix.contains('\u{FFFD}')
    {
        return None;
    }
    Some(suffix.to_owned())
}

/// Whether a candidate GGUF file exists at this path.
pub fn model_file_exists(path: &Path) -> bool {
    path.is_file()
}
