# Local T2 training

The first fine-tuning run did not improve the test results, so the app does not
load its adapter. See the [report](REPORT.md) for the settings, scores, and
charts.

Fine-tuning runs on Apple Silicon with MLX. The GGUF inference model remains
separate from the training checkpoint and all datasets and adapters stay in
`~/Library/Application Support/auto-terminal/`. The directory keeps its
existing name so Preempt can continue using local model and training assets.

## Data and privacy

`preempt-training-data` reads local zsh history and redacts common secrets. It
then skips commands changed by redaction or marked as sensitive. It splits by
whole command, so prefixes from one command cannot appear in both training and
validation. The training script writes JSONL files to a private temporary
directory and removes them when it exits. The exporter prints counts and
refuses to overwrite an existing directory. It also writes a private
`train-history.jsonl` with timestamps and repeated history entries. Cascade
replay uses this file to rank T0/T1 suggestions without adding validation
commands to the history index.

Redaction can miss secrets. A trained adapter can retain patterns from its
input, so it stays in app data with owner-only permissions. The script does not
upload command data or enable a training tracker.

## Train

Run this on an Apple Silicon Mac:

```sh
scripts/train-local-model.sh
```

To export data without training, pass a private output path to
`cargo run -p preempt-store --bin preempt-training-data -- --output-dir PATH`.

To evaluate T0/T1/T2 on that data without printing commands:

```sh
cargo run -p preempt-predict --features llama --bin preempt-model-eval -- \
  --data PATH --cascade --candidate-limit 2
```

Append `--trigger-on-fast-miss` to queue T2 only when T0/T1 has no candidate.

The script pins `mlx-lm` 0.31.3 and Qwen3.5-0.8B MLX revision
`5d894f8cc4ef3e6c88537bf3746ed262f549da6a`. It creates a private virtual
environment, downloads the checkpoint into app data, trains an 8-layer LoRA
adapter, then scores the base and adapter on 128 deduplicated holdout examples.
It prints counts only. Before cleanup, it removes the temporary dataset path
from the adapter metadata. Set
`PREEMPT_MLX_VENV` to use an existing compatible virtual environment.
`AUTO_TERMINAL_MLX_VENV` remains accepted as a compatibility alias.

## Evaluation status

The first run used 1,073 history entries. Of these, 407 unique commands passed
the redaction filter. On an evenly spread 128-example sample, the base and
adapter both had 0 exact suffixes. The base produced 128 clean one-line outputs;
the adapter produced 98. The app does not load the adapter.

The Rust `T2Prefetcher` replayed the installed GGUF on the same holdout. Three
candidates gave 2/128 exact suffixes and returned a candidate on 124/128
requests; warm latency was 1,275 ms median and 2,362 ms p95 (3,519 ms cold).
Two candidates kept the exact count at 2/128 and returned a candidate on 119/128
requests. Warm latency fell to 806 ms median and 1,468 ms p95 (1,142 ms cold).
One candidate also gave 2/128 exact suffixes, with a candidate on 95/128
requests. The app uses two candidates by default. These GGUF results are
separate from the MLX scores above.

A timestamp-aware cascade replay on 2026-09-30 found T0/T1 candidates on 78 of
128 requests, including 5 exact suffixes. T2 added 2 exact suffixes. Together,
the tiers had 7 exact suffixes, and the first suggestion was exact on 6 inputs.
T0/T1 took 24 µs median and 72 µs p95. T2 took 1,162 ms warm median and
2,347 ms p95. The report describes the method and includes a chart.

On the same holdout, triggering T2 only when T0/T1 had no candidate reduced
T2 requests from 128 to 50. With two candidates it retained 7 combined exact
suffixes and 6 top-1 hits; with one candidate it retained 6 exact candidate
hits and the same 6 top-1 hits. Second-candidate sampling makes per-run
alternatives variable; policy details and latency samples are in the report.

Preempt loads the GGUF model; it does not load the MLX adapter. The MLX guide
lists GGUF export for Llama, Mistral, and Mixtral, but not Qwen3.5
([MLX-LM LoRA guide](https://github.com/ml-explore/mlx-lm/blob/main/mlx_lm/LORA.md)).
A Qwen3.5 adapter-conversion failure was also reported in
[llama.cpp issue #21125](https://github.com/ggml-org/llama.cpp/issues/21125),
which GitHub now marks as closed as a duplicate. We have not tested newer
conversion tools. The adapter stays disabled because it scored worse than the
base model in this holdout.
