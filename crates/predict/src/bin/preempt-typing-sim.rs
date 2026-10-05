//! Measure local T2 debounce and cancellation behavior with synthetic typing.

use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

use preempt_predict::t2::{default_model_path, T2Prefetcher, T2Result};

const SYNTHETIC_COMMAND: &str = "git status --short";
const KEY_INTERVALS_MS: &[u64] = &[50, 80, 120, 180];
const RESULT_TIMEOUT: Duration = Duration::from_secs(120);

fn main() {
    if let Err(error) = run() {
        eprintln!("preempt-typing-sim: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let model_path = default_model_path().ok_or("no model path configured")?;
    let prefetcher = T2Prefetcher::new(model_path)?;
    let (sender, receiver) = mpsc::channel::<T2Result>();

    let warm_handler = Arc::new({
        let sender = sender.clone();
        move |result| {
            let _ = sender.send(result);
        }
    });
    let mut latest_request =
        prefetcher.request_streaming(SYNTHETIC_COMMAND.to_owned(), warm_handler);
    wait_for_final(&receiver, latest_request)?;

    let chars: Vec<char> = SYNTHETIC_COMMAND.chars().collect();
    for interval_ms in KEY_INTERVALS_MS {
        eprintln!("typing simulation scenario: key_interval_ms={interval_ms}");
        let mut final_key_at = Instant::now();
        for end in 3..=chars.len() {
            prefetcher.cancel(latest_request);
            let input: String = chars[..end].iter().collect();
            let handler = Arc::new({
                let sender = sender.clone();
                move |result| {
                    let _ = sender.send(result);
                }
            });
            if end == chars.len() {
                final_key_at = Instant::now();
            }
            latest_request = prefetcher.request_streaming(input, handler);
            if end < chars.len() {
                thread::sleep(Duration::from_millis(*interval_ms));
            }
        }

        let mut first_candidate_ms = None;
        let mut intermediate_results = 0_usize;
        loop {
            let result = receiver.recv_timeout(RESULT_TIMEOUT)?;
            if result.request_id == latest_request {
                if first_candidate_ms.is_none()
                    && result
                        .completions
                        .as_ref()
                        .is_ok_and(|completions| !completions.is_empty())
                {
                    first_candidate_ms = Some(final_key_at.elapsed().as_millis());
                }
                if result.is_final {
                    break;
                }
            } else if result
                .completions
                .as_ref()
                .is_ok_and(|completions| !completions.is_empty())
            {
                intermediate_results += 1;
            }
        }

        println!(
            "key_interval_ms={interval_ms} input_updates={} final_first_candidate_ms={} intermediate_prefix_results={intermediate_results}",
            chars.len() - 2,
            first_candidate_ms
                .map_or_else(|| "none".to_owned(), |latency| latency.to_string()),
        );
    }

    Ok(())
}

fn wait_for_final(
    receiver: &mpsc::Receiver<T2Result>,
    request_id: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    loop {
        let result = receiver.recv_timeout(RESULT_TIMEOUT)?;
        if result.request_id == request_id && result.is_final {
            break;
        }
    }
    Ok(())
}
