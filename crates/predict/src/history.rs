use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq)]
pub struct HistoryEntry {
    pub command: String,
    pub timestamp: Option<i64>,
}

pub fn parse_zsh(text: &str) -> Vec<HistoryEntry> {
    let mut entries = Vec::new();
    let mut pending: Option<(Option<i64>, String)> = None;

    for line in text.lines() {
        let (timestamp, command_line) = match split_extended(line) {
            Some((ts, cmd)) => (Some(ts), cmd.to_string()),
            None => (None, line.to_string()),
        };

        if command_line.ends_with('\\') {
            let mut joined = command_line;
            joined.pop();
            joined.push('\n');
            pending = match pending {
                Some((ts, mut acc)) => {
                    acc.push_str(&joined);
                    Some((ts, acc))
                }
                None => Some((timestamp, joined)),
            };
            continue;
        }

        match pending.take() {
            Some((ts, mut acc)) => {
                acc.push_str(&command_line);
                entries.push(HistoryEntry { command: acc, timestamp: ts });
            }
            None => {
                if !command_line.trim().is_empty() {
                    entries.push(HistoryEntry { command: command_line, timestamp });
                }
            }
        }
    }

    if let Some((ts, acc)) = pending {
        let trimmed = acc.trim();
        if !trimmed.is_empty() {
            entries.push(HistoryEntry { command: trimmed.to_string(), timestamp: ts });
        }
    }

    entries
}

fn split_extended(line: &str) -> Option<(i64, &str)> {
    let rest = line.strip_prefix(": ")?;
    let (ts, rest) = rest.split_once(':')?;
    let (_duration, cmd) = rest.split_once(';')?;
    let ts: i64 = ts.parse().ok()?;
    Some((ts, cmd))
}

pub fn default_history_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".zsh_history")
}

pub fn load(path: &PathBuf) -> std::io::Result<Vec<HistoryEntry>> {
    let bytes = std::fs::read(path)?;
    let text = String::from_utf8_lossy(&bytes);
    Ok(parse_zsh(&text)
        .into_iter()
        // A replacement character means the command could not be decoded
        // faithfully. Never offer a corrupted command as an executable suggestion.
        .filter(|entry| !entry.command.contains('\u{FFFD}'))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_extended_entries() {
        let text = ": 1700000000:0;git status\n: 1700000100:5;cargo build --release\n";
        let entries = parse_zsh(text);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].command, "git status");
        assert_eq!(entries[0].timestamp, Some(1700000000));
        assert_eq!(entries[1].command, "cargo build --release");
    }

    #[test]
    fn parses_plain_entries() {
        let text = "ls -la\necho hello\n";
        let entries = parse_zsh(text);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].command, "ls -la");
        assert_eq!(entries[0].timestamp, None);
    }

    #[test]
    fn joins_multiline_continuations() {
        let text = ": 1700000000:0;echo one \\\ntwo \\\nthree\nls\n";
        let entries = parse_zsh(text);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].command, "echo one \ntwo \nthree");
        assert_eq!(entries[1].command, "ls");
    }

    #[test]
    fn skips_empty_lines() {
        let text = "\n   \nls\n";
        let entries = parse_zsh(text);
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn handles_missing_history_file() {
        let err = load(&PathBuf::from("/nonexistent/history")).is_err();
        assert!(err);
    }

    #[test]
    fn skips_undecodable_commands_but_keeps_valid_history_entries() {
        let path = std::env::temp_dir().join(format!(
            "at-predict-history-{}.zsh_history",
            std::process::id()
        ));
        std::fs::write(&path, b"echo before\necho bad \xff command\necho after\n").unwrap();

        let entries = load(&path).unwrap();

        std::fs::remove_file(path).unwrap();
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.command.as_str())
                .collect::<Vec<_>>(),
            ["echo before", "echo after"]
        );
    }
}
