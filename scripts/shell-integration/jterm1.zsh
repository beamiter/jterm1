# jterm1 shell integration for zsh.
#
# Source from ~/.zshrc:
#     [[ $TERM_PROGRAM == jterm1 ]] && source /path/to/jterm1.zsh
# or unconditionally.
#
# Emits OSC 133 (FTCS) command lifecycle marks and OSC 7 cwd updates.

[[ -n ${__JTERM1_ZSH_LOADED:-} ]] && return 0
__JTERM1_ZSH_LOADED=1

__jterm1_osc() { printf '\033]%s\007' "$1"; }

__jterm1_prompt_start()  { __jterm1_osc "133;A"; }
__jterm1_prompt_end()    { __jterm1_osc "133;B"; }
__jterm1_command_start() { __jterm1_osc "133;C"; }
__jterm1_command_end()   { __jterm1_osc "133;D;$1"; }

__jterm1_report_cwd() {
    local host=${HOST:-${HOSTNAME:-localhost}}
    local out= i ch
    for (( i=1; i<=${#PWD}; i++ )); do
        ch=${PWD[i]}
        case $ch in
            [A-Za-z0-9._~/-]) out+=$ch ;;
            *) printf -v out '%s%%%02X' "$out" "'$ch" ;;
        esac
    done
    __jterm1_osc "7;file://${host}${out}"
}

__jterm1_preexec() {
    __jterm1_command_start
}

__jterm1_precmd() {
    local ec=$?
    # If the previous turn was a real command (preexec ran), close it out.
    # zsh runs precmd on first prompt too; emitting ;D;0 there is harmless and
    # matches what every other terminal's integration does.
    __jterm1_command_end "$ec"
    __jterm1_report_cwd
    __jterm1_prompt_start
}

# Append the prompt-end mark to PS1 inside %{...%} so widths stay correct.
if [[ -z ${__JTERM1_PS1_HOOKED:-} ]]; then
    PS1="${PS1}%{$(__jterm1_prompt_end)%}"
    __JTERM1_PS1_HOOKED=1
fi

autoload -Uz add-zsh-hook
add-zsh-hook preexec __jterm1_preexec
add-zsh-hook precmd  __jterm1_precmd

export TERM_PROGRAM=jterm1
