# Compatibility shim for existing shell configuration paths.
[[ -o interactive ]] || return
source "${${(%):-%N}:A:h}/preempt.zsh"
