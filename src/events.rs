//! Event bus (build guide section 7).
//!
//! Order per event: audit line in `events.jsonl` (at-least-once gate) ->
//! internal handling (sync) -> notification channels (failures logged, never
//! blocking) -> out-of-process plugins (timeout 30s, parallelism <= 2).

use std::collections::HashMap;
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use chrono::Local;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::notify::{self, Notify};
use crate::util::Paths;

/// Event names devzone emits (section 6.7.1).
pub mod name {
    pub const BATTERY_LOW: &str = "battery_low";
    pub const BATTERY_CRITICAL: &str = "battery_critical";
    pub const GRACEFUL_PAUSE_STARTED: &str = "graceful_pause_started";
    pub const GRACEFUL_PAUSE_DONE: &str = "graceful_pause_done";
    pub const BEFORE_SHUTDOWN: &str = "before_shutdown";
    pub const POWER_RESTORED: &str = "power_restored";
    pub const AFTER_RESUME: &str = "after_resume";
    pub const STALL_DETECTED: &str = "stall_detected";
    pub const STALL_RESOLVED: &str = "stall_resolved";
    pub const CREDITS_LOW: &str = "credits_low";
    pub const ZONE_ALERT: &str = "zone_alert";
    pub const REMIND: &str = "remind";
    pub const CRUNCH_STATE: &str = "crunch_state";

    /// Everything `devzone event <name>` documents as valid.
    pub const KNOWN: [&str; 12] = [
        BATTERY_LOW,
        BATTERY_CRITICAL,
        GRACEFUL_PAUSE_STARTED,
        GRACEFUL_PAUSE_DONE,
        BEFORE_SHUTDOWN,
        POWER_RESTORED,
        AFTER_RESUME,
        STALL_DETECTED,
        CREDITS_LOW,
        ZONE_ALERT,
        REMIND,
        CRUNCH_STATE,
    ];
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Event {
    pub name: String,
    pub payload: serde_json::Value,
    pub ts: String,
}

impl Event {
    pub fn new(name: &str, payload: serde_json::Value) -> Event {
        Event {
            name: name.to_string(),
            payload,
            ts: Local::now().to_rfc3339(),
        }
    }

    pub fn known(&self) -> bool {
        name::KNOWN.contains(&self.name.as_str())
    }

    /// Human readable title/body used by every notification channel.
    pub fn render(&self) -> (String, String) {
        let title = match self.name.as_str() {
            name::BATTERY_LOW => "电量偏低，请插电或保存进度",
            name::BATTERY_CRITICAL => "电量临界，准备优雅暂停",
            name::GRACEFUL_PAUSE_STARTED => "正在优雅暂停任务",
            name::GRACEFUL_PAUSE_DONE => "优雅暂停完成",
            name::BEFORE_SHUTDOWN => "30 秒后关机",
            name::POWER_RESTORED => "充电已恢复",
            name::AFTER_RESUME => "本地恢复已完成",
            name::STALL_DETECTED => "AI 疑似停住或在等待提问",
            name::STALL_RESOLVED => "AI 已恢复输出",
            name::CREDITS_LOW => "积分快见底，可能随时停",
            name::ZONE_ALERT => "开发区空间预警",
            name::REMIND => "提醒",
            _ => "devzone 事件",
        };
        (
            title.to_string(),
            format!("event={} payload={}", self.name, self.payload),
        )
    }
}

pub struct EventBus {
    cfg: Config,
    paths: Paths,
    channels: Vec<Box<dyn Notify>>,
    debounce: Mutex<HashMap<String, Instant>>,
}

impl EventBus {
    pub fn new(cfg: &Config, paths: Paths) -> Result<EventBus> {
        Ok(EventBus {
            cfg: cfg.clone(),
            paths,
            channels: notify::build_channels(cfg),
            debounce: Mutex::new(HashMap::new()),
        })
    }

