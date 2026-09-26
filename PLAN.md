# auto-terminal

A local-first predictive terminal. Warp's Next Command UX, but on-device: no account, no cloud dependency, no telemetry by default.

## Thesis

Every serious AI terminal in 2026 (Warp) requires an account and routes AI through hosted backends. The open lane is **local-first prediction**: same ghost-text UX, all models on-device, user history never leaves the machine.

## Architecture

```
┌─ Terminal core (Rust, GPU) ──────────────────────┐
│ PTY │ ANSI parser │ renderer │ input pipeline     │
│            └─ prediction overlay hook (ours)      │
├─ Prediction cascade ─────────────────────────────┤
│ T0 <5ms    trie prefix-match on history          │
│ T1 <30ms   frecency n-grams (cwd/git/exit-code)  │
│ T2 <150ms  on-device NL2Shell-class model (GGUF,│
│            llama.cpp, ~400MB, fine-tuned on user)│
│ T3 async   cloud LLM (opt-in): NL→cmd, fixes     │
├─ Systems ────────────────────────────────────────┤
│ • Speculative prefetch: pre-compute top-k while │
│   user types (ShellGames pattern)                │
│ • Safety gate: CARE-style static verifier (~2ms) │
│   flags destructive cmds before accept           │
│ • Learning loop: accept/reject/modify → ranking  │
│ • Privacy: secret redaction before anything      │
│   leaves device; encrypted history store          │
└──────────────────────────────────────────────────┘
```

### Decisions (2026 research)

- **Base: Alacritty 0.17 fork.** Clean MIT, fast, light. We own the prediction layer natively — no AGPL entanglement (Warp fork rejected), no plugin latency ceiling (WezTerm rejected).
- **Tier 2 model class: NL2Shell recipe** (Qwen3.5-0.8B, QLoRA, GGUF ~400MB, runs in llama.cpp). MIT-licensed precedent, edge-deployable.
- **Cascade validated by research:** ShellGames (arXiv 2606.17986) uses the same two-stage design — command-level 3-gram + lightweight LLM refinement — plus speculative prefetch of top-k predictions.
- **Safety gate pattern:** CARE (ISSRE 2026) — static-first command verification (~2ms, 85.6% F1), LLM judge only for ambiguous cases.

## Stack

| Layer | Tech |
|---|---|
| Core language | Rust |
| Terminal base | Alacritty 0.17 fork — winit, vte (ANSI parser), wgpu (GPU rendering) |
| PTY | portable-pty |
| T0/T1 predictors | Pure Rust: radix_trie (prefix match), in-house n-gram engine; tokio for async orchestration + prefetch |
| Tier 2 (on-device LLM) | llama.cpp via llama-cpp-2, GGUF sub-1B model, in-process |
| Tier 3 (cloud, opt-in) | reqwest, provider-agnostic keys (Anthropic/OpenAI/local Ollama), never a hard dependency |
| Safety gate | Rust port of CARE pattern — static AST analysis, shell-words parsing |
| History store | SQLite (SQLCipher-encrypted): command, cwd, git branch, exit code, timestamp |
| Training pipeline | Python (PyTorch + TRL/Unsloth QLoRA) — fine-tunes Tier 2 on user history, exports GGUF; not part of runtime |
| Config | TOML (Alacritty convention) + inline keybinding picker |
| Testing | criterion (micro), vtebench (terminal perf), replay-eval harness measuring accept-rate/top-k on real history |
| CI/Release | GitHub Actions, cargo-dist, macOS notarization, Sparkle-style auto-update |
| Telemetry | Local SQLite metrics only; opt-in crash reporting |

## UX bar (Warp conventions)

- Ghost text suggestion after cursor
- `→` / `Ctrl-F` accept, cycling through ranked candidates
- Inline keybinding picker to remap accept key
- Risk highlighting for destructive commands (safety gate)
- Every AI feature individually toggleable
- Debounced, never blocks input; stale suggestion > no suggestion is a bug

## Workspace layout

```
auto-terminal/
├── Cargo.toml            # workspace
├── crates/
│   ├── predict/          # cascade engine: T0 trie, T1 n-grams, orchestration, prefetch
│   ├── safety/           # command risk verification
│   ├── store/            # encrypted history DB, feature extraction, learning signals
│   └── core/             # Alacritty fork + prediction overlay hook (added P1)
├── training/             # Python: QLoRA fine-tune pipeline, GGUF export
└── PLAN.md
```

## Roadmap

- **P0 (wk 1):** Workspace scaffold; `predict` crate with T0 prefix index + history parser, tested end-to-end against real shell history. ✓ complete (2026-09-26)
- **P1 (wk 2–3):** Fork Alacritty into `crates/core`; wire prediction overlay into input pipeline; ghost text UX; T1 n-grams; accept/reject telemetry; cycling UX. ← current
- **P2 (mo 2):** T2 on-device model (llama-cpp-2, GGUF); speculative prefetch; safety gate; personalization loop (fine-tune on user history); encrypted store.
- **P3 (mo 3):** Opt-in T3 cloud tier; NL command bar; error recovery; signed/notarized builds; auto-update; 1.0.

## Success metrics

- Suggestion latency p99 < 50ms for T0/T1
- Accept rate on replay-eval: > 30% top-1 (Warp-style Next Command baseline)
- Zero network calls in default mode (verifiable via sandboxed CI test)
- Terminal rendering perf within 10% of stock Alacritty (vtebench)

## Privacy principles

1. All training and inference on-device by default
2. Secret redaction (never learn/teach/store tokens, keys, passwords)
3. Encrypted history at rest
4. Cloud tier strictly opt-in, per-session, with visible indicator
