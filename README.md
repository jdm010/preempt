# Preempt

Preempt is a terminal prototype that suggests shell commands as you type. It
uses local shell history and can optionally add suggestions from a local model.
The terminal is built from a modified Alacritty source tree.

This is a prototype, not a stable release. There are no packaged installers or
release binaries yet. The repository contains a Rust workspace, an integrated
Alacritty source tree, and a history-based prediction CLI.

## What works today

- **T0:** prefix completion ranked by history frequency and recency.
- **T1:** token n-gram fallback when no full-command prefix matches.
- **T2:** optional Qwen3.5-0.8B inference through llama.cpp. The model file is
  not included, and T2 runs only when T0/T1 has no candidate. In a paired
  128-input replay, the fast-miss policy made 47 T2 requests, kept 9/128
  top-1 exact hits and 9/128 exact cycleable candidates, and combined
  availability was 125/128.
  With the shorter prompt, warm latency was 401 ms median to the first
  candidate and 592 ms to the full two-candidate set, still above the 150 ms
  target. See [MODEL.md](MODEL.md) and the
  [evaluation report](training/REPORT.md).
- **Safety hints:** heuristic risk labels for common destructive commands.
  These hints do not parse or block commands.
- **Local history storage:** SQLCipher-backed history and feedback storage,
  with its key stored in the operating system's credential store.
- **Alacritty overlay:** the modified terminal displays ghost text, accepts
  suggestions, lets you cycle through them, and can color risky ones. A zsh
  hook sends the current command line to the terminal.

There is no cloud prediction code yet.

## Try the history predictor

Install a stable Rust toolchain, CMake, and a C++ toolchain (the workspace
builds the optional llama.cpp backend), then run the workspace tests:

```sh
cargo test --workspace
```

To query your zsh history without launching the terminal UI:

```sh
cargo run -p preempt-predict --bin preempt-suggest -- \
  --file "$HOME/.zsh_history" --top 5 'git '
```

This command reads the history file you provide and prints matching
completions. Use a sample file if you do not want to use your own history.

To check or build the modified Alacritty application, use its nested workspace:

```sh
cargo check --manifest-path crates/core/alacritty/Cargo.toml
cargo build --manifest-path crates/core/alacritty/Cargo.toml -p alacritty
```

On macOS 14, llama.cpp uses CPU by default; on macOS 15 or newer, Metal can be
enabled with `GGML_METAL=ON`. See [MODEL.md](MODEL.md) for model installation
details. The app does not download model weights at startup.

## Privacy and local data

The runtime reads local shell history, redacts common secrets, and encrypts the
history database. The redactor can miss secrets. Check the code and use sample
history if yours contains sensitive commands. Training uses temporary local
files, skips commands changed by redaction, and does not upload data. The
repository ignores model weights, databases, and training output.

Preempt keeps the old `auto-terminal` data folder and credential-store entry so
existing model files and encrypted history remain available. Use
`PREEMPT_GGUF` to set a model path; `AUTO_TERMINAL_GGUF` still works.

## Project notes

- [PLAN.md](PLAN.md) describes the broader roadmap and current implementation
  status.
- [MODEL.md](MODEL.md) documents the optional local model and pinned download.
- [training/README.md](training/README.md) explains the local training workflow.
- [training/REPORT.md](training/REPORT.md) records training and replay results.
- [crates/core/UPSTREAM.md](crates/core/UPSTREAM.md) records the vendored
  Alacritty and `vte` source revisions and retained license notices.

## License

The project's own crates are licensed under Apache-2.0; see [LICENSE](LICENSE).
Vendored Alacritty and `vte` source retain their upstream license notices.
Downloaded model weights are distributed under their own license.
