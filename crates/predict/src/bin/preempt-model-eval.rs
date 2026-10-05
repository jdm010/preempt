use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use preempt_predict::engine::Engine;
use preempt_predict::history::HistoryEntry;
use preempt_predict::t2::{default_model_path, T2Prefetcher, T2Result};
use preempt_predict::{Suggestion, Tier};

const DEFAULT_PREFIXES: &[&str] = &[
    "git sta",
    "cargo bu",
    "docker ps --",
    "kubectl get po",
    "python -m pip ins",
    "systemctl sta",
];
const DEFAULT_DATA_LIMIT: usize = 128;
const DEFAULT_CANDIDATE_LIMIT: usize = 2;
const MAX_CANDIDATE_LIMIT: usize = 3;
const MAX_OVERLAY_CANDIDATES: usize = 5;
const RESULT_TIMEOUT: Duration = Duration::from_secs(120);
const PREFIX_START: &str = "<shell-command-prefix>\n";
const PREFIX_END: &str = "\n</shell-command-prefix>";

struct Example {
    prefix: String,
    expected_suffix: Option<String>,
}

struct Args {
    model_path: PathBuf,
    examples: Vec<Example>,
    aggregate_only: bool,
    candidate_limit: usize,
    cascade: bool,
    trigger_on_fast_miss: bool,
    training_history: Vec<HistoryEntry>,
}

fn main() {
    let args = match parse_args() {
        Ok(args) => args,
        Err(error) => {
            eprintln!("preempt-model-eval: {error}");
            std::process::exit(2);
        }
    };

    if !args.model_path.is_file() {
        eprintln!(
            "preempt-model-eval: model file not found: {}",
            args.model_path.display()
        );
        std::process::exit(1);
    }

    if args.aggregate_only {
        if args.cascade {
            println!("Scoring the local T0/T1/T2 cascade; command text is suppressed.");
        } else {
            println!("Scoring local holdout examples; command text is suppressed.");
        }
    } else {
        println!("model: {}", args.model_path.display());
        println!(
            "Each request includes the 100 ms debounce and generates up to {} candidates.",
            args.candidate_limit
        );
    }

    let prefetcher =
        match T2Prefetcher::with_candidate_limit(&args.model_path, args.candidate_limit) {
            Ok(prefetcher) => prefetcher,
            Err(error) => {
                eprintln!("preempt-model-eval: could not start model worker: {error}");
                std::process::exit(1);
            }
        };

    if args.cascade {
        let engine = Engine::build(&args.training_history, unix_now());
        let distinct_training_commands = args
            .training_history
            .iter()
            .map(|entry| entry.command.as_str())
            .collect::<HashSet<_>>()
            .len();
        score_cascade(
            &engine,
            &prefetcher,
            &args.examples,
            args.candidate_limit,
            args.trigger_on_fast_miss,
            args.training_history.len(),
            distinct_training_commands,
        );
        return;
    }

    let mut latencies = Vec::with_capacity(args.examples.len());
    let mut first_exact = 0_usize;
    let mut any_exact = 0_usize;
    let mut nonempty = 0_usize;
    let mut scoring_errors = 0_usize;

    for (index, example) in args.examples.iter().enumerate() {
        let (sender, receiver) = mpsc::channel::<T2Result>();
        let handler = Arc::new(move |result| {
            let _ = sender.send(result);
        });
        let started = Instant::now();
        prefetcher.request_with_seed(
            example.prefix.clone(),
            replay_sample_seed(&example.prefix),
            handler,
        );

        match receiver.recv_timeout(RESULT_TIMEOUT) {
            Ok(result) => {
                let elapsed = started.elapsed();
                latencies.push(elapsed);
                match result.completions {
                    Ok(completions) => {
                        if !completions.is_empty() {
                            nonempty += 1;
                        }
                        if let Some(expected) = &example.expected_suffix {
                            first_exact += usize::from(
                                completions.first().is_some_and(|item| item == expected),
                            );
                            any_exact +=
                                usize::from(completions.iter().any(|item| item == expected));
                        } else {
                            println!("\n{:>3} ms  {:?}", elapsed.as_millis(), example.prefix);
                            if completions.is_empty() {
                                println!("      (no completion)");
                            }
                            for (candidate_index, completion) in completions.iter().enumerate() {
                                println!(
                                    "  {}. {}{}",
                                    candidate_index + 1,
                                    example.prefix,
                                    completion
                                );
                            }
                        }
                    }
                    Err(error) => {
                        scoring_errors += 1;
                        if !args.aggregate_only {
                            eprintln!("\n{:>3} ms: {error}", elapsed.as_millis());
                        }
                    }
                }
            }
            Err(error) => {
                scoring_errors += 1;
                if !args.aggregate_only {
                    eprintln!(
                        "\n{:?}: timed out waiting for the model ({error})",
                        example.prefix
                    );
                }
            }
        }

        if !args.aggregate_only && index + 1 < args.examples.len() {
            println!("------------------------------------------------------------");
        }
    }

    if args.aggregate_only {
        println!("Examples scored: {}", args.examples.len());
        println!(
            "Top-1 exact suffixes: {first_exact}/{}",
            args.examples.len()
        );
        println!(
            "Exact suffixes among returned candidates: {any_exact}/{}",
            args.examples.len()
        );
        println!("Candidate limit: {}", args.candidate_limit);
        println!(
            "Requests with a candidate: {nonempty}/{}",
            args.examples.len()
        );
        println!("Scoring errors: {scoring_errors}");
    }

    print_latency_summary(&latencies, "Cold first request");
    if scoring_errors > 0 {
        std::process::exit(1);
    }
}

