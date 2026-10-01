# Preempt

Preempt is a terminal prototype that suggests commands from zsh history. It is
built on a modified Alacritty source tree. An optional local model can add more
suggestions. The prediction code has no cloud service.

## Current status

| Part | Status |
|---|---|
| T0 history matching | Implemented. Ranks command prefixes by frequency and recency. |
| T1 n-gram fallback | Implemented. Runs when T0 has no match. |
| T2 local model | Experimental. The two-candidate run returned a candidate for 119 of 128 inputs. Warm latency was 806 ms median and 1,468 ms p95; the target is 150 ms. |
| Risk hints | Implemented as UI labels. They do not block commands. |
| History and feedback | Stored locally in a SQLCipher database. The key is kept in the operating system's credential store. |
| Terminal overlay | Displays ghost text and accepts or cycles suggestions. A zsh hook supplies the current input line. |
| Cloud prediction | Not implemented. |

The T0/T1 replay measured 24 µs median and 72 µs p95. See the [evaluation
report](training/REPORT.md) for the test setup and results.

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
  - In a separate cascade replay, T0/T1 produced 5 exact completions and T2
    added 2. The combined set had 7; the first suggestion was exact on 6 inputs.
  - Next: reduce T2 latency, make replay comparisons more repeatable, and
    evaluate the fast-tier miss trigger before another training run.
- **P3 — future work.** Consider opt-in cloud prediction, a natural-language
  command bar, error recovery, packaging, signing, and automatic updates.

## Performance targets

- T0/T1: p99 latency below 50 ms.
- T2: latency below 150 ms.
- Replay: top suggestion accepted on more than 30% of examples.
- Default mode: no network requests.
- Rendering: within 10% of stock Alacritty.

The measured results are listed above. There is no project CI or release
pipeline yet.

## Privacy requirements

- Keep shell history, prompts, and model inference on the device by default.
- Redact common secrets and encrypt stored history. Redaction can miss secrets.
- Do not add cloud prediction without an explicit opt-in and a visible status.
- Keep training data and adapters in local app data; do not commit or upload
  them.
