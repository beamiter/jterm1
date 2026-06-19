# jterm1 shell integration for fish.
#
# Source from ~/.config/fish/config.fish:
#     if test "$TERM_PROGRAM" = jterm1
#         source /path/to/jterm1.fish
#     end
#
# Emits OSC 133 (FTCS) command lifecycle marks and OSC 7 cwd updates.

if set -q __jterm1_fish_loaded
    exit 0
end
set -g __jterm1_fish_loaded 1

function __jterm1_osc
    printf '\033]%s\007' $argv[1]
end

function __jterm1_report_cwd --on-variable PWD
    set -l host (hostname 2>/dev/null; or echo localhost)
    set -l enc (string escape --style=url -- $PWD)
    __jterm1_osc "7;file://$host$enc"
end

function __jterm1_prompt_start  ; __jterm1_osc "133;A" ; end
function __jterm1_prompt_end    ; __jterm1_osc "133;B" ; end
function __jterm1_command_start ; __jterm1_osc "133;C" ; end
function __jterm1_command_end   ; __jterm1_osc "133;D;$argv[1]" ; end

function __jterm1_preexec --on-event fish_preexec
    __jterm1_command_start
end

function __jterm1_postexec --on-event fish_postexec
    __jterm1_command_end $status
end

# Wrap the existing fish_prompt so we don't have to fight user customizations.
if not functions -q __jterm1_orig_prompt
    functions -c fish_prompt __jterm1_orig_prompt
    function fish_prompt
        __jterm1_prompt_start
        __jterm1_orig_prompt
        __jterm1_prompt_end
    end
end

# Initial cwd report on first load.
__jterm1_report_cwd

set -gx TERM_PROGRAM jterm1