    /// Channels currently active (used by `doctor` and `status`).
    pub fn channel_names(&self) -> Vec<String> {
        self.channels.iter().map(|c| c.name().to_string()).collect()
    }

    pub fn emit(&self, event: &Event) -> Result<()> {
        self.audit(event)
            .context("写入 events.jsonl 失败，事件未派发（至少一次语义）")?;
        self.internal(event);
        self.notify(event);
        self.plugins(event);
        Ok(())
    }

    pub fn emit_json(&self, event_name: &str, payload: serde_json::Value) -> Result<()> {
        self.emit(&Event::new(event_name, payload))
    }

    /// In-process debounce: at most one emission of `key` per `window`.
    pub fn emit_debounced(
        &self,
        event_name: &str,
        payload: serde_json::Value,
        key: &str,
        window: Duration,
    ) -> Result<bool> {
        let now = Instant::now();
        let should = {
            let mut map = self.lock_debounce();
            match map.get(key) {
                Some(last) if last.elapsed() < window => false,
                _ => {
                    map.insert(key.to_string(), now);
                    true
                }
            }
        };
        if !should {
            tracing::debug!("debounced event {event_name} key={key}");
            return Ok(false);
        }
        self.emit_json(event_name, payload)?;
        Ok(true)
    }

    /// Cross-process debounce backed by the audit log (used by `zone_alert`,
    /// which fires from one-shot CLI runs).
    pub fn emit_debounced_by_audit(
        &self,
        event_name: &str,
        payload: serde_json::Value,
        window: Duration,
    ) -> Result<bool> {
        if self.recent_in_audit(event_name, &payload, window)? {
            tracing::info!("zone_alert suppressed by audit window");
            return Ok(false);
        }
        self.emit_json(event_name, payload)?;
        Ok(true)
    }

    fn lock_debounce(&self) -> std::sync::MutexGuard<'_, HashMap<String, Instant>> {
        match self.debounce.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Internal handling is a structured log line for now (section 7 step 1).
    fn internal(&self, event: &Event) {
        tracing::info!(event = %event.name, payload = %event.payload, "event");
    }

    fn notify(&self, event: &Event) {
        let (title, body) = event.render();
        let body = with_handoff_footer(&event.name, &body, &self.cfg);
        if self.channels.is_empty() {
            crate::notify::ConsoleNotify.send(&title, &body).ok();
            return;
        }
        let failures = notify::dispatch(&self.channels, &title, &body);
        for failure in failures {
            tracing::warn!("notify channel failed, console fallback used: {failure}");
        }
    }

    fn plugins(&self, event: &Event) {
        if !self.cfg.plugin.enable {
            return;
        }
        let dir = self.cfg.plugin_dir(&self.paths);
        let outcomes =
            crate::mod_plugin::dispatch(&dir, event, self.cfg.plugin.default_timeout_sec);
        for outcome in outcomes {
            match &outcome.error {
                Some(err) => tracing::warn!("plugin {} failed: {err}", outcome.name),
                None => tracing::info!("plugin {} ok ({}ms)", outcome.name, outcome.duration_ms),
            }
        }
    }

    /// Append one JSON line and flush before dispatching anything else.
    fn audit(&self, event: &Event) -> Result<()> {
        self.paths.ensure_dirs()?;
        let line = serde_json::to_string(event).context("serialize event")?;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.paths.events)
            .with_context(|| format!("open {}", self.paths.events.display()))?;
        writeln!(file, "{line}")
            .with_context(|| format!("append {}", self.paths.events.display()))?;
        file.flush().ok();
        Ok(())
    }

