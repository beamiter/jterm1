# Welcome to jterm1 notebooks

A `.jtnb.md` file is just **markdown** with `bash` / `sh` / `shell` code fences.
Each code fence becomes a runnable cell. Click *Run* to execute, *Stop* to
cancel a long-running cell, *Copy* to grab the source.

Cells run in an *isolated* `bash -c` subprocess rooted at the notebook's own
directory — they do **not** touch your active terminal. That means no shell
aliases, no PROMPT_COMMAND surprises, just plain bash.

## Try it

```bash
echo "hello from a notebook cell"
```

## Multiple commands per cell

```bash
date
uname -srm
echo "cwd is $(pwd)"
```

## Non-zero exit codes

The exit status is shown after the cell finishes. A non-zero status is
highlighted.

```bash
ls /this/path/does/not/exist
```

## Long-running cells

Use *Stop* to send SIGKILL. The cell will report `cancelled` instead of an
exit code.

```bash
echo "starting"; sleep 30; echo "done"
```

## Other languages

Fences in other languages render as read-only snippets — no Run button.
This keeps execution explicit and predictable.

```python
print("This is a python snippet — display only.")
```

```rust
fn main() {
    println!("Rust snippet — also display only.");
}
```

## Markdown caveats

Inline formatting is minimal: `# / ## / ###` for headings, `**bold**`,
`*italic*`, and `` `inline code` ``. Tables, nested lists and images are
*not* rendered — they'll appear as literal markdown text. The goal is
runnable cells, not a full markdown reader.
