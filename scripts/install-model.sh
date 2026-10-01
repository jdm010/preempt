#!/bin/sh
set -eu

MODEL_REPOSITORY='ggml-org/Qwen3.5-0.8B-GGUF'
MODEL_REVISION='8fea620810c4afa23dd6443f999a48574c1611a3'
MODEL_FILENAME='Qwen3.5-0.8B-Q4_0.gguf'
MODEL_SHA256='57d1997790d1744fba5b40a7317df71ea5e2acee28c47e78f0cce39c0703f8cf'

if [ "$#" -gt 1 ]; then
    printf 'Usage: %s [model-destination]\n' "$0" >&2
    exit 2
fi

if [ "$#" -eq 1 ]; then
    MODEL_PATH=$1
elif [ "$(uname -s)" = 'Darwin' ]; then
    # Preserve the existing app-data location so prior installs are reused.
    MODEL_PATH="${HOME}/Library/Application Support/auto-terminal/model.gguf"
else
    MODEL_PATH="${XDG_DATA_HOME:-${HOME}/.local/share}/auto-terminal/model.gguf"
fi

if command -v shasum >/dev/null 2>&1; then
    sha256_file() { LC_ALL=C shasum -a 256 "$1" | awk '{print $1}'; }
elif command -v sha256sum >/dev/null 2>&1; then
    sha256_file() { sha256sum "$1" | awk '{print $1}'; }
else
    printf 'Install shasum or sha256sum before downloading model weights.\n' >&2
    exit 1
fi

if [ -e "$MODEL_PATH" ]; then
    if [ -f "$MODEL_PATH" ] && [ "$(sha256_file "$MODEL_PATH")" = "$MODEL_SHA256" ]; then
        printf 'Model is already installed and verified: %s\n' "$MODEL_PATH"
        exit 0
    fi
    printf 'Refusing to overwrite the existing file: %s\n' "$MODEL_PATH" >&2
    printf 'Move it aside or choose a different destination.\n' >&2
    exit 1
fi

MODEL_DIR=$(dirname "$MODEL_PATH")
mkdir -p "$MODEL_DIR"
PARTIAL_PATH="${MODEL_PATH}.partial"
MODEL_URL="https://huggingface.co/${MODEL_REPOSITORY}/resolve/${MODEL_REVISION}/${MODEL_FILENAME}"

printf 'Downloading %s (about 563 MB) to %s\n' "$MODEL_FILENAME" "$MODEL_PATH"
curl --fail --location --retry 3 --continue-at - --output "$PARTIAL_PATH" "$MODEL_URL"

ACTUAL_SHA256=$(sha256_file "$PARTIAL_PATH")
if [ "$ACTUAL_SHA256" != "$MODEL_SHA256" ]; then
    rm -f "$PARTIAL_PATH"
    printf 'Downloaded file failed SHA-256 verification.\n' >&2
    exit 1
fi

mv "$PARTIAL_PATH" "$MODEL_PATH"
printf 'Installed and verified local model: %s\n' "$MODEL_PATH"
