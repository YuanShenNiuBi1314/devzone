//! Plugin interface (build guide section 6.7) — out-of-process event dispatch.

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::events::Event;

/// `plugin.toml` manifest (section 6.7.3).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PluginManifest {
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub events: Vec<String>,
    /// Script or executable relative to the plugin directory; any language.
    pub command: String,
    #[serde(default)]
    pub timeout_sec: u64,
}

#[derive(Clone, Debug)]
pub struct PluginOutcome {
    pub name: String,
    pub duration_ms: u64,
    pub error: Option<String>,
}

/// Load one plugin manifest (validates required fields).
pub fn read_manifest(dir: &Path) -> Result<PluginManifest> {
    let path = dir.join("plugin.toml");
    let raw = std::fs::read_to_string(&path)?;
    let manifest: PluginManifest = toml::from_str(&raw)?;
    if manifest.name.trim().is_empty() || manifest.command.trim().is_empty() {
        return Err(anyhow::anyhow!("{path:?} 缺少 name 或 command"));
    }
    Ok(manifest)
}

/// Every plugin directory that parses, in deterministic order.
pub fn list_installed(plugin_dir: &Path) -> Vec<(PathBuf, PluginManifest)> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(plugin_dir) else {
        return out;
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .filter_map(|e| e.path().is_dir().then_some(e.path()))
        .collect();
    dirs.sort();
    for dir in dirs {
        match read_manifest(&dir) {
            Ok(manifest) => out.push((dir, manifest)),
            Err(err) => tracing::warn!("skip plugin {}: {err}", dir.display()),
        }
    }
    out
}

/// Run the subscribers for one event: stdin gets the full payload JSON,
/// stdout is ignored, non-zero exit or timeout is recorded as a failure
/// (section 6.7.4). Parallelism is capped at 2.
pub fn dispatch(plugin_dir: &Path, event: &Event, default_timeout_sec: u64) -> Vec<PluginOutcome> {
    let subscribers: Vec<(PathBuf, PluginManifest)> = list_installed(plugin_dir)
        .into_iter()
        .filter(|(_, manifest)| manifest.events.iter().any(|e| e == &event.name))
        .collect();
    if subscribers.is_empty() {
        return Vec::new();
    }
    let payload = serde_json::to_vec(&serde_json::json!({
        "event": event.name, "ts": event.ts, "payload": event.payload,
    }))
    .unwrap_or_else(|_| b"{}".to_vec());
    let mut outcomes = Vec::new();
    for chunk in subscribers.chunks(2) {
        let handles: Vec<_> = chunk
            .iter()
            .map(|(dir, manifest)| {
                let dir = dir.clone();
                let manifest = manifest.clone();
                let payload = payload.clone();
                std::thread::spawn(move || run_one(&dir, &manifest, &payload, default_timeout_sec))
            })
            .collect();
        for handle in handles {
            outcomes.push(match handle.join() {
                Ok(outcome) => outcome,
                Err(_) => PluginOutcome {
                    name: "unknown".to_string(),
                    duration_ms: 0,
                    error: Some("plugin thread panicked".to_string()),
                },
            });
        }
    }
    outcomes
}

/// Scripts are not executable images on Windows: route `.ps1`, `.cmd`, `.bat`
/// through their interpreter, everything else runs directly.
fn interpreter_for(program: &Path) -> (String, Vec<String>) {
    let extension = program
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    match extension.as_str() {
        "ps1" => (
            "powershell.exe".to_string(),
            vec![
                "-NoProfile".to_string(),
                "-ExecutionPolicy".to_string(),
                "Bypass".to_string(),
                "-File".to_string(),
            ],
        ),
        "cmd" | "bat" => ("cmd.exe".to_string(), vec!["/C".to_string()]),
        _ => (program.display().to_string(), Vec::new()),
    }
}

fn run_one(
    dir: &Path,
    manifest: &PluginManifest,
    payload: &[u8],
    default_timeout_sec: u64,
) -> PluginOutcome {
    use std::io::Write as _;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let started = Instant::now();
    let timeout = Duration::from_secs(if manifest.timeout_sec > 0 {
        manifest.timeout_sec
    } else {
        default_timeout_sec
    });
    let program = dir.join(&manifest.command);
    let (launcher, prefix) = interpreter_for(&program);
    let mut child = match Command::new(&launcher)
        .args(&prefix)
        .arg(&program)
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(err) => {
            return PluginOutcome {
                name: manifest.name.clone(),
                duration_ms: 0,
                error: Some(format!("启动 {} 失败：{err}", program.display())),
            }
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(payload);
    }
    let outcome_loop = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if started.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(err) => {
                return PluginOutcome {
                    name: manifest.name.clone(),
                    duration_ms: crate::util::elapsed_ms(started),
                    error: Some(format!("wait 失败：{err}")),
                }
            }
        }
    };
    PluginOutcome {
        name: manifest.name.clone(),
        duration_ms: crate::util::elapsed_ms(started),
        error: match outcome_loop {
            None => Some(format!("超过 {timeout:?} 已 kill")),
            Some(status) if status.success() => None,
            Some(status) => Some(format!("退出码 {}", status)),
        },
    }
}