fn parse_args() -> Result<Args, String> {
    let mut model_path = None;
    let mut data_path = None;
    let mut limit = DEFAULT_DATA_LIMIT;
    let mut candidate_limit = DEFAULT_CANDIDATE_LIMIT;
    let mut cascade = false;
    let mut trigger_on_fast_miss = false;
    let mut prefixes = Vec::new();
    let mut args = std::env::args().skip(1);

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--model" => {
                model_path = Some(PathBuf::from(
                    args.next().ok_or("--model needs a file path")?,
                ));
            }
            "--data" => {
                data_path = Some(PathBuf::from(
                    args.next().ok_or("--data needs a directory")?,
                ));
            }
            "--limit" => {
                limit = args
                    .next()
                    .ok_or("--limit needs a positive integer")?
                    .parse()
                    .map_err(|_| "--limit needs a positive integer")?;
                if limit == 0 {
                    return Err("--limit must be greater than zero".to_owned());
                }
            }
            "--candidate-limit" => {
                candidate_limit = args
                    .next()
                    .ok_or("--candidate-limit needs an integer from 1 to 3")?
                    .parse()
                    .map_err(|_| "--candidate-limit needs an integer from 1 to 3")?;
                if !(1..=MAX_CANDIDATE_LIMIT).contains(&candidate_limit) {
                    return Err("--candidate-limit must be between 1 and 3".to_owned());
                }
            }
            "--cascade" => cascade = true,
            "--trigger-on-fast-miss" => trigger_on_fast_miss = true,
            "--help" | "-h" => {
                println!("usage: preempt-model-eval [--model PATH] [PREFIX ...]");
                println!("       preempt-model-eval [--model PATH] --data DIR [--limit N] [--candidate-limit N]");
                println!("       preempt-model-eval [--model PATH] --data DIR --cascade [--limit N] [--candidate-limit N]");
                println!("       append --trigger-on-fast-miss to queue T2 only when T0/T1 has no candidate");
                println!("Without PREFIX values or --data, runs a small generic shell prompt set.");
                std::process::exit(0);
            }
            _ => prefixes.push(arg),
        }
    }

    let model_path = model_path
        .or_else(default_model_path)
        .ok_or("no model path; set PREEMPT_GGUF (AUTO_TERMINAL_GGUF is also supported) or pass --model PATH")?;
    if cascade && data_path.is_none() {
        return Err("--cascade requires --data DIR".to_owned());
    }
    if trigger_on_fast_miss && !cascade {
        return Err("--trigger-on-fast-miss requires --cascade".to_owned());
    }
    let (examples, aggregate_only) = if let Some(data_path) = data_path.as_ref() {
        if !prefixes.is_empty() {
            return Err("positional PREFIX values cannot be combined with --data".to_owned());
        }
        (
            read_holdout_examples(&data_path.join("valid.jsonl"), limit)?,
            true,
        )
    } else {
        if prefixes.is_empty() {
            prefixes = DEFAULT_PREFIXES
                .iter()
                .map(|prefix| (*prefix).to_owned())
                .collect();
        }
        (
            prefixes
                .into_iter()
                .map(|prefix| Example {
                    prefix,
                    expected_suffix: None,
                })
                .collect(),
            false,
        )
    };
    let training_history = if cascade {
        read_training_history(
            &data_path
                .as_ref()
                .expect("cascade requires data path")
                .join("train-history.jsonl"),
        )?
    } else {
        Vec::new()
    };

    Ok(Args {
        model_path,
        examples,
        aggregate_only,
        candidate_limit,
        cascade,
        trigger_on_fast_miss,
        training_history,
    })
}