    /// Was a matching event already audited inside the window?
    fn recent_in_audit(
        &self,
        event_name: &str,
        payload: &serde_json::Value,
        window: Duration,
    ) -> Result<bool> {
        if !self.paths.events.exists() {
            return Ok(false);
        }
        let mut file = std::fs::File::open(&self.paths.events)
            .with_context(|| format!("open {}", self.paths.events.display()))?;
        let size = file.metadata()?.len();
        let tail: u64 = 256 * 1024;
        if size > tail {
            file.seek(SeekFrom::End(-(tail as i64)))?;
        }
        let mut buffer = String::new();
        file.read_to_string(&mut buffer)?;
        let cutoff = Local::now() - chrono::Duration::from_std(window)?;
        for line in buffer.lines().rev() {
            let Ok(parsed) = serde_json::from_str::<Event>(line) else {
                continue;
            };
            if parsed.name != event_name {
                continue;
            }
            if !same_alert_target(&parsed.payload, payload) {
                continue;
            }
            let Ok(ts) = chrono::DateTime::parse_from_rfc3339(&parsed.ts) else {
                continue;
            };
            if ts.with_timezone(&Local) >= cutoff {
                return Ok(true);
            }
            return Ok(false);
        }
        Ok(false)
    }

    /// Read the whole audit log (bounded by the tail window) for `status`.
    pub fn recent_events(&self, limit: usize) -> Result<Vec<Event>> {
        if !self.paths.events.exists() {
            return Ok(Vec::new());
        }
        let text = std::fs::read_to_string(&self.paths.events)?;
        Ok(text
            .lines()
            .filter_map(|line| serde_json::from_str::<Event>(line).ok())
            .rev()
            .take(limit)
            .collect())
    }
}

/// Zone alerts debounce per (zone, metric) pair.
fn same_alert_target(a: &serde_json::Value, b: &serde_json::Value) -> bool {
    let key = |v: &serde_json::Value| {
        format!(
            "{}|{}",
            v.get("zone").and_then(|z| z.as_str()).unwrap_or("?"),
            v.get("metric").and_then(|m| m.as_str()).unwrap_or("?")
        )
    };
    key(a) == key(b)
}

