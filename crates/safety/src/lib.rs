//! Conservative, local command-risk hints for prediction UIs.
//!
//! This is a heuristic highlighter, not a shell parser or execution policy.
//! It never blocks a command and never retains the command text.

const MAX_COMMAND_BYTES: usize = 16 * 1024;
const MAX_SHELL_RECURSION: usize = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RiskLevel {
    Clear,
    Review,
    High,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RiskAssessment {
    pub level: RiskLevel,
    pub reason: Option<&'static str>,
}

impl RiskAssessment {
    const CLEAR: Self = Self {
        level: RiskLevel::Clear,
        reason: None,
    };

    const fn new(level: RiskLevel, reason: &'static str) -> Self {
        Self {
            level,
            reason: Some(reason),
        }
    }
}

/// Assess a command for common destructive shell operations.
///
/// `Review` and `High` are UI hints only. This intentionally favors surfacing
/// a possible risk over deciding whether a command is safe to execute.
pub fn assess(command: &str) -> RiskAssessment {
    assess_at_depth(command, 0)
}

fn assess_at_depth(command: &str, depth: usize) -> RiskAssessment {
    if command.len() > MAX_COMMAND_BYTES {
        return RiskAssessment::new(RiskLevel::Review, "command is too large to inspect");
    }

    let parsed = match tokenize(command) {
        Ok(parsed) => parsed,
        Err(()) => return RiskAssessment::new(RiskLevel::Review, "command has unfinished quoting"),
    };

    let mut assessment = RiskAssessment::CLEAR;
    for (index, segment) in parsed.iter().enumerate() {
        assessment = more_severe(assessment, assess_segment(&segment.words, depth));

        // A downloader whose output is sent through a shell executes remote
        // content directly. Restrict this rule to a single pipeline group.
        if segment.pipeline_group
            != parsed
                .get(index + 1)
                .map_or(usize::MAX, |next| next.pipeline_group)
        {
            continue;
        }
        if is_stdout_downloader(&segment.words)
            && parsed[index + 1..]
                .iter()
                .take_while(|next| next.pipeline_group == segment.pipeline_group)
                .any(|next| is_shell(&next.words))
        {
            assessment = more_severe(
                assessment,
                RiskAssessment::new(RiskLevel::High, "downloaded content is piped to a shell"),
            );
        }
    }
    assessment
}

#[derive(Debug)]
struct Segment {
    words: Vec<String>,
    pipeline_group: usize,
}

/// Split unquoted shell command separators and remove simple quoting. This is
/// deliberately a small lexical scanner: it does not expand variables, globs,
/// substitutions, aliases, functions, or shell-specific grammar.
fn tokenize(input: &str) -> Result<Vec<Segment>, ()> {
    let mut segments = Vec::new();
    let mut words = Vec::new();
    let mut word = String::new();
    let mut word_started = false;
    let mut quote = None;
    let mut escaped = false;
    let mut pipeline_group = 0;
    let mut chars = input.char_indices().peekable();

    while let Some((_, ch)) = chars.next() {
        if escaped {
            word.push(ch);
            word_started = true;
            escaped = false;
            continue;
        }

        if let Some(active_quote) = quote {
            if ch == active_quote {
                quote = None;
            } else if ch == '\\' && active_quote == '"' {
                escaped = true;
            } else {
                word.push(ch);
                word_started = true;
            }
            continue;
        }

        match ch {
            '\'' | '"' => {
                quote = Some(ch);
                word_started = true;
            }
            '\\' => escaped = true,
            '#' if !word_started => {
                // Comments end at the next newline, which also ends a command.
                for (_, comment_char) in chars.by_ref() {
                    if comment_char == '\n' {
                        break;
                    }
                }
                flush_segment(
                    &mut segments,
                    &mut words,
                    &mut word,
                    &mut word_started,
                    pipeline_group,
                );
                pipeline_group += 1;
            }
            c if c.is_whitespace() => {
                flush_word(&mut words, &mut word, &mut word_started);
                if c == '\n' || c == '\r' {
                    flush_segment(
                        &mut segments,
                        &mut words,
                        &mut word,
                        &mut word_started,
                        pipeline_group,
                    );
                    pipeline_group += 1;
                }
            }
            ';' => {
                flush_segment(
                    &mut segments,
                    &mut words,
                    &mut word,
                    &mut word_started,
                    pipeline_group,
                );
                pipeline_group += 1;
            }
            '|' => {
                flush_segment(
                    &mut segments,
                    &mut words,
                    &mut word,
                    &mut word_started,
                    pipeline_group,
                );
                if chars.peek().is_some_and(|(_, next)| *next == '|') {
                    chars.next();
                    pipeline_group += 1;
                } else {
                    // `|&` still pipes stdout into the next command.
                    if chars.peek().is_some_and(|(_, next)| *next == '&') {
                        chars.next();
                    }
                }
            }
            '(' | ')' => {
                flush_segment(
                    &mut segments,
                    &mut words,
                    &mut word,
                    &mut word_started,
                    pipeline_group,
                );
                pipeline_group += 1;
            }
            '&' if chars.peek().is_some_and(|(_, next)| *next == '&') => {
                flush_segment(
                    &mut segments,
                    &mut words,
                    &mut word,
                    &mut word_started,
                    pipeline_group,
                );
                chars.next();
                pipeline_group += 1;
            }
            '&' if !chars.peek().is_some_and(|(_, next)| *next == '>') => {
                flush_segment(
                    &mut segments,
                    &mut words,
                    &mut word,
                    &mut word_started,
                    pipeline_group,
                );
                pipeline_group += 1;
            }
            _ => {
                word.push(ch);
                word_started = true;
            }
        }
    }

    if quote.is_some() || escaped {
        return Err(());
    }
    flush_segment(
        &mut segments,
        &mut words,
        &mut word,
        &mut word_started,
        pipeline_group,
    );
    Ok(segments)
}

fn flush_word(words: &mut Vec<String>, word: &mut String, word_started: &mut bool) {
    if *word_started {
        words.push(std::mem::take(word));
        *word_started = false;
    }
}

fn flush_segment(
    segments: &mut Vec<Segment>,
    words: &mut Vec<String>,
    word: &mut String,
    word_started: &mut bool,
    pipeline_group: usize,
) {
    flush_word(words, word, word_started);
    if !words.is_empty() {
        segments.push(Segment {
            words: std::mem::take(words),
            pipeline_group,
        });
    }
}

fn assess_segment(words: &[String], depth: usize) -> RiskAssessment {
    let words = command_words(words);
    let Some(executable) = words.first() else {
        return RiskAssessment::CLEAR;
    };
    let name = basename(executable);

    if is_shell_name(name) {
        if let Some(script) = shell_command_argument(words) {
            if depth >= MAX_SHELL_RECURSION {
                return RiskAssessment::new(
                    RiskLevel::Review,
                    "nested shell command could not be inspected",
                );
            }
            return assess_at_depth(script, depth + 1);
        }
    }

    if name.eq_ignore_ascii_case("rm") {
        return assess_rm(&words[1..]);
    }

    if name.eq_ignore_ascii_case("find") {
        if words.iter().any(|word| word == "-delete") {
            return RiskAssessment::new(RiskLevel::High, "find deletes matching files");
        }
        if has_exec_rm(&words) {
            return RiskAssessment::new(RiskLevel::High, "find runs recursive file removal");
        }
    }

    if name.eq_ignore_ascii_case("dd")
        && words.iter().any(|word| {
            word.strip_prefix("of=")
                .is_some_and(|target| target.starts_with("/dev/") && target != "/dev/null")
        })
    {
        return RiskAssessment::new(RiskLevel::High, "dd writes directly to a device");
    }

    if name.eq_ignore_ascii_case("mkfs") || name.to_ascii_lowercase().starts_with("mkfs.") {
        return RiskAssessment::new(RiskLevel::High, "formats a filesystem");
    }
    if ["wipefs", "shred"]
        .iter()
        .any(|candidate| name.eq_ignore_ascii_case(candidate))
    {
        return RiskAssessment::new(RiskLevel::High, "erases data from a device or file");
    }

    if name.eq_ignore_ascii_case("diskutil")
        && words.iter().skip(1).any(|word| {
            word.eq_ignore_ascii_case("eraseDisk") || word.eq_ignore_ascii_case("partitionDisk")
        })
    {
        return RiskAssessment::new(RiskLevel::High, "erases or repartitions a disk");
    }

    if name.eq_ignore_ascii_case("git") {
        return assess_git(&words[1..]);
    }

    if (name.eq_ignore_ascii_case("chmod") || name.eq_ignore_ascii_case("chown"))
        && has_recursive_flag(&words[1..])
    {
        let reason = if words.iter().any(|word| is_root_or_home(word)) {
            "recursive permission change targets a root or home path"
        } else {
            "recursive permission or ownership change"
        };
        let level = if words.iter().any(|word| is_root_or_home(word)) {
            RiskLevel::High
        } else {
            RiskLevel::Review
        };
        return RiskAssessment::new(level, reason);
    }

    if name.eq_ignore_ascii_case("terraform") && words.iter().skip(1).any(|word| word == "destroy")
    {
        return RiskAssessment::new(RiskLevel::High, "terraform destroys managed infrastructure");
    }
    if name.eq_ignore_ascii_case("kubectl") && words.iter().skip(1).any(|word| word == "delete") {
        return RiskAssessment::new(RiskLevel::Review, "kubectl deletes cluster resources");
    }

    RiskAssessment::CLEAR
}

fn assess_rm(args: &[String]) -> RiskAssessment {
    let mut recursive = false;
    let mut forced = false;
    let mut options = true;
    let mut targets = Vec::new();

    for arg in args {
        if options && arg == "--" {
            options = false;
        } else if options && arg.starts_with("--") {
            match arg.as_str() {
                "--recursive" => recursive = true,
                "--force" => forced = true,
                _ => (),
            }
        } else if options && arg.starts_with('-') && arg.len() > 1 {
            recursive |= arg[1..].contains('r') || arg[1..].contains('R');
            forced |= arg[1..].contains('f');
        } else {
            targets.push(arg.as_str());
        }
    }

    if targets.is_empty() {
        return RiskAssessment::CLEAR;
    }
    if targets.iter().any(|target| is_root_or_home(target)) {
        return RiskAssessment::new(RiskLevel::High, "file removal targets a root or home path");
    }
    if recursive && forced {
        return RiskAssessment::new(RiskLevel::High, "forced recursive file removal");
    }
    if recursive {
        return RiskAssessment::new(RiskLevel::Review, "recursive file removal");
    }
    RiskAssessment::new(RiskLevel::Review, "file removal")
}

fn assess_git(args: &[String]) -> RiskAssessment {
    if args.first().is_some_and(|arg| arg == "reset") && args.iter().any(|arg| arg == "--hard") {
        return RiskAssessment::new(RiskLevel::High, "git reset discards tracked changes");
    }
    if args.first().is_some_and(|arg| arg == "clean")
        && args
            .iter()
            .skip(1)
            .any(|arg| arg.starts_with('-') && arg.contains('f'))
    {
        return RiskAssessment::new(RiskLevel::Review, "git clean removes untracked files");
    }
    if args.first().is_some_and(|arg| arg == "push")
        && args.iter().skip(1).any(|arg| {
            arg == "--force"
                || arg.starts_with("--force-with-lease")
                || arg == "-f"
                || (arg.starts_with('-') && arg.len() > 2 && arg[1..].contains('f'))
                || arg.starts_with('+')
        })
    {
        return RiskAssessment::new(RiskLevel::Review, "git push rewrites remote history");
    }
    if (args.first().is_some_and(|arg| arg == "checkout") && args.iter().any(|arg| arg == "--"))
        || (args.first().is_some_and(|arg| arg == "restore")
            && (!args.iter().any(|arg| arg == "--staged")
                || args.iter().any(|arg| arg == "--worktree")))
    {
        return RiskAssessment::new(
            RiskLevel::Review,
            "git command may discard working-tree changes",
        );
    }
    RiskAssessment::CLEAR
}

fn has_exec_rm(words: &[String]) -> bool {
    let Some(exec_at) = words
        .iter()
        .position(|word| word == "-exec" || word == "-execdir")
    else {
        return false;
    };
    let Some(end) = words[exec_at + 1..]
        .iter()
        .position(|word| word == ";" || word == "+")
        .map(|offset| exec_at + 1 + offset)
    else {
        return false;
    };
    let args = &words[exec_at + 1..end];
    command_words(args)
        .first()
        .is_some_and(|executable| basename(executable).eq_ignore_ascii_case("rm"))
        && args
            .iter()
            .any(|arg| arg.starts_with('-') && (arg.contains('r') || arg.contains('R')))
}

fn has_recursive_flag(args: &[String]) -> bool {
    args.iter().any(|arg| {
        arg == "--recursive"
            || (arg.starts_with('-')
                && arg.len() > 1
                && (arg[1..].contains('r') || arg[1..].contains('R')))
    })
}

fn is_root_or_home(path: &str) -> bool {
    matches!(
        path,
        "/" | "/*" | "~" | "~/" | "$HOME" | "${HOME}" | "$HOME/" | "${HOME}/"
    ) || path.starts_with("~/")
        || path.starts_with("$HOME/")
        || path.starts_with("${HOME}/")
        || ["/home/", "/Users/", "/root/"]
            .iter()
            .any(|prefix| path.starts_with(prefix))
        || matches!(path, "/home" | "/Users" | "/root")
}

fn is_stdout_downloader(words: &[String]) -> bool {
    let words = command_words(words);
    let Some(executable) = words.first() else {
        return false;
    };
    let name = basename(executable);
    if name.eq_ignore_ascii_case("curl") {
        let mut index = 1;
        while let Some(arg) = words.get(index) {
            if arg == "-o" || arg == "--output" {
                return words.get(index + 1).is_some_and(|target| target == "-");
            }
            if let Some(target) = arg.strip_prefix("--output=") {
                return target == "-";
            }
            if arg == "-O" || arg == "--remote-name" || arg == "--remote-name-all" {
                return false;
            }
            if arg.starts_with('-') && !arg.starts_with("--") {
                if let Some(output_option) = arg.find('o') {
                    let target = &arg[output_option + 1..];
                    return if target.is_empty() {
                        words.get(index + 1).is_some_and(|target| target == "-")
                    } else {
                        target == "-"
                    };
                }
                if arg[1..].contains('O') {
                    return false;
                }
            }
            index += 1;
        }
        return true;
    }
    if name.eq_ignore_ascii_case("wget") {
        let mut index = 1;
        while let Some(arg) = words.get(index) {
            if arg == "-O" || arg == "--output-document" {
                return words.get(index + 1).is_some_and(|target| target == "-");
            }
            if let Some(target) = arg.strip_prefix("--output-document=") {
                return target == "-";
            }
            if arg.starts_with('-') && !arg.starts_with("--") {
                if let Some(output_option) = arg.find('O') {
                    let target = &arg[output_option + 1..];
                    return if target.is_empty() {
                        words.get(index + 1).is_some_and(|target| target == "-")
                    } else {
                        target == "-"
                    };
                }
            }
            index += 1;
        }
    }
    false
}

fn is_shell(words: &[String]) -> bool {
    let words = command_words(words);
    words
        .first()
        .is_some_and(|executable| is_shell_name(basename(executable)))
}

fn is_shell_name(name: &str) -> bool {
    ["sh", "bash", "zsh", "dash", "ash", "ksh", "fish"]
        .iter()
        .any(|shell| name.eq_ignore_ascii_case(shell))
}

fn shell_command_argument(words: &[String]) -> Option<&str> {
    for (index, arg) in words.iter().enumerate().skip(1) {
        if arg == "--command" || arg == "--command=" {
            return words.get(index + 1).map(String::as_str);
        }
        if let Some(script) = arg.strip_prefix("--command=") {
            return Some(script);
        }
        if arg == "-c" || (arg.starts_with('-') && !arg.starts_with("--") && arg[1..].contains('c'))
        {
            return words.get(index + 1).map(String::as_str);
        }
    }
    None
}

/// Peel common command wrappers and shell reserved words from the front.
fn command_words(words: &[String]) -> &[String] {
    let mut offset = 0;
    loop {
        let Some(word) = words.get(offset) else {
            return &words[offset..];
        };
        let name = basename(word);
        if [
            "then", "else", "elif", "if", "while", "until", "do", "!", "exec", "{",
        ]
        .iter()
        .any(|keyword| name == *keyword)
        {
            offset += 1;
            continue;
        }
        match name {
            "sudo" | "doas" => {
                offset += 1;
                while let Some(arg) = words.get(offset) {
                    if arg == "--" {
                        offset += 1;
                        break;
                    }
                    if !arg.starts_with('-') {
                        break;
                    }
                    let takes_value = ["-u", "-g", "-h", "-p", "-C", "-D", "--user", "--group"]
                        .iter()
                        .any(|option| arg == option);
                    offset += 1;
                    if takes_value {
                        offset = (offset + 1).min(words.len());
                    }
                }
            }
            "env" => {
                offset += 1;
                while let Some(arg) = words.get(offset) {
                    if arg == "--" {
                        offset += 1;
                        break;
                    }
                    if arg.starts_with('-') {
                        let takes_value =
                            ["-u", "--unset", "-C", "--chdir", "-S", "--split-string"]
                                .iter()
                                .any(|option| arg == option);
                        offset += 1;
                        if takes_value {
                            offset = (offset + 1).min(words.len());
                        }
                    } else if arg.contains('=') && !arg.starts_with('/') {
                        offset += 1;
                    } else {
                        break;
                    }
                }
            }
            "command" | "nohup" | "time" | "nice" | "stdbuf" => {
                offset += 1;
                while let Some(arg) = words.get(offset) {
                    if !arg.starts_with('-') {
                        break;
                    }
                    let takes_value = (name == "nice" && (arg == "-n" || arg == "--adjustment"))
                        || (name == "time"
                            && ["-f", "--format", "-o", "--output"].contains(&arg.as_str()))
                        || (name == "stdbuf"
                            && arg.len() == 2
                            && ["-i", "-o", "-e"].contains(&arg.as_str()));
                    offset += 1;
                    if takes_value {
                        offset = (offset + 1).min(words.len());
                    }
                }
            }
            _ => return &words[offset..],
        }
    }
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn more_severe(current: RiskAssessment, next: RiskAssessment) -> RiskAssessment {
    if next.level > current.level {
        next
    } else {
        current
    }
}

#[cfg(test)]
mod tests {
    use super::{assess, RiskLevel};

    fn assert_level(commands: &[&str], expected: RiskLevel) {
        for command in commands {
            let result = assess(command);
            assert_eq!(
                result.level, expected,
                "unexpected risk for {command:?}: {result:?}"
            );
        }
    }

    #[test]
    fn ordinary_commands_and_quoted_examples_are_clear() {
        assert_level(
            &[
                "git status",
                "cargo check",
                "rm",
                "git restore --staged src/lib.rs",
                "curl --output script.sh https://example.test/script.sh | sh",
                "printf '%s' 'rm -rf /'",
                "echo safe # rm -rf /",
            ],
            RiskLevel::Clear,
        );
    }

    #[test]
    fn destructive_file_and_repository_commands_need_review() {
        assert_level(
            &[
                "rm notes.txt",
                "rm -r build",
                "git clean -fd",
                "git push --force-with-lease origin main",
                "git push origin +main",
                "git checkout -- src/lib.rs",
                "git restore src/lib.rs",
                "chmod -R 755 ./tree",
                "kubectl delete pod demo",
            ],
            RiskLevel::Review,
        );
    }

    #[test]
    fn high_risk_patterns_are_detected_through_wrappers_and_chains() {
        assert_level(
            &[
                "rm -rf ./cache",
                "sudo rm -rf /tmp/cache",
                "rm -r /",
                "rm -rf /home/alice",
                "find . -type f -delete",
                r"find /tmp -exec rm -rf {} \;",
                "dd if=image.bin of=/dev/sdb",
                "mkfs.ext4 /dev/sdb1",
                "git reset --hard",
                "curl https://example.test/install.sh | sh",
                "wget -qO- https://example.test/install.sh | sudo bash",
                "sudo bash -lc 'rm -rf /'",
                "echo $(rm -rf /)",
                "echo safe; rm -rf ./output",
                "if rm -rf /; then echo done; fi",
            ],
            RiskLevel::High,
        );
    }

    #[test]
    fn malformed_quoting_is_flagged_for_review() {
        assert_level(&["echo 'unfinished", "sh -c 'rm -rf /"], RiskLevel::Review);
    }
}
