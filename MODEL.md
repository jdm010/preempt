# Local model

Preempt's optional T2 model is [Qwen3.5-0.8B](https://huggingface.co/Qwen/Qwen3.5-0.8B)
in GGUF Q4_0 format. The file comes from the
[ggml-org GGUF repository](https://huggingface.co/ggml-org/Qwen3.5-0.8B-GGUF).
The model is licensed under Apache-2.0. Its weights are about 563 MB and are
not committed to this repository.

The installer pins revision `8fea620810c4afa23dd6443f999a48574c1611a3` and
expects SHA-256 `57d1997790d1744fba5b40a7317df71ea5e2acee28c47e78f0cce39c0703f8cf`.

On macOS or Linux, run `scripts/install-model.sh`. It downloads the pinned
revision, resumes an interrupted download, checks the SHA-256 digest, and will
not overwrite an existing file. By default, it uses the existing
`auto-terminal` data folder so Preempt can find models installed before the
rename. To use another path, pass it to the script and set `PREEMPT_GGUF` to
the same path when launching the terminal. `AUTO_TERMINAL_GGUF` still works.

The app never downloads model weights at startup. It runs inference locally
through llama.cpp. If the GGUF file is missing, Preempt uses the history
predictors without T2.

On macOS 14, builds use the CPU backend because llama.cpp's Metal backend needs
an API from macOS 15. On macOS 15 or newer, set `GGML_METAL=ON` to build with
Metal support.