fn read_holdout_examples(path: &PathBuf, limit: usize) -> Result<Vec<Example>, String> {
    let file = File::open(path)
        .map_err(|error| format!("could not open holdout data {}: {error}", path.display()))?;
    let mut examples = Vec::new();
    let mut seen = HashSet::new();

    for line in BufReader::new(file).lines() {
        let line = line.map_err(|error| format!("could not read holdout data: {error}"))?;
        let row: serde_json::Value = serde_json::from_str(&line)
            .map_err(|error| format!("holdout data contains invalid JSON: {error}"))?;
        let (prefix, target) = parse_chat_example(&row)?;
        if seen.insert((prefix.to_owned(), target.to_owned())) {
            examples.push(Example {
                prefix,
                expected_suffix: Some(target),
            });
        }
    }

    if examples.is_empty() {
        return Err("holdout data has no examples".to_owned());
    }
    if examples.len() <= limit {
        return Ok(examples);
    }
    if limit == 1 {
        examples.truncate(1);
        return Ok(examples);
    }

    let last_index = examples.len() - 1;
    Ok((0..limit)
        .map(|index| {
            let selected = index * last_index / (limit - 1);
            Example {
                prefix: examples[selected].prefix.clone(),
                expected_suffix: examples[selected].expected_suffix.clone(),
            }
        })
        .collect())
}

fn read_training_history(path: &PathBuf) -> Result<Vec<HistoryEntry>, String> {
    let file = File::open(path)
        .map_err(|error| format!("could not open training data {}: {error}", path.display()))?;
    let mut entries = Vec::new();

    for line in BufReader::new(file).lines() {
        let line = line.map_err(|error| format!("could not read training data: {error}"))?;
        let row: serde_json::Value = serde_json::from_str(&line)
            .map_err(|error| format!("training row contains invalid JSON: {error}"))?;
        let command = row["command"]
            .as_str()
            .ok_or("training history row has no command")?;
        let timestamp = row["timestamp"].as_i64();
        entries.push(HistoryEntry {
            command: command.to_owned(),
            timestamp,
        });
    }

    if entries.is_empty() {
        return Err("training data has no command examples".to_owned());
    }

    Ok(entries)
}

fn parse_chat_example(row: &serde_json::Value) -> Result<(String, String), String> {
    let messages = row["messages"]
        .as_array()
        .ok_or("chat row is missing its messages array")?;
    if messages.len() < 3 {
        return Err("chat row has fewer than three messages".to_owned());
    }
    let user = messages[messages.len() - 2]["content"]
        .as_str()
        .ok_or("chat row has no user prompt text")?;
    let target = messages[messages.len() - 1]["content"]
        .as_str()
        .ok_or("chat row has no target completion text")?;
    let prefix = user
        .split_once(PREFIX_START)
        .and_then(|(_, rest)| rest.split_once(PREFIX_END))
        .map(|(prefix, _)| prefix)
        .ok_or("chat row has an unrecognized command-prefix prompt")?;
    Ok((prefix.to_owned(), target.to_owned()))
}

