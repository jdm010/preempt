# Local T2 training and evaluation

**Training run:** 2026-09-29 · **Cascade replay:** 2026-09-30
**Status:** exploratory; the trained adapter is not used by the terminal

## Summary

We exported local history, trained a Qwen3.5-0.8B MLX LoRA adapter, and scored
the MLX model and the terminal's GGUF path. The adapter had no exact completions
and produced fewer clean outputs than the base model, so the app does not use
it. The GGUF sweep led us to lower the default candidate count from three to
two: exact-match count stayed the same while latency fell.

A fresh command-disjoint replay found an exact suffix in 5/128 T0/T1 candidate
sets and 2/128 T2 candidate sets. Their union contained 7/128 exact suffixes;
the app-order top suggestion was exact on 6/128. T0/T1 prediction was fast
(24 µs median, 72 µs p95), while T2 remained slow (1,162 ms warm median,
2,347 ms p95).

Policy replays showed that invoking T2 only on fast-tier misses reduced model
requests from 128 to 50. With two candidates, it retained 7 combined exact
suffixes and 6 top-1 hits in this holdout; one candidate retained 6 and 6.

## Data preparation

The exporter processed **1,073 shell-history entries**:

| Measure | Count |
|---|---:|
| Unique commands passing the redaction filter | 407 |
| History entries skipped | 69 |
| Training examples generated | 3,938 |
| Validation examples generated | 485 |

Commands were split as whole strings, so prefixes of one command could not land
in both training and validation. Each safe command generated up to eight prefix
completion examples, with modest repetition for more frequent commands. The
validation evaluation deduplicated identical prefix/target pairs and sampled
evenly across the command-sorted set.

The exporter uses the store's common-secret redactor and excludes commands that
would need redaction or match sensitive markers. Training data was written to a
private temporary directory and removed after evaluation. The scoring tools
printed aggregate metrics only; no command text is included in this report.
Redaction is best-effort, and model weights can retain patterns from training
data, so the adapter remains in owner-only local app data.

## Fine-tuning run

| Setting | Value |
|---|---|
| Base checkpoint | `mlx-community/Qwen3.5-0.8B-MLX-4bit` |
| Checkpoint revision | `5d894f8cc4ef3e6c88537bf3746ed262f549da6a` |
| Trainer | `mlx-lm` 0.31.3 |
| Fine-tuning method | LoRA on the last 8 transformer layers |
| LoRA rank / scale / dropout | 8 / 20 / 0 |
| Steps / batch size | 600 / 1 |
| Maximum sequence length | 384 tokens |
| Learning rate | `1e-5` |
| Other options | prompt masking and gradient checkpointing |
| Peak reported memory | 1.621 GB |

Validation loss generally fell during training, with a small rise at steps 400
and 500 before reaching its lowest reported value at step 600. These values came
from eight validation batches per checkpoint and should be treated as a noisy
training signal, not a quality score.

```mermaid
xychart-beta
    title "MLX adapter validation loss"
    x-axis "Training step" [1, 100, 200, 300, 400, 500, 600]
    y-axis "Loss" 0 --> 6
    line [5.804, 3.282, 3.071, 2.931, 3.041, 3.221, 2.815]
```

| Step | Validation loss | Training loss at report |
|---:|---:|---:|
| 1 | 5.804 | — |
| 100 | 3.282 | 2.972 |
| 200 | 3.071 | 1.649 |
| 300 | 2.931 | 1.967 |
| 400 | 3.041 | 1.890 |
| 500 | 3.221 | 2.004 |
| 600 | 2.815 | 1.994 |

## MLX generation results

The scorer used deterministic MLX generation on a deduplicated, evenly spread
sample of 128 validation examples. “Exact” means the first generated line,
after removing an echoed prefix if present, exactly matched the expected suffix.
“Clean line” means the output was nonempty, one line, and free of control or
thinking tags; it does **not** mean the command was syntactically valid or safe.

| Model | Exact suffixes | Clean one-line outputs |
|---|---:|---:|
| Unadapted MLX checkpoint | 0/128 | 128/128 |
| Full 600-step LoRA adapter | 0/128 | 98/128 |
| MLP-only factors filtered from that adapter | 0/128 | 128/128 |

