//! Parameterised command templates — Warp-style "workflows".
//!
//! A workflow is a YAML file: a name, a description, an optional shell, an
//! optional tag list, a command template with `{{arg}}` placeholders, and a
//! list of named arguments (each with an optional default and description).
//!
//! Files are loaded from `~/.config/jterm1/workflows/*.yaml` plus, optionally,
//! a bundled `scripts/workflows/` directory. Parse failures are logged and
//! skipped — a single broken file should never disable the rest.
//!
//! The render step is intentionally tiny (mustache-style `{{name}}` literal
//! substitution); we don't pull in handlebars, since workflows are short
//! single-command strings and conditionals/loops would add config-language
//! complexity Warp itself avoids.
//!
//! Once loaded, workflows surface in the command palette as a third tier
//! (after actions and history) and via `:` prefix or `Action::OpenWorkflows`.

use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Workflow {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub command: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub args: Vec<WorkflowArg>,
    /// Source file the workflow was loaded from — useful for "edit workflow"
    /// shortcuts later; populated post-deserialize.
    #[serde(skip)]
    pub source_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct WorkflowArg {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub default: Option<String>,
}

/// Load every `*.yaml` / `*.yml` file under the given directories. Missing
/// directories are silently skipped. Returns workflows in (load-order)
/// sequence; the caller is responsible for sorting/deduplication if it cares.
pub(crate) fn load_all(dirs: &[PathBuf]) -> Vec<Workflow> {
    let mut out = Vec::new();
    for dir in dirs {
        if !dir.is_dir() {
            continue;
        }
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(err) => {
                log::warn!("workflows: cannot list {}: {err}", dir.display());
                continue;
            }
        };
        let mut paths: Vec<PathBuf> = entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                p.extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e.eq_ignore_ascii_case("yaml") || e.eq_ignore_ascii_case("yml"))
                    .unwrap_or(false)
            })
            .collect();
        // Deterministic order so two runs with the same files produce the same
        // palette ordering — easier to keep muscle memory.
        paths.sort();
        for path in paths {
            match load_one(&path) {
                Ok(wf) => out.push(wf),
                Err(err) => log::warn!("workflows: skipping {}: {err}", path.display()),
            }
        }
    }
    out
}

fn load_one(path: &Path) -> Result<Workflow, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("read: {e}"))?;
    let mut wf: Workflow = serde_yaml::from_str(&text).map_err(|e| format!("parse: {e}"))?;
    if wf.name.trim().is_empty() {
        return Err("workflow has empty name".to_string());
    }
    if wf.command.trim().is_empty() {
        return Err("workflow has empty command".to_string());
    }
    wf.source_path = Some(path.to_path_buf());
    Ok(wf)
}

/// Standard config dir: `<XDG_CONFIG_HOME>/jterm1/workflows/`.
pub(crate) fn user_workflow_dir() -> PathBuf {
    let base: PathBuf = gtk4::glib::user_config_dir();
    base.join("jterm1").join("workflows")
}

