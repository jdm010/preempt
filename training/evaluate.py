#!/usr/bin/env python3
"""Score private validation examples without printing shell commands."""

import argparse
import gc
import json
from pathlib import Path

import mlx.core as mx
from mlx_lm import generate, load
from mlx_lm.sample_utils import make_sampler


def parse_args():
    parser = argparse.ArgumentParser()
    parser.add_argument("--model", required=True, type=Path)
    parser.add_argument("--data", required=True, type=Path)
    parser.add_argument("--adapter", type=Path)
    parser.add_argument("--limit", type=int, default=128)
    return parser.parse_args()


def read_examples(path: Path, limit: int):
    examples = {}
    with path.open(encoding="utf-8") as dataset:
        for line in dataset:
            messages = json.loads(line)["messages"]
            key = (messages[-2]["content"], messages[-1]["content"])
            examples.setdefault(key, messages)

    examples = list(examples.values())
    if len(examples) <= limit:
        return examples
    if limit <= 1:
        return examples[:limit]

    # Spread the sample across the full command-sorted validation set instead
    # of measuring only its first commands.
    last_index = len(examples) - 1
    return [examples[index * last_index // (limit - 1)] for index in range(limit)]


def main():
    args = parse_args()
    messages = read_examples(args.data / "valid.jsonl", args.limit)
    if not messages:
        raise SystemExit("validation set is empty")

    model, tokenizer = load(
        str(args.model), adapter_path=str(args.adapter) if args.adapter else None
    )
    exact = clean = 0

    for example in messages:
        target = example[-1]["content"]
        user_content = example[-2]["content"]
        prefix = user_content.split("<shell-command-prefix>\n", 1)[1].split(
            "\n</shell-command-prefix>", 1
        )[0]
        prompt = tokenizer.apply_chat_template(
            example[:-1], tokenize=False, add_generation_prompt=True
        )
        output = generate(
            model,
            tokenizer,
            prompt,
            max_tokens=48,
            sampler=make_sampler(temp=0.0),
            verbose=False,
        )
        line = output.splitlines()[0] if output else ""
        if line.startswith("Append:"):
            line = line[7:].lstrip()
        suffix = line[len(prefix) :] if line.startswith(prefix) else line
        valid = (
            bool(suffix.strip())
            and not any(ord(char) < 32 for char in suffix)
            and "<think>" not in suffix
            and "</think>" not in suffix
        )
        clean += int(valid)
        exact += int(valid and suffix == target)

    label = "adapter" if args.adapter else "base"
    print(f"{label}: exact suffixes {exact}/{len(messages)}")
    print(f"{label}: clean one-line suffixes {clean}/{len(messages)}")
    del model, tokenizer
    mx.clear_cache()
    gc.collect()


if __name__ == "__main__":
    main()
