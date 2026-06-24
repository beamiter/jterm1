# jterm1 shell integration

Source the file matching your shell from its rc file:

| Shell      | File           | Source from |
|------------|----------------|-------------|
| bash       | `jterm1.bash`  | `~/.bashrc` |
| zsh        | `jterm1.zsh`   | `~/.zshrc`  |
| fish       | `jterm1.fish`  | `~/.config/fish/config.fish` |
| PowerShell | `jterm1.ps1`   | `$PROFILE`  |

Example (bash):

```bash
[[ $TERM_PROGRAM == jterm1 ]] && source /path/to/jterm1.bash
```

Example (PowerShell):

```powershell
if ($env:TERM_PROGRAM -eq 'jterm1') { . /path/to/jterm1.ps1 }
```

The PowerShell script requires PSReadLine (bundled with pwsh 7+; preinstalled
on Windows PowerShell 5+). Without it OSC 133 ;C is not emitted on Enter — the
prompt-side markers still fire, so blocks are still demarcated but exit codes
attach only at the *next* prompt.

## What it provides

Each script emits two escape sequence families that jterm1 parses to drive its
block view (`src/parser.rs`):

- **OSC 133 (FTCS)** — `;A` at prompt render, `;B` when prompt finishes,
  `;C` when a command starts executing, `;D;<exit>` when it returns. This
  lets jterm1 attribute output to discrete blocks and read the exit code
  exactly (no error-text heuristics).

- **OSC 7** — reports the current working directory as a `file://` URI so the
  active prompt chip stays in sync with `cd`.

The sequences are silently ignored by terminals that don't understand them,
so it is safe to source unconditionally.
