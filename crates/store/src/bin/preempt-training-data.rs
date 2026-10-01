use std::collections::BTreeMap;
use std::error::Error;
use std::fs::{self, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

const MAX_PREFIXES_PER_COMMAND: usize = 8;
const TRAINING_SYSTEM_PROMPT: &str = preempt_predict::T2_SYSTEM_PROMPT;

struct Args {
    history: PathBuf,
    output_dir: PathBuf,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("preempt-training-data: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    #[cfg(windows)]
    return Err("private training export currently requires Unix file permissions".into());

    let args = parse_args()?;
    let history = preempt_predict::history::load(&args.history)?;
    let mut commands = BTreeMap::<String, u32>::new();
    let mut safe_history = Vec::new();
    let mut skipped = 0_usize;

    for entry in &history {
        match preempt_store::training_safe_command(&entry.command) {
            Some(command) => {
                let count = commands.entry(command.clone()).or_default();
                *count = count.saturating_add(1);
                safe_history.push(preempt_predict::history::HistoryEntry {
                    command,
                    timestamp: entry.timestamp,
                });
            }
            None => skipped += 1,
        }
    }

    let mut train = Vec::new();
    let mut valid = Vec::new();
    for (command, count) in &commands {
        let destination = if stable_hash(command) % 10 == 0 {
            &mut valid
        } else {
            &mut train
        };
        append_examples(destination, command, *count);
    }

    if train.is_empty() {
        return Err("no safe training examples were produced from shell history".into());
    }

    create_private_directory(&args.output_dir)?;
    write_jsonl(&args.output_dir.join("train.jsonl"), &train)?;
    if !valid.is_empty() {
        write_jsonl(&args.output_dir.join("valid.jsonl"), &valid)?;
    }
    write_safe_training_history(&args.output_dir.join("train-history.jsonl"), &safe_history)?;
    write_summary(
        &args.output_dir.join("summary.txt"),
        history.len(),
        commands.len(),
        skipped,
        train.len(),
        valid.len(),
    )?;

    println!("History entries: {}", history.len());
    println!("Unique redaction-safe commands: {}", commands.len());
    println!("Skipped entries: {skipped}");
    println!("Training examples: {}", train.len());
    println!("Validation examples: {}", valid.len());
    println!("Private dataset directory: {}", args.output_dir.display());
    Ok(())
}

fn parse_args() -> Result<Args, String> {
    let mut history = None;
    let mut output_dir = None;
    let mut args = std::env::args().skip(1);

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--history" => {
                history = Some(PathBuf::from(
                    args.next().ok_or("--history needs a file path")?,
                ));
            }
            "--output-dir" => {
                output_dir = Some(PathBuf::from(
                    args.next().ok_or("--output-dir needs a directory path")?,
                ));
            }
            "--help" | "-h" => {
                println!("usage: preempt-training-data [--history PATH] [--output-dir PATH]");
                println!("Defaults to the current zsh history and app data training directory.");
                std::process::exit(0);
            }
            _ => return Err(format!("unknown argument: {arg}")),
        }
    }

    Ok(Args {
        history: history.unwrap_or_else(preempt_predict::history::default_history_path),
        output_dir: output_dir.unwrap_or(default_output_dir()?),
    })
}

fn default_output_dir() -> Result<PathBuf, String> {
    #[cfg(target_os = "macos")]
    let app_data = std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join("Library/Application Support"));

    #[cfg(target_os = "windows")]
    let app_data = std::env::var_os("APPDATA").map(PathBuf::from);

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let app_data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")));

    app_data
        .map(|path| path.join("auto-terminal/training-data"))
        .ok_or_else(|| "could not find an application data directory".to_owned())
}

fn append_examples(dataset: &mut Vec<serde_json::Value>, command: &str, frequency: u32) {
    let char_offsets = command
        .char_indices()
        .map(|(offset, _)| offset)
        .collect::<Vec<_>>();
    let char_count = char_offsets.len();
    if char_count < 4 {
        return;
    }

    let available_positions = char_count - 2;
    let sample_count = MAX_PREFIXES_PER_COMMAND.min(available_positions);
    let repetition = (u32::BITS - frequency.max(1).leading_zeros()).min(3) as usize;
    let repetition = repetition.max(1);

    for sample_index in 0..sample_count {
        let char_index = if sample_count == 1 {
            2
        } else {
            2 + sample_index * (char_count - 3) / (sample_count - 1)
        };
        let byte_offset = char_offsets[char_index];
        let prefix = &command[..byte_offset];
        let completion = &command[byte_offset..];
        if prefix.trim().is_empty() || completion.trim().is_empty() {
            continue;
        }

        let user_prompt = preempt_predict::t2_user_prompt(prefix);
        for _ in 0..repetition {
            dataset.push(serde_json::json!({
                "messages": [
                    { "role": "system", "content": TRAINING_SYSTEM_PROMPT },
                    { "role": "user", "content": user_prompt },
                    { "role": "assistant", "content": completion },
                ]
            }));
        }
    }
}

fn stable_hash(value: &str) -> u64 {
    value
        .as_bytes()
        .iter()
        .fold(0xcbf29ce484222325, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
        })
}

fn create_private_directory(path: &Path) -> Result<(), Box<dyn Error>> {
    if path.exists() {
        return Err(format!(
            "refusing to overwrite existing dataset directory: {}",
            path.display()
        )
        .into());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::create_dir(path)?;
    #[cfg(unix)]
    fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
    Ok(())
}

fn write_jsonl(path: &Path, rows: &[serde_json::Value]) -> Result<(), Box<dyn Error>> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let file = options.open(path)?;
    let mut writer = BufWriter::new(file);
    for row in rows {
        serde_json::to_writer(&mut writer, row)?;
        writer.write_all(b"\n")?;
    }
    writer.flush()?;
    Ok(())
}

fn write_safe_training_history(
    path: &Path,
    entries: &[preempt_predict::history::HistoryEntry],
) -> Result<(), Box<dyn Error>> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut writer = BufWriter::new(options.open(path)?);
    for entry in entries {
        if stable_hash(&entry.command) % 10 != 0 {
            serde_json::to_writer(
                &mut writer,
                &serde_json::json!({
                    "command": entry.command,
                    "timestamp": entry.timestamp,
                }),
            )?;
            writer.write_all(b"\n")?;
        }
    }
    writer.flush()?;
    Ok(())
}

fn write_summary(
    path: &Path,
    history_entries: usize,
    unique_commands: usize,
    skipped_entries: usize,
    train_examples: usize,
    valid_examples: usize,
) -> Result<(), Box<dyn Error>> {
    let summary = format!(
        "source: parsed shell history\nhistory entries: {history_entries}\nunique redaction-safe commands: {unique_commands}\nskipped entries: {skipped_entries}\ntraining examples: {train_examples}\nvalidation examples: {valid_examples}\n"
    );
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    options.open(path)?.write_all(summary.as_bytes())?;
    Ok(())
}