/// `devzone plugin install <dir>`: validate then copy into the plugin root.
pub fn install(source: &Path, plugin_dir: &Path) -> Result<PluginManifest> {
    let manifest = read_manifest(source)?;
    std::fs::create_dir_all(plugin_dir)?;
    let target = plugin_dir.join(&manifest.name);
    if target.exists() {
        std::fs::remove_dir_all(&target)?;
    }
    copy_tree(source, &target)?;
    Ok(manifest)
}

fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let path = entry.path();
        let dest = to.join(entry.file_name());
        if path.is_dir() {
            copy_tree(&path, &dest)?;
        } else {
            std::fs::copy(&path, &dest)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(dir: &Path, name: &str, events: &[&str], body: &str) -> PathBuf {
        let plugin = dir.join(name);
        std::fs::create_dir_all(&plugin).unwrap();
        std::fs::write(
            plugin.join("plugin.toml"),
            format!(
                "name = \"{name}\"\nversion = \"0.1.0\"\ndescription = \"t\"\nevents = [{}]\ncommand = \"run.ps1\"\ntimeout_sec = 5\n",
                events
                    .iter()
                    .map(|e| format!("\"{e}\""))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        )
        .unwrap();
        std::fs::write(plugin.join("run.ps1"), body).unwrap();
        plugin
    }

    #[test]
    fn manifest_roundtrip_and_listing() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("plugins");
        fixture(&root, "alpha", &["power_restored"], "Write-Host ok");
        // a directory without a manifest is skipped, not fatal
        std::fs::create_dir_all(root.join("broken")).unwrap();
        let installed = list_installed(&root);
        assert_eq!(installed.len(), 1);
        let (path, manifest) = &installed[0];
        assert_eq!(manifest.name, "alpha");
        assert_eq!(manifest.events, vec!["power_restored"]);
        assert!(path.ends_with("alpha"));
        assert!(read_manifest(&root.join("broken")).is_err());
    }

    #[test]
    fn dispatch_only_runs_subscribers() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("plugins");
        std::fs::create_dir_all(&root).unwrap();
        fixture(
            &root,
            "watcher",
            &["stall_detected"],
            "$j = [Console]::In.ReadToEnd(); Set-Content -Path (Join-Path $PSScriptRoot 'seen.json') -Value $j",
        );
        fixture(&root, "other", &["zone_alert"], "exit 0");
        // plugin root without any subscriber for `remind` -> no outcomes
        let outcomes = dispatch(&root, &Event::new("remind", serde_json::json!({})), 30);
        assert!(outcomes.is_empty());
        let outcomes = dispatch(
            &root,
            &Event::new("stall_detected", serde_json::json!({"app": "trae.exe"})),
            30,
        );
        assert_eq!(outcomes.len(), 1, "{outcomes:?}");
        assert!(outcomes[0].error.is_none(), "{:?}", outcomes[0].error);
        let seen = root.join("watcher/seen.json");
        assert!(seen.exists(), "plugin did not receive stdin");
        let text = std::fs::read_to_string(&seen).unwrap();
        assert!(text.contains("trae.exe"), "{text}");
        assert!(text.contains("\"event\""), "{text}");
    }

    #[test]
    fn failing_plugin_is_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("plugins");
        std::fs::create_dir_all(&root).unwrap();
        fixture(&root, "loser", &["remind"], "exit 3");
        let outcomes = dispatch(&root, &Event::new("remind", serde_json::json!({})), 30);
        assert_eq!(outcomes.len(), 1);
        let error = outcomes[0].error.clone().unwrap_or_default();
        assert!(
            error.contains("退出码") || error.contains("exit"),
            "{error}"
        );
    }

    #[test]
    fn timeout_kills_the_plugin() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("plugins");
        std::fs::create_dir_all(&root).unwrap();
        let plugin = fixture(&root, "slow", &["remind"], "Start-Sleep -Seconds 30");
        let mut manifest = read_manifest(&plugin).unwrap();
        manifest.timeout_sec = 1;
        let outcome = run_one(&plugin, &manifest, b"{}", manifest.timeout_sec);
        let error = outcome.error.clone().unwrap_or_default();
        assert!(error.contains("kill"), "{error}");
    }

    #[test]
    fn install_copies_tree() {
        let dir = tempfile::tempdir().unwrap();
        let source = fixture(dir.path(), "gift", &["battery_low"], "exit 0");
        let root = dir.path().join("plugins");
        let manifest = install(&source, &root).unwrap();
        assert_eq!(manifest.name, "gift");
        assert!(root.join("gift/plugin.toml").exists());
        assert!(root.join("gift/run.ps1").exists());
        // idempotent: reinstalling replaces the copy
        install(&source, &root).unwrap();
        assert_eq!(list_installed(&root).len(), 1);
    }
}
