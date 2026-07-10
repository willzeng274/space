# space shell integration (zsh). Emitted by `space --init zsh`; add
#   eval "$(space --init zsh)"
# to ~/.zshrc. The binary reports "cd here, then run this" through a temp
# file; this function does both in YOUR shell, so no nested shells are ever
# spawned and job control / history / SHLVL stay intact.
space() {
    local f st
    f="$(command mktemp "${TMPDIR:-/tmp}/space-handoff.XXXXXX")" || return 1
    command space --handoff-file "$f" "$@"
    st=$?
    local -a lines cmd
    lines=("${(@f)$(<"$f")}")
    command rm -f -- "$f"
    local dir="${lines[1]-}"
    (( ${#lines[@]} > 1 )) && cmd=("${(@)lines[2,-1]}")
    if [[ -n "$dir" ]]; then
        builtin cd -- "$dir" || return 1
    fi
    if (( ${#cmd[@]} > 0 )) && [[ -n "${cmd[1]}" ]]; then
        "${cmd[@]}"
        st=$?
    fi
    return $st
}
