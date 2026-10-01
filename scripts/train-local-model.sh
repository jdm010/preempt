#!/bin/sh
set -eu
umask 077

if [ "$(uname -s)" != "Darwin" ] || [ "$(uname -m)" != "arm64" ]; then
  echo "Local Qwen fine-tuning currently requires Apple Silicon macOS." >&2
  exit 1
fi

APP_DATA="$HOME/Library/Application Support/auto-terminal"
MODEL_ID="mlx-community/Qwen3.5-0.8B-MLX-4bit"
MODEL_REVISION="5d894f8cc4ef3e6c88537bf3746ed262f549da6a"
MODEL_DIR="$APP_DATA/mlx-base"
ADAPTER_DIR="$APP_DATA/t2-adapter"
VENV_DIR="${PREEMPT_MLX_VENV:-${AUTO_TERMINAL_MLX_VENV:-$APP_DATA/training-venv}}"
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO_DIR=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
TRAIN_WORK=""

cleanup() {
  if [ -n "$TRAIN_WORK" ]; then
    rm -rf -- "$TRAIN_WORK"
  fi
}
trap cleanup EXIT HUP INT TERM

mkdir -p "$APP_DATA"
chmod 700 "$APP_DATA"

if [ ! -x "$VENV_DIR/bin/python" ]; then
  python3 -m venv "$VENV_DIR"
fi

if [ ! -x "$VENV_DIR/bin/mlx_lm.lora" ]; then
  "$VENV_DIR/bin/python" -m pip install -r "$REPO_DIR/training/requirements.txt"
fi

TRAIN_WORK=$(mktemp -d "$APP_DATA/training-run.XXXXXX")
chmod 700 "$TRAIN_WORK"
DATA_DIR="$TRAIN_WORK/data"
cargo run --manifest-path "$REPO_DIR/Cargo.toml" -p preempt-store --bin preempt-training-data -- \
  --output-dir "$DATA_DIR"

if [ ! -f "$MODEL_DIR/config.json" ]; then
  "$VENV_DIR/bin/hf" download "$MODEL_ID" \
    --revision "$MODEL_REVISION" \
    --local-dir "$MODEL_DIR"
fi

if [ -e "$ADAPTER_DIR" ]; then
  echo "Refusing to overwrite existing adapter directory: $ADAPTER_DIR" >&2
  exit 1
fi
mkdir -p "$ADAPTER_DIR"
chmod 700 "$ADAPTER_DIR"

"$VENV_DIR/bin/mlx_lm.lora" \
  --model "$MODEL_DIR" \
  --train \
  --data "$DATA_DIR" \
  --iters 600 \
  --batch-size 1 \
  --num-layers 8 \
  --max-seq-length 384 \
  --learning-rate 1e-5 \
  --steps-per-report 25 \
  --steps-per-eval 100 \
  --val-batches 8 \
  --save-every 100 \
  --grad-checkpoint \
  --mask-prompt \
  --adapter-path "$ADAPTER_DIR"

"$VENV_DIR/bin/python" "$REPO_DIR/training/evaluate.py" \
  --model "$MODEL_DIR" \
  --data "$DATA_DIR" \
  --limit 128

"$VENV_DIR/bin/python" "$REPO_DIR/training/evaluate.py" \
  --model "$MODEL_DIR" \
  --data "$DATA_DIR" \
  --adapter "$ADAPTER_DIR" \
  --limit 128

"$VENV_DIR/bin/python" - "$ADAPTER_DIR/adapter_config.json" <<'PY'
import json
import os
import sys
from pathlib import Path

config_path = Path(sys.argv[1])
config = json.loads(config_path.read_text(encoding="utf-8"))
config.pop("data", None)
config.pop("adapter_path", None)
config["model"] = "mlx-community/Qwen3.5-0.8B-MLX-4bit"
config_path.write_text(json.dumps(config, indent=2) + "\n", encoding="utf-8")
os.chmod(config_path, 0o600)
PY