/// Mobile hand-off guidance for the events that need a human elsewhere
/// (section 6.5.4 message template).
pub fn with_handoff_footer(event_name: &str, body: &str, cfg: &Config) -> String {
    let needs_handoff = matches!(
        event_name,
        name::STALL_DETECTED | name::CREDITS_LOW | name::POWER_RESTORED | name::AFTER_RESUME
    );
    if !needs_handoff {
        return body.to_string();
    }
    let apps: Vec<String> = if event_name == name::STALL_DETECTED || event_name == name::CREDITS_LOW
    {
        cfg.watchdog
            .adapters
            .keys()
            .cloned()
            .chain(cfg.watchdog.regions.keys().cloned())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect()
    } else {
        cfg.guard
            .whitelist
            .apps
            .keys()
            .map(|app| app.trim_end_matches(".exe").to_string())
            .collect()
    };
    let app_hint = if apps.is_empty() {
        "在手机端打开对应 AI 应用".to_string()
    } else {
        format!("在手机端打开 {}（点继续）", apps.join(" / "))
    };
    format!("{body}\n{app_hint}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DEFAULT_TEMPLATE;

    fn bus(dir: &tempfile::TempDir, raw: &str) -> (EventBus, Paths) {
        let paths = Paths::with_root(dir.path().to_path_buf());
        paths.ensure_dirs().unwrap();
        std::fs::write(&paths.config, raw).unwrap();
        let cfg = Config::load(&paths).unwrap();
        (EventBus::new(&cfg, paths.clone()).unwrap(), paths)
    }

    #[test]
    fn audit_line_is_written_before_dispatch() {
        let dir = tempfile::tempdir().unwrap();
        let (bus, paths) = bus(&dir, DEFAULT_TEMPLATE);
        bus.emit_json(name::REMIND, serde_json::json!({"name": "standup"}))
            .unwrap();
        let text = std::fs::read_to_string(&paths.events).unwrap();
        let parsed: Event = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(parsed.name, "remind");
        assert_eq!(parsed.payload["name"], "standup");
        assert_eq!(bus.recent_events(5).unwrap().len(), 1);
    }

    #[test]
    fn in_process_debounce_window() {
        let dir = tempfile::tempdir().unwrap();
        let (bus, paths) = bus(&dir, DEFAULT_TEMPLATE);
        let fired = bus
            .emit_debounced(
                name::BATTERY_LOW,
                serde_json::json!({"level": 18}),
                "battery_low",
                Duration::from_secs(3600),
            )
            .unwrap();
        assert!(fired);
        let second = bus
            .emit_debounced(
                name::BATTERY_LOW,
                serde_json::json!({"level": 17}),
                "battery_low",
                Duration::from_secs(3600),
            )
            .unwrap();
        assert!(!second);
        assert_eq!(
            std::fs::read_to_string(&paths.events)
                .unwrap()
                .lines()
                .count(),
            1
        );
        let other = bus
            .emit_debounced(
                name::POWER_RESTORED,
                serde_json::json!({"level": 22}),
                "power_restored",
                Duration::from_secs(600),
            )
            .unwrap();
        assert!(other);
    }

    #[test]
    fn audit_debounce_compares_zone_and_metric() {
        let dir = tempfile::tempdir().unwrap();
        let (bus, _) = bus(&dir, DEFAULT_TEMPLATE);
        let first = bus
            .emit_debounced_by_audit(
                name::ZONE_ALERT,
                serde_json::json!({"zone": "work", "metric": "free_gb", "value": 6.2, "threshold": 10}),
                Duration::from_secs(6 * 3600),
            )
            .unwrap();
        assert!(first);
        let suppressed = bus
            .emit_debounced_by_audit(
                name::ZONE_ALERT,
                serde_json::json!({"zone": "work", "metric": "free_gb", "value": 5.9, "threshold": 10}),
                Duration::from_secs(6 * 3600),
            )
            .unwrap();
        assert!(!suppressed);
        let other_zone = bus
            .emit_debounced_by_audit(
                name::ZONE_ALERT,
                serde_json::json!({"zone": "home", "metric": "free_gb", "value": 1.0, "threshold": 10}),
                Duration::from_secs(6 * 3600),
            )
            .unwrap();
        assert!(other_zone);
    }

    #[test]
    fn unknown_event_names_are_flagged() {
        assert!(Event::new(name::ZONE_ALERT, serde_json::json!({})).known());
        assert!(!Event::new("not_a_devzone_event", serde_json::json!({})).known());
    }

    #[test]
    fn handoff_footer_for_stall() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::with_root(dir.path().to_path_buf());
        std::fs::create_dir_all(&paths.config_dir).unwrap();
        std::fs::write(
            &paths.config,
            "[profile]\nactive = \"low\"\n[watchdog.adapters]\ndoubao = { priority = [\"manual\"] }\n",
        )
        .unwrap();
        let cfg = Config::load(&paths).unwrap();
        let body = with_handoff_footer(name::STALL_DETECTED, "base", &cfg);
        assert!(body.contains("doubao"), "{body}");
        assert!(body.contains("点继续"), "{body}");
        assert_eq!(with_handoff_footer(name::ZONE_ALERT, "base", &cfg), "base");
    }

    #[test]
    fn console_only_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let (bus, _) = bus(&dir, DEFAULT_TEMPLATE);
        assert_eq!(bus.channel_names(), vec!["console".to_string()]);
    }

    #[test]
    fn footer_lists_whitelisted_apps_for_resume() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::with_root(dir.path().to_path_buf());
        std::fs::create_dir_all(&paths.config_dir).unwrap();
        std::fs::write(&paths.config, "[profile]\nactive = \"mid\"\n").unwrap();
        let cfg = Config::load(&paths).unwrap();
        let body = with_handoff_footer(name::AFTER_RESUME, "base", &cfg);
        assert!(body.contains("doubao"), "{body}");
    }
}
