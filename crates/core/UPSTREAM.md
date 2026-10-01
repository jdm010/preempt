# Alacritty source

`alacritty/` contains Alacritty 0.17.0 at commit
`94e7c8874e526b1e67b349d9ba30ddf81669119e`. Its MIT and Apache-2.0 license
files are included.

The `vte/` parser is version 0.15.0 at commit
`3b3da71c34cc1256c7e20981cf03f8eb95e08ffc`. Its license files are also
included.

The `preempt-core` crate connects prediction to the terminal. The zsh
integration is `alacritty/extra/shell-integration/zsh/preempt.zsh`. It sends the
current line only when the cursor is at its end. The terminal cannot reliably
rebuild shell input from key events alone.

The hook sends input in a private OSC 777 message. It escapes delimiters, clears
suggestions when the line contains control characters, and caps the message
size. The VTE parser rejects decoded control characters. Input is not logged.

Press Right or Ctrl-F to accept a suggestion. Ctrl-N and Ctrl-P cycle through
suggestions. These keys keep their normal behavior when no suggestion is
active.

The overlay counts accepts and rejects by prediction tier in memory. The
SQLCipher store also keeps feedback counts for keyed, redacted command
fingerprints; it does not store the command text in feedback rows.

## Local model

T2 uses a local GGUF model through llama.cpp. Run `scripts/install-model.sh` to
install the pinned model. The script checks its SHA-256 digest and will not
replace an existing file. Preempt does not download weights at startup.

Set `PREEMPT_GGUF` to use a model at another path. The old
`AUTO_TERMINAL_GGUF` variable still works. T2 stays off when no model is
installed. The app defaults to two candidates and supports up to three.

Building T2 requires CMake and a C++ toolchain.