/// Substitute `{{name}}` occurrences with values from `values`. Returns an
/// error listing any unresolved placeholders so the UI can surface them
/// instead of silently emitting a half-rendered command.
pub(crate) fn render(
    workflow: &Workflow,
    values: &HashMap<String, String>,
) -> Result<String, String> {
    let mut out = String::with_capacity(workflow.command.len());
    let bytes = workflow.command.as_bytes();
    let mut i = 0;
    let mut missing: Vec<String> = Vec::new();
    while i < bytes.len() {
        if i + 1 < bytes.len() && bytes[i] == b'{' && bytes[i + 1] == b'{' {
            // Scan for closing `}}`.
            if let Some(end) = find_close(bytes, i + 2) {
                let name = std::str::from_utf8(&bytes[i + 2..end])
                    .map_err(|e| format!("invalid utf-8 in placeholder: {e}"))?
                    .trim();
                match values.get(name) {
                    Some(v) => out.push_str(v),
                    None => {
                        // Fall through to default if the workflow declared one,
                        // so partial UIs still work for power users invoking
                        // render() directly.
                        if let Some(arg) = workflow.args.iter().find(|a| a.name == name) {
                            if let Some(d) = &arg.default {
                                out.push_str(d);
                            } else if !missing.contains(&name.to_string()) {
                                missing.push(name.to_string());
                            }
                        } else if !missing.contains(&name.to_string()) {
                            missing.push(name.to_string());
                        }
                    }
                }
                i = end + 2;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    if !missing.is_empty() {
        return Err(format!("missing values: {}", missing.join(", ")));
    }
    Ok(out)
}

fn find_close(bytes: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i + 1 < bytes.len() {
        if bytes[i] == b'}' && bytes[i + 1] == b'}' {
            return Some(i);
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wf(name: &str, command: &str, args: &[(&str, Option<&str>)]) -> Workflow {
        Workflow {
            name: name.to_string(),
            description: String::new(),
            command: command.to_string(),
            tags: Vec::new(),
            args: args
                .iter()
                .map(|(n, d)| WorkflowArg {
                    name: n.to_string(),
                    description: String::new(),
                    default: d.map(|s| s.to_string()),
                })
                .collect(),
            source_path: None,
        }
    }

    #[test]
    fn render_substitutes_single_placeholder() {
        let w = wf("t", "git rebase -i {{target}}", &[("target", None)]);
        let mut v = HashMap::new();
        v.insert("target".to_string(), "origin/main".to_string());
        assert_eq!(render(&w, &v).unwrap(), "git rebase -i origin/main");
    }

    #[test]
    fn render_uses_declared_default_when_value_missing() {
        let w = wf(
            "t",
            "echo {{greeting}} {{name}}",
            &[("greeting", Some("hi")), ("name", Some("world"))],
        );
        let v = HashMap::new();
        assert_eq!(render(&w, &v).unwrap(), "echo hi world");
    }

    #[test]
    fn render_reports_missing_placeholder() {
        let w = wf("t", "kill -9 {{pid}}", &[("pid", None)]);
        let v = HashMap::new();
        let err = render(&w, &v).unwrap_err();
        assert!(err.contains("pid"), "got {err}");
    }

    #[test]
    fn render_leaves_unterminated_braces_alone() {
        let w = wf("t", "echo {{not_closed", &[]);
        let v = HashMap::new();
        // Without a closing `}}` we treat the rest as literal text rather than
        // erroring — keeps the failure mode predictable.
        assert_eq!(render(&w, &v).unwrap(), "echo {{not_closed");
    }

    #[test]
    fn render_handles_multiple_occurrences_of_same_arg() {
        let w = wf("t", "cp {{f}} {{f}}.bak", &[("f", None)]);
        let mut v = HashMap::new();
        v.insert("f".to_string(), "config.toml".to_string());
        assert_eq!(render(&w, &v).unwrap(), "cp config.toml config.toml.bak");
    }

    #[test]
    fn load_all_skips_invalid_files_but_returns_good_ones() {
        let dir = tempdir();
        std::fs::write(dir.join("a.yaml"), "name: A\ncommand: echo a\n").unwrap();
        std::fs::write(dir.join("b.yaml"), "this: is not a workflow\n").unwrap();
        std::fs::write(dir.join("c.yaml"), "name: C\ncommand: echo c\n").unwrap();
        let loaded = load_all(&[dir.clone()]);
        let names: Vec<&str> = loaded.iter().map(|w| w.name.as_str()).collect();
        assert_eq!(names, vec!["A", "C"], "names actually {:?}", names);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn load_one_rejects_empty_command() {
        let dir = tempdir();
        let p = dir.join("bad.yaml");
        std::fs::write(&p, "name: X\ncommand: \"\"\n").unwrap();
        let err = load_one(&p).unwrap_err();
        assert!(err.contains("empty command"), "got {err}");
        let _ = std::fs::remove_dir_all(dir);
    }

    fn tempdir() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "jterm1-workflows-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}