fn score_cascade(
    engine: &Engine,
    prefetcher: &T2Prefetcher,
    examples: &[Example],
    candidate_limit: usize,
    trigger_on_fast_miss: bool,
    training_history_count: usize,
    distinct_training_commands: usize,
) {
    let mut fast_latencies = Vec::with_capacity(examples.len());
    let mut first_candidate_latencies = Vec::with_capacity(examples.len());
    let mut model_latencies = Vec::with_capacity(examples.len());
    let mut t0_requests = 0;
    let mut t1_requests = 0;
    let mut fast_coverage = 0;
    let mut fast_top1_exact = 0;
    let mut fast_any_exact = 0;
    let mut t2_coverage = 0;
    let mut t2_added = 0;
    let mut t2_top1_exact = 0;
    let mut t2_any_exact = 0;
    let mut t2_rescued = 0;
    let mut t2_calls = 0;
    let mut combined_top1_exact = 0;
    let mut combined_any_exact = 0;
    let mut combined_coverage = 0;
    let mut scoring_errors = 0;

    for example in examples {
        let started = Instant::now();
        let fast: Vec<Suggestion> = engine
            .suggest(&example.prefix, 64)
            .into_iter()
            .filter(|suggestion| {
                !suggestion.completion.is_empty()
                    && !suggestion.completion.chars().any(char::is_control)
            })
            .take(MAX_OVERLAY_CANDIDATES)
            .collect();
        fast_latencies.push(started.elapsed());

        if fast.iter().any(|item| item.tier == Tier::T0Prefix) {
            t0_requests += 1;
        } else if fast.iter().any(|item| item.tier == Tier::T1Ngram) {
            t1_requests += 1;
        }
        fast_coverage += usize::from(!fast.is_empty());

        let expected = example.expected_suffix.as_deref().unwrap_or_default();
        let fast_first_exact = fast.first().is_some_and(|item| item.completion == expected);
        let fast_exact = fast.iter().any(|item| item.completion == expected);
        fast_top1_exact += usize::from(fast_first_exact);
        fast_any_exact += usize::from(fast_exact);

        if trigger_on_fast_miss && !fast.is_empty() {
            combined_top1_exact += usize::from(fast_first_exact);
            combined_any_exact += usize::from(fast_exact);
            combined_coverage += 1;
            continue;
        }

        let (sender, receiver) = mpsc::channel::<T2Result>();
        let handler = Arc::new(move |result| {
            let _ = sender.send(result);
        });
        let started = Instant::now();
        t2_calls += 1;
        prefetcher.request_streaming_with_seed(
            example.prefix.clone(),
            replay_sample_seed(&example.prefix),
            handler,
        );

        let mut first_candidate_latency = None;
        let result = loop {
            match receiver.recv_timeout(RESULT_TIMEOUT) {
                Ok(result) => {
                    if result
                        .completions
                        .as_ref()
                        .is_ok_and(|completions| !completions.is_empty())
                    {
                        first_candidate_latency.get_or_insert_with(|| started.elapsed());
                    }
                    if result.is_final {
                        break Some(result.completions);
                    }
                }
                Err(_) => break None,
            }
        };
        if let Some(latency) = first_candidate_latency {
            first_candidate_latencies.push(latency);
        }

        match result {
            Some(result) => {
                model_latencies.push(started.elapsed());
                match result {
                    Ok(completions) => {
                        let usable: Vec<&String> = completions
                            .iter()
                            .filter(|item| !item.is_empty() && !item.chars().any(char::is_control))
                            .collect();
                        let added: Vec<&String> = usable
                            .iter()
                            .copied()
                            .filter(|item| {
                                let full_command = format!("{}{item}", example.prefix);
                                !fast
                                    .iter()
                                    .any(|suggestion| suggestion.full_command == full_command)
                            })
                            .collect();
                        let model_first_exact =
                            usable.first().is_some_and(|item| item.as_str() == expected);
                        let model_exact = usable.iter().any(|item| item.as_str() == expected);
                        let combined_first_exact = if fast.is_empty() {
                            model_first_exact
                        } else {
                            fast_first_exact
                        };
                        let union_exact = fast_exact || model_exact;
                        let has_combined_candidate = !fast.is_empty() || !usable.is_empty();

                        t2_coverage += usize::from(!usable.is_empty());
                        t2_added += usize::from(!added.is_empty());
                        t2_top1_exact += usize::from(model_first_exact);
                        t2_any_exact += usize::from(model_exact);
                        t2_rescued += usize::from(!fast_exact && model_exact);
                        combined_top1_exact += usize::from(combined_first_exact);
                        combined_any_exact += usize::from(union_exact);
                        combined_coverage += usize::from(has_combined_candidate);
                    }
                    Err(_) => {
                        scoring_errors += 1;
                        combined_top1_exact += usize::from(fast_first_exact);
                        combined_any_exact += usize::from(fast_exact);
                        combined_coverage += usize::from(!fast.is_empty());
                    }
                }
            }
            None => {
                scoring_errors += 1;
                combined_top1_exact += usize::from(fast_first_exact);
                combined_any_exact += usize::from(fast_exact);
                combined_coverage += usize::from(!fast.is_empty());
            }
        }
    }

    let count = examples.len();
    println!("Examples scored: {count}");
    println!("Safe training-history entries in T0/T1 index: {training_history_count}");
    println!("Distinct safe training commands: {distinct_training_commands}");
    println!("T0 exact-prefix source requests: {t0_requests}/{count}");
    println!("T1 n-gram fallback source requests: {t1_requests}/{count}");
    println!("T0/T1 requests with candidates: {fast_coverage}/{count}");
    println!("T0/T1 top-1 exact suffixes: {fast_top1_exact}/{count}");
    println!("T0/T1 exact suffix in up to 5 candidates: {fast_any_exact}/{count}");
    println!(
        "T2 trigger policy: {}",
        if trigger_on_fast_miss {
            "only when T0/T1 has no candidate"
        } else {
            "every request"
        }
    );
    println!("T2 second-candidate sampling: stable per replay example");
    println!("T2 requests issued: {t2_calls}/{count}");
    println!("T2 requests with a candidate: {t2_coverage}/{t2_calls}");
    println!("T2 added a new cycleable candidate: {t2_added}/{t2_calls}");
    println!("T2 top-1 exact suffixes: {t2_top1_exact}/{t2_calls}");
    println!("T2 exact suffix among candidates: {t2_any_exact}/{t2_calls}");
    println!("T2 exact suffix recovered after T0/T1 miss: {t2_rescued}/{count}");
    println!("Combined app top-1 exact suffixes: {combined_top1_exact}/{count}");
    println!("Combined T0/T1/T2 exact suffix in candidates: {combined_any_exact}/{count}");
    println!("T2 candidate limit: {candidate_limit}");
    println!("Scoring errors: {scoring_errors}");
    println!("Combined requests with a candidate: {combined_coverage}/{count}");
    print_micro_latency_summary("T0/T1 synchronous", &fast_latencies);
    print_latency_summary(&first_candidate_latencies, "Time to first T2 candidate");
    print_latency_summary(&model_latencies, "Full T2 candidate set");

    if scoring_errors > 0 {
        std::process::exit(1);
    }
}

