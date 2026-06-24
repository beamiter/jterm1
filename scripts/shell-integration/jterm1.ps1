# jterm1 shell integration for PowerShell (Windows PowerShell 5+ / pwsh 7+).
#
# Emits FTCS / OSC 133 marks so jterm1 can attribute output to discrete blocks
# and know each command's exit code, plus OSC 7 so it tracks the working dir.
#
# Source from your $PROFILE, e.g.
#     if ($env:TERM_PROGRAM -eq 'jterm1') { . /path/to/jterm1.ps1 }
# or unconditionally:
#     . /path/to/jterm1.ps1
#
# Safe to source under non-jterm1 terminals: the OSC sequences are silently
# discarded by anything that doesn't parse them.

# Guard against double-sourcing in the same shell session.
if ($script:__jterm1_loaded) { return }
$script:__jterm1_loaded = $true

$script:__jterm1_in_cmd = $false
$script:__jterm1_orig_prompt = ${function:prompt}

# Build an OSC payload terminated with BEL. ESC = char 27, BEL = char 7.
function __jterm1_osc($payload) {
    "$([char]27)]${payload}$([char]7)"
}

function __jterm1_report_cwd_seq {
    # OSC 7 — percent-encode the path so non-ASCII / spaces survive.
    $path = (Get-Location).ProviderPath
    $hostName = if ($env:COMPUTERNAME) { $env:COMPUTERNAME } else { [System.Net.Dns]::GetHostName() }
    $bytes = [System.Text.Encoding]::UTF8.GetBytes($path)
    $sb = [System.Text.StringBuilder]::new()
    foreach ($b in $bytes) {
        $c = [char]$b
        if (($b -ge 0x41 -and $b -le 0x5a) -or  # A-Z
            ($b -ge 0x61 -and $b -le 0x7a) -or  # a-z
            ($b -ge 0x30 -and $b -le 0x39) -or  # 0-9
            $c -eq '/' -or $c -eq '.' -or $c -eq '_' -or $c -eq '-' -or $c -eq '~') {
            [void]$sb.Append($c)
        } else {
            [void]$sb.AppendFormat('%{0:X2}', $b)
        }
    }
    __jterm1_osc "7;file://${hostName}$($sb.ToString())"
}

# Replace the global `prompt` function. Build a single string composed of:
#   [OSC 133;D;<exit> if a command just finished]
#   [OSC 7;file://...]
#   [OSC 133;A]
#   <original prompt text>
#   [OSC 133;B]
#
# Returning a single string keeps PSReadLine's column accounting correct (it
# treats OSC bytes as zero-width when they're contained in the prompt return).
function global:prompt {
    # Snapshot exit info immediately — every cmdlet below clobbers $? / $LASTEXITCODE.
    $dollarQ = $?
    $lastEC = $LASTEXITCODE
    $ec = if ($dollarQ) { 0 } elseif ($lastEC) { $lastEC } else { 1 }

    $pre = ''
    if ($script:__jterm1_in_cmd) {
        $pre += __jterm1_osc "133;D;$ec"
        $script:__jterm1_in_cmd = $false
    }
    $pre += __jterm1_report_cwd_seq
    $pre += __jterm1_osc "133;A"

    # Title: keep iTerm2-style "user@host:cwd" so jterm1's tab label reflects pwd.
    $title = "$($env:USERNAME)@$([System.Net.Dns]::GetHostName()):$(Get-Location)"
    $pre += "$([char]27)]2;${title}$([char]7)"

    $orig = ''
    try {
        $orig = & $script:__jterm1_orig_prompt
    } catch {
        $orig = "PS $(Get-Location)> "
    }

    $post = __jterm1_osc "133;B"

    # Restore exit state so the user-visible $? / $LASTEXITCODE aren't perturbed
    # by anything this function ran. PSReadLine's renderer reads these after
    # prompt() returns when expanding `$?` in user PS1.
    $global:LASTEXITCODE = $lastEC
    if (-not $dollarQ) {
        # No clean PS way to set $? = $false; the side effect of a failing
        # expression at the end of a function call does it.
        & { Write-Error -ErrorAction SilentlyContinue 'preserve-dollar-q' } 2>$null
    }

    return "${pre}${orig}${post}"
}

# OSC 133;C — emit when the user submits a command. PSReadLine's Enter handler
# is the right hook; emit ;C just before AcceptLine actually runs the buffer.
# Skip when the buffer is empty (blank Enter at prompt produces no command).
if (Get-Module -ListAvailable PSReadLine) {
    Set-PSReadLineKeyHandler -Chord Enter -ScriptBlock {
        param($key, $arg)
        $line = $null
        $cursor = $null
        [Microsoft.PowerShell.PSConsoleReadLine]::GetBufferState([ref]$line, [ref]$cursor)
        if ($line -and $line.Trim().Length -gt 0) {
            [Console]::Write($(__jterm1_osc "133;C"))
            $script:__jterm1_in_cmd = $true
        }
        [Microsoft.PowerShell.PSConsoleReadLine]::AcceptLine($key, $arg)
    }

    # Same handling for keypad Enter, which is a distinct chord.
    Set-PSReadLineKeyHandler -Chord NumPadEnter -ScriptBlock {
        param($key, $arg)
        $line = $null
        $cursor = $null
        [Microsoft.PowerShell.PSConsoleReadLine]::GetBufferState([ref]$line, [ref]$cursor)
        if ($line -and $line.Trim().Length -gt 0) {
            [Console]::Write($(__jterm1_osc "133;C"))
            $script:__jterm1_in_cmd = $true
        }
        [Microsoft.PowerShell.PSConsoleReadLine]::AcceptLine($key, $arg)
    }
}

$env:TERM_PROGRAM = 'jterm1'
