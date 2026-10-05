# Preempt

Preempt is a terminal prototype that suggests commands from zsh history. It is
built on a modified Alacritty source tree. An optional local model can add more
suggestions. The prediction code has no cloud service.

## Current status

| Part | Status |
|---|---|
| T0 history matching | Implemented. Ranks command prefixes by frequency and recency. |
| T1 n-gram fallback | Implemented. Runs when T0 has no match. |
| T2 local model | Experimental. The app requests T2 only on T0/T1 misses and streams its first candidate. With a shorter prompt, a 128-input replay measured 401 ms warm median to the first candidate and 592 ms to the complete two-candidate set; the target is 150 ms. |
| Risk hints | Implemented as UI labels. They do not block commands. |
| History and feedback | Stored locally in a SQLCipher database. The key is kept in the operating system's credential store. |
| Terminal overlay | Displays ghost text and accepts or cycles suggestions. A zsh hook supplies the current input line. |
| Cloud prediction | Not implemented. |

The latest T0/T1 replay measured 9 µs median and 25 µs p95. See the
[evaluation report](training/REPORT.md) for the setup and full results.

## Milestones

- **P0 — history predictor: complete (2026-09-26).** Added the Rust workspace,
  zsh history parser, prefix index, and frequency/recency ranking.
- **P1 — terminal integration: complete (2026-09-26).** Added the Alacritty
  overlay, zsh input hook, n-gram fallback, suggestion cycling, and risk hints.
- **P2 — local model and storage: in progress.** Added optional GGUF inference,
  background generation, encrypted history and feedback storage, and a local
  training workflow. The app does not download model weights at startup.
  - The first MLX fine-tuning run did not improve results. On a 128-example
    holdout, the base model and adapter both produced 0 exact completions. The
    adapter produced 98 clean one-line outputs; the base produced 128. The app
    does not use the adapter.
  - In the GGUF candidate sweep, two candidates produced 2 exact completions
    out of 128 and returned at least one candidate for 119 inputs. Warm median
    latency was 806 ms, above the 150 ms target.
  - A repeatable 128-example replay on 2026-10-04 found 7 exact T0/T1 hits.
    Triggering T2 only on the 47 fast-tier misses kept top-1 exact hits at 9,
    compared with all-request T2, and returned candidates on the same 122
    inputs with two candidates. It found one fewer exact cycleable alternative
    (9 rather than 10) while reducing T2 calls by 63%. The app uses this trigger.
  - Before prompt shortening, a 64-input replay measured a 574 ms warm median to
    the first usable candidate (21 samples) and 776 ms to the full set (24
    samples); the sample sets differ. Combined candidates contained 6/64 exact
    completions.
  - A local phase profile found about 186 ms median prompt evaluation for a
    92–95-token prompt on four CPU threads. A small thread-count comparison
    favored the existing four-thread default.
  - Shortening the prompt reduced it from 92–95 tokens to 57–60 and prompt
    evaluation to 128 ms median. On the same 64-input holdout, top-1 exact
    completions rose from 5 to 6, exact-in-candidates stayed at 6, and
    availability rose from 61 to 63. The 128-input replay kept 9 top-1 and
    exact candidate completions, with availability rising from 122 to 125.
  - Next: reduce first-candidate latency further while preserving holdout
    quality before another training run.
- **P3 — future work.** Consider opt-in cloud prediction, a natural-language
  command bar, error recovery, packaging, signing, and automatic updates.

## Performance targets

- T0/T1: p99 latency below 50 ms.
- T2: latency below 150 ms.
- Replay: top suggestion accepted on more than 30% of examples.
- Default mode: no network requests.
- Rendering: within 10% of stock Alacritty.

On the 2026-10-04 replay, the T0/T1 stage took 9 µs median and 25 µs p95. In
the 2026-10-05 shorter-prompt replay, warm T2 latency was 401 ms median to the
first candidate and 592 ms to the full two-candidate set. There is no project
CI or release pipeline yet.

## Privacy requirements

- Keep shell history, prompts, and model inference on the device by default.
- Redact common secrets and encrypt stored history. Redaction can miss secrets.
- Do not add cloud prediction without an explicit opt-in and a visible status.
- Keep training data and adapters in local app data; do not commit or upload
  them.