/// Seed stochastic candidates from the inference input, independent of replay order.
/// The expected suffix is deliberately excluded to avoid leaking the label into sampling.
/// FNV-1a is used because its output is stable across Rust toolchain versions.
fn replay_sample_seed(input_prefix: &str) -> u32 {
    let mut hash = 0x811C_9DC5_u32;
    for byte in input_prefix.bytes() {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

fn print_micro_latency_summary(label: &str, latencies: &[Duration]) {
    if latencies.is_empty() {
        return;
    }
    let mut sorted = latencies.to_vec();
    sorted.sort_unstable();
    let median = sorted[sorted.len() / 2];
    let p95_index = (sorted.len() * 95).div_ceil(100).saturating_sub(1);
    println!(
        "{label} latency: median {} µs, p95 {} µs ({} samples)",
        median.as_micros(),
        sorted[p95_index].as_micros(),
        sorted.len()
    );
}

fn print_latency_summary(latencies: &[Duration], first_label: &str) {
    if let Some((cold, warm)) = latencies.split_first() {
        println!("{first_label}: {} ms", cold.as_millis());
        if !warm.is_empty() {
            let mut warm_sorted = warm.to_vec();
            warm_sorted.sort_unstable();
            let median = warm_sorted[warm_sorted.len() / 2];
            let p95_index = (warm_sorted.len() * 95).div_ceil(100).saturating_sub(1);
            println!(
                "Warm latency: median {} ms, p95 {} ms ({} samples)",
                median.as_millis(),
                warm_sorted[p95_index].as_millis(),
                warm_sorted.len()
            );
        }
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs() as i64)
}