The MLP-only row is a post-hoc subset of the trained adapter weights, not a
separately trained run. It restored clean formatting but did not add an exact
completion. The full adapter is therefore not an improvement over the base
checkpoint on this evaluation.

## GGUF runtime results

We separately replayed the same 128-example holdout through the Rust
`T2Prefetcher` and the installed GGUF. This exercises the app prompt, llama.cpp,
normalization, debounce, and candidate generation; it is not directly comparable
to MLX generation. “Candidate coverage” counts requests that returned at least
one normalized candidate.

| Candidate limit | Top-1 exact | Exact among returned candidates | Candidate coverage | Warm median | Warm p95 | Cold first request |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 2/128 | 2/128 | 95/128 | 489 ms | 967 ms | 971 ms |
| 2 | 2/128 | 2/128 | 119/128 | 806 ms | 1,468 ms | 1,142 ms |
| 3 | 2/128 | 2/128 | 124/128 | 1,275 ms | 2,362 ms | 3,519 ms |

```mermaid
xychart-beta
    title "GGUF candidate coverage"
    x-axis "Candidate limit" [1, 2, 3]
    y-axis "Requests with a candidate" 0 --> 128
    bar [95, 119, 124]
```

```mermaid
xychart-beta
    title "GGUF warm latency by candidate limit"
    x-axis "Candidate limit" [1, 2, 3]
    y-axis "Latency in milliseconds" 0 --> 2500
    line [489, 806, 1275]
    line [967, 1468, 2362]
```

In the latency chart, the first line is warm median and the second is warm p95.
Each budget was measured in a separate process; cold first-request times varied
with model loading and filesystem cache. The host was an Apple Silicon Mac with
16 GB RAM on macOS 14.6. MLX used the GPU for training; the app's llama.cpp
runtime used CPU because Metal was disabled on this OS/runtime combination. The
measured two-candidate p95 of 1,468 ms remains well above the project's 150 ms
T2 target. T2 runs asynchronously, but latency still limits how often a result
can arrive before the user continues typing.

The app now defaults to **two candidates**. It kept the same exact-match count
as three candidates, returned candidates on 119 rather than 124 requests, and
reduced warm median latency by about 37% and p95 by about 38% in this run. One
candidate was faster but had lower coverage.

## End-to-end cascade replay

On 2026-09-30, the Rust replay tool ran the same 128-example, command-disjoint
holdout through T0/T1 and the app's asynchronous T2 path. The exporter wrote a
private `train-history.jsonl` alongside the train and validation examples. It
contains only redaction-safe commands assigned to training, with original
timestamps and repeated history entries intact; validation commands are kept
out of the T0/T1 index. The evaluator suppresses all command text.

| Stage | Requests with candidates | Top-1 exact suffix | Exact suffix in candidates |
|---|---:|---:|---:|
| T0/T1 engine | 78/128 | 5/128 | 5/128 (up to 5 shown) |
| T2, two candidates | 119/128 | 2/128 | 2/128 |
| Combined app order | — | 6/128 | 7/128 |

T0 supplied candidates for 69 requests, T1's n-gram fallback for 9, and neither
fast tier supplied a candidate for 50. T0/T1's exact matches were all top-1.
T2 added a new cycleable candidate on 119 requests and recovered two exact
suffixes that were absent from T0/T1. The combined candidate set improved exact
coverage from 5 to 7 examples; app-order top-1 improved from 5 to 6.

```mermaid
xychart-beta
    title "Exact suffix hits in the 128-example cascade replay"
    x-axis "Series order: T0/T1, T2, combined" [1, 2, 3]
    y-axis "Exact suffix hits" 0 --> 8
    bar [5, 2, 7]
```

The synchronous `Engine::suggest` stage measured 24 µs median and 72 µs p95
across 128 requests. T2's first request took 5,875 ms from queueing through its
callback (including model startup); warm requests measured 1,162 ms median and
2,347 ms p95. T2 includes the 220 ms debounce. This fresh run was slower than
the earlier candidate-budget sweep, so treat those wall-clock figures as a
noisy single-machine sample. The T0/T1 replay uses safe training-side history,
but does not apply persisted feedback-based reranking.

### Candidate budget and trigger policy

We compared all-request T2 against a policy that queues T2 only when the fast
T0/T1 engine has no candidate. The four runs use the same 128-example holdout
and timestamped training-side history:

| Policy | T2 requests | T2 requests with candidates | Combined requests with candidates | Combined top-1 exact | Combined exact in candidates | T2 warm median / p95 |
|---|---:|---:|---:|---:|---:|---:|
| All requests, 1 candidate | 128/128 | 95/128 | 114/128 | 6/128 | 7/128 | 646 / 1,952 ms |
| All requests, 2 candidates | 128/128 | 119/128 | 123/128 | 6/128 | 7/128 | 1,191 / 2,767 ms |
| Fast misses only, 1 candidate | 50/128 | 36/50 | 114/128 | 6/128 | 6/128 | 644 / 1,676 ms |
| Fast misses only, 2 candidates | 50/128 | 49/50 | 127/128 | 6/128 | 7/128 | 1,237 / 3,863 ms |

The selective trigger reduced T2 requests by **61%**. In this replay, both
two-candidate policies had 7 exact suffixes in the combined set and 6 exact
app-order top suggestions. The one-candidate full path had the same scores;
the one-candidate selective path lost one exact alternate but kept top-1
unchanged. The two-candidate path returned more cycleable alternatives, at a
higher per-request cost. Selective triggering reduces the number of model
requests; it does not make each individual T2 response faster.

```mermaid
xychart-beta
    title "Combined exact suffix hits by policy"
    x-axis "Order: full-1, full-2, miss-only-1, miss-only-2" [1, 2, 3, 4]
    y-axis "Exact suffix hits" 0 --> 8
    bar [7, 7, 6, 7]
```

```mermaid
xychart-beta
    title "T2 requests issued by policy"
    x-axis "Order: full-1, full-2, miss-only-1, miss-only-2" [1, 2, 3, 4]
    y-axis "Requests" 0 --> 128
    bar [128, 128, 50, 50]
```

The second candidate is sampled using the worker's request ID as its seed, so
skipping requests changes the sampled alternative. The second-candidate rows
are therefore practical policy samples, not perfectly paired deterministic
comparisons. Latency also varied across process runs and prompt subsets; use the
request counts and exact-match totals as the stronger signals.

## Adapter/runtime status

The MLX adapter was not converted to GGUF and the app does not load it. The
MLX guide lists GGUF export for Llama, Mistral, and Mixtral, but not Qwen3.5
([MLX-LM LoRA guide](https://github.com/ml-explore/mlx-lm/blob/main/mlx_lm/LORA.md)).
A Qwen3.5 adapter-conversion failure was reported in
[llama.cpp issue #21125](https://github.com/ggml-org/llama.cpp/issues/21125);
GitHub now marks it as closed as a duplicate. We have not tested newer
conversion tools. The adapter stays disabled because it scored worse than the
base model in this evaluation.

The official [Qwen3.5-0.8B model card](https://huggingface.co/Qwen/Qwen3.5-0.8B)
describes that checkpoint as post-trained and lists a separate
`Qwen3.5-0.8B-Base` parent. The GGUF metadata's base-model reference therefore
does not mean the installed checkpoint itself is an untuned base model.

## Reproduction and files

- `scripts/train-local-model.sh` exports a private temporary dataset, trains,
  scores base and adapter, sanitizes path metadata, then removes the dataset.
- `crates/store/src/bin/preempt-training-data.rs` performs the redaction-safe export,
  including the private timestamped `train-history.jsonl` used by cascade replay.
- `training/evaluate.py` scores MLX holdout completions without printing them.
- `crates/predict/src/bin/preempt-model-eval.rs --data DIR` scores the GGUF runtime
  using aggregate-only output; `--candidate-limit 1..3` compares latency budgets.
- `crates/predict/src/t2.rs` contains the runtime's two-candidate default.

The 600-step training completed and saved its adapter. The wrapper then exited
with a shell error because its script was edited while that run was in progress;
scoring was completed directly afterward. The wrapper has since been corrected
and its shell syntax checked.

## Decision and next work

- Keep the current adapter disabled; this run did not improve exact completion
  accuracy and the full adapter reduced clean output rate.
- Keep two as the default T2 candidate limit based on the coverage/latency tradeoff.
- Improve T2 runtime latency or evaluate a trigger policy before another
  personalization run. The current adapter still did not improve completions.
