//! Battery reading (`GetSystemPowerStatus`) plus the three-level guardian state
//! machine from build guide section 6.3.2 / 6.3.3.
//!
//! The state machine is a pure function of `(state, reading, clock)` so every
//! transition, debounce and the charger-anomaly path can be unit tested with an
//! injected fake battery source, as required by section 11.

use anyhow::Result;
use chrono::{DateTime, Duration, Local};
use serde::Serialize;

use crate::config::GuardCfg;
use crate::events::name;

/// `BatteryLifePercent` / `ACLineStatus` == 255 means "unknown".
pub const UNKNOWN: u8 = 255;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct BatteryInfo {
    /// `Some(true)` AC online, `Some(false)` on battery, `None` unknown.
    pub ac: Option<bool>,
    /// 0..=100, `None` when the API reports 255.
    pub level_pct: Option<u8>,
    /// Raw `BatteryFlag` byte, kept for the audit trail.
    pub battery_flag: Option<u8>,
    /// `BatteryLifeTime` in minutes when the firmware reports it.
    pub life_min: Option<i64>,
    /// Charging bit of `BatteryFlag` (BATTERY_CHARGING = 0x4).
    pub charging: Option<bool>,
}

impl BatteryInfo {
    /// Unknown readings keep the previous state (section 9, first row).
    pub fn is_unknown(&self) -> bool {
        self.level_pct.is_none() && self.ac.is_none()
    }

    pub fn level_or_unknown(&self) -> i64 {
        self.level_pct.map(i64::from).unwrap_or(-1)
    }

    pub fn ac_i64(&self) -> i64 {
        match self.ac {
            Some(true) => 1,
            Some(false) => 0,
            None => -1,
        }
    }
}

/// Anything that can produce a battery reading; tests inject a scripted mock.
pub trait BatterySource: Send + Sync {
    fn read(&self) -> BatteryInfo;
}

pub struct SystemBattery;

impl BatterySource for SystemBattery {
    fn read(&self) -> BatteryInfo {
        read_system_power_status()
    }
}

/// Thin wrapper over `GetSystemPowerStatus`; 255 values become `None`.
pub fn read_system_power_status() -> BatteryInfo {
    use windows::Win32::System::Power::GetSystemPowerStatus;

    let mut status = windows::Win32::System::Power::SYSTEM_POWER_STATUS::default();
    let ok = unsafe { GetSystemPowerStatus(&mut status) };
    if ok.is_err() {
        tracing::debug!("GetSystemPowerStatus failed: {ok:?}");
        return BatteryInfo::default();
    }
    let flag = Some(status.BatteryFlag);
    BatteryInfo {
        ac: match status.ACLineStatus {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        },
        level_pct: if status.BatteryLifePercent == UNKNOWN || status.BatteryLifePercent > 100 {
            None
        } else {
            Some(status.BatteryLifePercent)
        },
        battery_flag: flag,
        life_min: if status.BatteryLifeTime == u32::MAX || status.BatteryLifeTime == 0 {
            None
        } else {
            Some(i64::from(status.BatteryLifeTime / 60))
        },
        charging: flag.map(|f| f & 0x4 != 0),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum GuardState {
    Normal,
    Warn,
    PausedWait,
    Shutdown,
}

/// Something the resident loop must do after a transition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Emit an event on the bus with the documented payload.
    Emit {
        event: &'static str,
        payload: serde_json::Value,
    },
    /// Run the graceful-pause ladder for the whitelisted apps (6.3.6).
    GracefulPause,
    /// Enter the power-saving wait window (6.3.7).
    EnterPowerSaving,
    /// Reverse the power-saving changes (6.3.7 item 5).
    RestorePowerSettings,
    /// Local auto resume: relaunch the checkpoint manifest (6.3.8).
    ResumeLocal,
    /// `shutdown.exe /s /t 30` after the `before_shutdown` event.
    Shutdown { reason: &'static str },
    /// tier2 strategy: hibernate/sleep instead of waiting on battery.
    SleepInsteadOfWait,
    /// Nothing to do, just record why.
    Log(String),
}

#[derive(Clone, Debug)]
struct Debounce {
    last: Option<DateTime<Local>>,
    window: Duration,
}

impl Debounce {
    fn new(window: Duration) -> Debounce {
        Debounce { last: None, window }
    }

    /// True (and stamps the clock) when the event may fire now.
    fn allow(&mut self, now: DateTime<Local>) -> bool {
        match self.last {
            Some(previous) if now - previous < self.window => false,
            _ => {
                self.last = Some(now);
                true
            }
        }
    }
}

pub struct Guardian {
    cfg: GuardCfg,
    state: GuardState,
    previous_level: Option<u8>,
    previous_at: Option<DateTime<Local>>,
    /// When the AC-online-but-discharging decline started.
    anomaly_since: Option<DateTime<Local>>,
    anomaly_active: bool,
    wait_started_at: Option<DateTime<Local>>,
    low_debounce: Debounce,
    restored_debounce: Debounce,
    /// Last reading that was not "unknown", for `guard status`.
    last_info: BatteryInfo,
}

impl Guardian {
    pub fn new(cfg: &GuardCfg) -> Guardian {
        Guardian {
            cfg: cfg.clone(),
            state: GuardState::Normal,
            previous_level: None,
            previous_at: None,
            anomaly_since: None,
            anomaly_active: false,
            wait_started_at: None,
            low_debounce: Debounce::new(Duration::minutes(60)),
            restored_debounce: Debounce::new(Duration::minutes(10)),
            last_info: BatteryInfo::default(),
        }
    }

    pub fn state(&self) -> GuardState {
        self.state
    }

    pub fn set_state(&mut self, state: GuardState) {
        self.state = state;
    }

    pub fn last_info(&self) -> BatteryInfo {
        self.last_info
    }

    pub fn is_anomaly_active(&self) -> bool {
        self.anomaly_active
    }

    /// Feed one poll result. Returns the actions the caller must execute, in order.
    pub fn on_reading(&mut self, info: &BatteryInfo, now: DateTime<Local>) -> Vec<Action> {
        if !self.cfg.enable {
            self.last_info = *info;
            return Vec::new();
        }
        if info.is_unknown() {
            // Section 9: keep the previous state, only log.
            tracing::warn!("battery reading unknown, keeping state {:?}", self.state);
            return vec![Action::Log(
                "电池状态未知（API 返回 255），维持上一状态".to_string(),
            )];
        }
        self.last_info = *info;

        let level = match info.level_pct {
            Some(level) => level,
            None => {
                return vec![Action::Log(
                    "电量百分比不可用（台式机或驱动未上报）".to_string(),
                )]
            }
        };
        let on_ac = info.ac.unwrap_or(false);

        let mut actions = Vec::new();
        self.track_charger_anomaly(level, on_ac, info.charging, now, &mut actions);

        // A single poll can walk several rows of the transition table: a
        // freshly started guardian that sees 12% must reach PAUSED_WAIT at once.
        for _ in 0..4 {
            let before = self.state;
            actions.extend(self.step(level, on_ac, now));
            if self.state == before || self.state == GuardState::Shutdown {
                break;
            }
        }

        self.previous_level = Some(level);
        self.previous_at = Some(now);
        actions
    }

    /// One row of the section 6.3.2 transition table, from the current state.
    fn step(&mut self, level: u8, on_ac: bool, now: DateTime<Local>) -> Vec<Action> {
        let mut actions = Vec::new();
        // 任意 -> WARN: a confirmed charger anomaly drops into the warn posture.
        if self.anomaly_active && self.state == GuardState::Normal {
            self.state = GuardState::Warn;
            if let Some(event) = self.low_battery_event(level, on_ac, now) {
                actions.push(event);
            }
            return actions;
        }
        match self.state {
            GuardState::Normal => {
                if level < self.cfg.warn_level {
                    self.state = GuardState::Warn;
                    if let Some(event) = self.low_battery_event(level, on_ac, now) {
                        actions.push(event);
                    }
                }
            }
            GuardState::Warn => {
                if level < self.cfg.pause_level || self.anomaly_active {
                    self.state = GuardState::PausedWait;
                    self.wait_started_at = Some(now);
                    actions.push(self.critical_event(level, on_ac));
                    actions.extend(self.enter_pause_sequence());
                } else if level >= self.cfg.warn_level
                    && on_ac
                    && self.last_info.charging != Some(false)
                {
                    // charging back out of warn
                    self.state = GuardState::Normal;
                    if self.restored_debounce.allow(now) {
                        actions.push(Action::Emit {
                            event: name::POWER_RESTORED,
                            payload: serde_json::json!({ "level": level, "source": "warn_exit" }),
                        });
                    }
                }
            }
            GuardState::PausedWait => {
                if level < self.cfg.hard_level {
                    self.state = GuardState::Shutdown;
                    actions.push(Action::Emit {
                        event: name::BEFORE_SHUTDOWN,
                        payload: serde_json::json!({
                            "reason": "battery_critical", "pending": true, "level": level
                        }),
                    });
                    actions.push(Action::Shutdown {
                        reason: "battery_critical",
                    });
                    return actions;
                }
                let waited_too_long = self
                    .wait_started_at
                    .map(|start| now - start >= Duration::minutes(i64::from(self.cfg.max_wait_min)))
                    .unwrap_or(false);
                // Resume needs current to actually flow: AC alone is not enough
                // while the charger is resting (section 6.3.3).
                let charging_now = self.last_info.charging == Some(true)
                    || (on_ac && self.last_info.charging.is_none() && !self.anomaly_active);
                if level >= self.cfg.resume_level && charging_now {
                    self.state = GuardState::Normal;
                    self.wait_started_at = None;
                    self.anomaly_since = None;
                    self.anomaly_active = false;
                    actions.push(Action::RestorePowerSettings);
                    if self.restored_debounce.allow(now) {
                        actions.push(Action::Emit {
                            event: name::POWER_RESTORED,
                            payload: serde_json::json!({ "level": level }),
                        });
                    }
                    actions.push(Action::ResumeLocal);
                } else if waited_too_long {
                    self.state = GuardState::Shutdown;
                    actions.push(Action::Emit {
                        event: name::BEFORE_SHUTDOWN,
                        payload: serde_json::json!({
                            "reason": "battery_critical",
                            "pending": true,
                            "level": level,
                            "waited_min": self.cfg.max_wait_min
                        }),
                    });
                    actions.push(Action::Shutdown {
                        reason: "battery_critical",
                    });
                    return actions;
                }
            }
            GuardState::Shutdown => {
                actions.push(Action::Log("已处于关机序列，忽略后续读数".to_string()));
            }
        }
        actions
    }

    /// `battery_low` is debounced to one notification per 60 minutes (6.3.2),
    /// while the state transition itself always happens.
    fn low_battery_event(
        &mut self,
        level: u8,
        on_ac: bool,
        now: DateTime<Local>,
    ) -> Option<Action> {
        let reason = if self.anomaly_active {
            "charger_anomaly"
        } else {
            "level"
        };
        if !self.low_debounce.allow(now) {
            tracing::debug!("battery_low suppressed by 60min debounce");
            return None;
        }
        Some(Action::Emit {
            event: name::BATTERY_LOW,
            payload: serde_json::json!({ "level": level, "ac": on_ac, "reason": reason }),
        })
    }

    fn critical_event(&self, level: u8, on_ac: bool) -> Action {
        Action::Emit {
            event: name::BATTERY_CRITICAL,
            payload: serde_json::json!({ "level": level, "ac": on_ac, "action": "pause" }),
        }
    }

    /// Graceful pause -> checkpoint layers -> power saving wait (or tier2/tier3).
    fn enter_pause_sequence(&mut self) -> Vec<Action> {
        let mut actions = Vec::new();
        if !self.cfg.pause_enabled {
            actions.push(Action::Log(
                "guard.pause_enabled=false：跳过优雅暂停，直接进入省电等待".to_string(),
            ));
        } else {
            actions.push(Action::GracefulPause);
        }
        match self.cfg.strategy.as_str() {
            "tier2" => {
                actions.push(Action::SleepInsteadOfWait);
                actions.push(Action::Log(
                    "tier2：睡眠 + 唤醒定时器（睡死与继续耗电风险见 README）".to_string(),
                ));
            }
            "tier3" => {
                actions.push(Action::Log(
                    "tier3：建议把长任务迁到云端 VM / CI，本机只做监测与通知".to_string(),
                ));
            }
            _ if self.cfg.power_saving.enable => actions.push(Action::EnterPowerSaving),
            _ => actions.push(Action::Log(
                "guard.power_saving.enable=false：跳过省电等待".to_string(),
            )),
        }
        actions
    }

    /// AC online but the level keeps falling -> "charger resting" (section 6.3.3).
    fn track_charger_anomaly(
        &mut self,
        level: u8,
        on_ac: bool,
        charging: Option<bool>,
        now: DateTime<Local>,
        actions: &mut Vec<Action>,
    ) {
        if !on_ac {
            self.anomaly_since = None;
            if self.anomaly_active && level >= self.cfg.resume_level {
                self.anomaly_active = false;
            }
            return;
        }
        let declining = self
            .previous_level
            .map(|previous| level.saturating_add(1) <= previous)
            .unwrap_or(false)
            && charging != Some(true);
        if declining {
            let start = *self.anomaly_since.get_or_insert(now);
            if now - start >= Duration::minutes(i64::from(self.cfg.charger_anomaly_min))
                && !self.anomaly_active
            {
                {
                    self.anomaly_active = true;
                    if let Some(event) = self.low_battery_event(level, on_ac, now) {
                        actions.push(event);
                    }
                    actions.push(Action::Log(
                        "插着电但电量持续下降，可能充电器过热/故障，请检查".to_string(),
                    ));
                }
            }
        } else {
            self.anomaly_since = None;
            if self.anomaly_active && charging == Some(true) {
                self.anomaly_active = false;
                actions.push(Action::Emit {
                    event: name::POWER_RESTORED,
                    payload: serde_json::json!({ "level": level, "source": "charger_recovered" }),
                });
            }
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct GuardianStatus {
    pub state: GuardState,
    pub level_pct: Option<u8>,
    pub ac: Option<bool>,
    pub charging: Option<bool>,
    pub charger_anomaly: bool,
    pub thresholds: Thresholds,
}

#[derive(Clone, Debug, Serialize)]
pub struct Thresholds {
    pub warn_level: u8,
    pub pause_level: u8,
    pub hard_level: u8,
    pub resume_level: u8,
    pub max_wait_min: u32,
    pub strategy: String,
}

impl Guardian {
    pub fn status(&self) -> GuardianStatus {
        GuardianStatus {
            state: self.state,
            level_pct: self.last_info.level_pct,
            ac: self.last_info.ac,
            charging: self.last_info.charging,
            charger_anomaly: self.anomaly_active,
            thresholds: Thresholds {
                warn_level: self.cfg.warn_level,
                pause_level: self.cfg.pause_level,
                hard_level: self.cfg.hard_level,
                resume_level: self.cfg.resume_level,
                max_wait_min: self.cfg.max_wait_min,
                strategy: self.cfg.strategy.clone(),
            },
        }
    }
}

/// A scripted battery source for tests.
#[cfg(test)]
pub struct ScriptedBattery {
    steps: std::sync::Mutex<std::vec::IntoIter<BatteryInfo>>,
    last: std::sync::Mutex<BatteryInfo>,
}

#[cfg(test)]
impl ScriptedBattery {
    pub fn new(steps: Vec<BatteryInfo>) -> ScriptedBattery {
        ScriptedBattery {
            steps: std::sync::Mutex::new(steps.into_iter()),
            last: std::sync::Mutex::new(BatteryInfo::default()),
        }
    }
}

#[cfg(test)]
impl BatterySource for ScriptedBattery {
    fn read(&self) -> BatteryInfo {
        let mut steps = self.steps.lock().unwrap();
        let info = steps.next().unwrap_or_else(|| *self.last.lock().unwrap());
        *self.last.lock().unwrap() = info;
        info
    }
}

/// Convenience constructor used by the state-machine tests.
#[cfg(test)]
pub fn battery_info(ac: Option<bool>, level: Option<u8>, charging: Option<bool>) -> BatteryInfo {
    BatteryInfo {
        ac,
        level_pct: level,
        battery_flag: level.map(|_| 1),
        life_min: None,
        charging,
    }
}

pub fn verify_source(source: &dyn BatterySource) -> Result<()> {
    let info = source.read();
    if info.level_pct.is_none() && info.ac.is_none() {
        return Err(anyhow::anyhow!(
            "GetSystemPowerStatus 未返回有效值（255/未知）"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn guardian(raw: &str) -> Guardian {
        let cfg = Config::assemble(raw).unwrap();
        Guardian::new(&cfg.guard)
    }

    use chrono::TimeZone as _;

    fn at(minute: i64) -> DateTime<Local> {
        let base = Local.with_ymd_and_hms(2026, 10, 10, 20, 0, 0).unwrap();
        base + Duration::minutes(minute)
    }

    fn events(actions: &[Action]) -> Vec<(&'static str, serde_json::Value)> {
        actions
            .iter()
            .filter_map(|a| match a {
                Action::Emit { event, payload } => Some((*event, payload.clone())),
                _ => None,
            })
            .collect()
    }

    fn kinds(actions: &[Action]) -> Vec<&'static str> {
        actions
            .iter()
            .map(|a| match a {
                Action::Emit { .. } => "emit",
                Action::GracefulPause => "graceful_pause",
                Action::EnterPowerSaving => "enter_power_saving",
                Action::RestorePowerSettings => "restore_power_settings",
                Action::ResumeLocal => "resume_local",
                Action::Shutdown { .. } => "shutdown",
                Action::SleepInsteadOfWait => "sleep",
                Action::Log(_) => "log",
            })
            .collect()
    }

    #[test]
    fn normal_to_warn_emits_battery_low_once_per_hour() {
        let mut g = guardian("[profile]\nactive = \"mid\"\n");
        let actions = g.on_reading(&battery_info(Some(false), Some(19), Some(false)), at(0));
        let fired = events(&actions);
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].0, name::BATTERY_LOW);
        assert_eq!(fired[0].1["level"], 19);
        assert_eq!(fired[0].1["reason"], "level");
        assert_eq!(g.state(), GuardState::Warn);
        // a second drop inside the 60 minute window still transitions but must
        // not spam a new notification
        let mut second = guardian("[profile]\nactive = \"mid\"\n");
        second.on_reading(&battery_info(Some(false), Some(19), Some(false)), at(0));
        let again = second.on_reading(&battery_info(Some(false), Some(18), Some(false)), at(5));
        assert!(again.is_empty() || events(&again).is_empty(), "{again:?}");
    }

    #[test]
    fn warn_to_paused_wait_runs_pause_then_power_saving() {
        let mut g = guardian("[profile]\nactive = \"mid\"\n");
        g.on_reading(&battery_info(Some(false), Some(19), Some(false)), at(0));
        let actions = g.on_reading(&battery_info(Some(false), Some(14), Some(false)), at(1));
        assert_eq!(g.state(), GuardState::PausedWait);
        let names = kinds(&actions);
        let critical = events(&actions);
        assert_eq!(critical[0].0, name::BATTERY_CRITICAL);
        assert_eq!(critical[0].1["action"], "pause");
        assert!(names.contains(&"graceful_pause"), "{names:?}");
        assert!(names.contains(&"enter_power_saving"), "{names:?}");
    }

    #[test]
    fn paused_wait_to_shutdown_on_hard_level() {
        let mut g = guardian("[profile]\nactive = \"mid\"\n");
        g.on_reading(&battery_info(Some(false), Some(19), Some(false)), at(0));
        g.on_reading(&battery_info(Some(false), Some(14), Some(false)), at(1));
        let actions = g.on_reading(&battery_info(Some(false), Some(4), Some(false)), at(2));
        assert_eq!(g.state(), GuardState::Shutdown);
        let fired = events(&actions);
        assert_eq!(fired[0].0, name::BEFORE_SHUTDOWN);
        assert_eq!(fired[0].1["reason"], "battery_critical");
        assert_eq!(fired[0].1["pending"], true);
        assert!(matches!(
            actions.last().unwrap(),
            Action::Shutdown {
                reason: "battery_critical"
            }
        ));
    }

    #[test]
    fn max_wait_expiry_shuts_down_even_above_hard_level() {
        let mut g = guardian("[profile]\nactive = \"mid\"\n");
        g.on_reading(&battery_info(Some(false), Some(14), Some(false)), at(0));
        assert_eq!(g.state(), GuardState::PausedWait);
        let actions = g.on_reading(&battery_info(Some(false), Some(16), Some(false)), at(46));
        assert_eq!(g.state(), GuardState::Shutdown);
        assert!(kinds(&actions).contains(&"shutdown"));
        // 45 minutes is still inside the window
        let mut g2 = guardian("[profile]\nactive = \"mid\"\n");
        g2.on_reading(&battery_info(Some(false), Some(14), Some(false)), at(0));
        let mid = g2.on_reading(&battery_info(Some(false), Some(16), Some(false)), at(44));
        assert_eq!(g2.state(), GuardState::PausedWait);
        assert!(mid.is_empty(), "{mid:?}");
    }

    #[test]
    fn resume_at_threshold_restores_then_relaunches() {
        let mut g = guardian("[profile]\nactive = \"mid\"\n");
        g.on_reading(&battery_info(Some(false), Some(14), Some(false)), at(0));
        assert_eq!(g.state(), GuardState::PausedWait);
        let actions = g.on_reading(&battery_info(Some(true), Some(21), Some(true)), at(10));
        assert_eq!(g.state(), GuardState::Normal);
        let names = kinds(&actions);
        assert!(names.contains(&"restore_power_settings"), "{names:?}");
        assert!(names.contains(&"resume_local"), "{names:?}");
        let fired = events(&actions);
        assert_eq!(fired[0].0, name::POWER_RESTORED);
        assert_eq!(fired[0].1["level"], 21);
    }

    #[test]
    fn resume_below_threshold_keeps_waiting() {
        let mut g = guardian("[profile]\nactive = \"mid\"\n");
        g.on_reading(&battery_info(Some(false), Some(14), Some(false)), at(0));
        let actions = g.on_reading(&battery_info(Some(true), Some(18), Some(true)), at(5));
        assert_eq!(g.state(), GuardState::PausedWait);
        assert!(actions.is_empty(), "{actions:?}");
    }

    #[test]
    fn warn_returns_to_normal_while_charging() {
        let mut g = guardian("[profile]\nactive = \"mid\"\n");
        g.on_reading(&battery_info(Some(false), Some(19), Some(false)), at(0));
        assert_eq!(g.state(), GuardState::Warn);
        let actions = g.on_reading(&battery_info(Some(true), Some(21), Some(true)), at(3));
        assert_eq!(g.state(), GuardState::Normal);
        assert_eq!(events(&actions)[0].0, name::POWER_RESTORED);
    }

    #[test]
    fn power_restored_debounce_is_ten_minutes() {
        let mut g = guardian("[profile]\nactive = \"mid\"\n");
        g.on_reading(&battery_info(Some(false), Some(19), Some(false)), at(0));
        let first = g.on_reading(&battery_info(Some(true), Some(21), Some(true)), at(1));
        assert!(!events(&first).is_empty());
        // drop back into warn then recover again inside the 10 minute window
        g.on_reading(&battery_info(Some(false), Some(19), Some(false)), at(2));
        let second = g.on_reading(&battery_info(Some(true), Some(21), Some(true)), at(4));
        assert!(events(&second).is_empty(), "{second:?}");
        assert_eq!(g.state(), GuardState::Normal);
        g.on_reading(&battery_info(Some(false), Some(19), Some(false)), at(5));
        let third = g.on_reading(&battery_info(Some(true), Some(21), Some(true)), at(12));
        assert_eq!(events(&third).len(), 1);
    }

    #[test]
    fn charger_anomaly_needs_sustained_decline() {
        let mut g = guardian("[profile]\nactive = \"mid\"\n");
        // AC online, level falling every minute
        let mut fired = Vec::new();
        for minute in 0..12 {
            let actions = g.on_reading(
                &battery_info(Some(true), Some(80 - minute as u8), Some(false)),
                at(minute),
            );
            fired.extend(events(&actions));
        }
        assert!(g.is_anomaly_active());
        assert!(
            fired
                .iter()
                .any(|(event, payload)| *event == name::BATTERY_LOW
                    && payload["reason"] == "charger_anomaly"),
            "{fired:?}"
        );
    }

    #[test]
    fn charger_recovers_back_to_power_restored() {
        let mut g = guardian("[profile]\nactive = \"mid\"\n");
        for minute in 0..12 {
            g.on_reading(
                &battery_info(Some(true), Some(80 - minute as u8), Some(false)),
                at(minute),
            );
        }
        assert!(g.is_anomaly_active());
        let actions = g.on_reading(&battery_info(Some(true), Some(70), Some(true)), at(13));
        assert!(!g.is_anomaly_active());
        assert!(events(&actions)
            .iter()
            .any(|(event, _)| *event == name::POWER_RESTORED));
    }

    #[test]
    fn unknown_reading_keeps_state() {
        let mut g = guardian("[profile]\nactive = \"mid\"\n");
        g.on_reading(&battery_info(Some(false), Some(14), Some(false)), at(0));
        let before = g.state();
        let actions = g.on_reading(&BatteryInfo::default(), at(1));
        assert_eq!(g.state(), before);
        assert_eq!(kinds(&actions), vec!["log"]);
    }

    #[test]
    fn disabled_guard_does_nothing() {
        let mut g = guardian("[profile]\nactive = \"low\"\n");
        let actions = g.on_reading(&battery_info(Some(false), Some(3), Some(false)), at(0));
        assert!(actions.is_empty());
        assert_eq!(g.state(), GuardState::Normal);
    }

    #[test]
    fn tier2_sleeps_and_tier3_only_logs() {
        let mut g = guardian("[profile]\nactive = \"mid\"\n[guard]\nstrategy = \"tier2\"\n");
        let actions = g.on_reading(&battery_info(Some(false), Some(14), Some(false)), at(0));
        assert!(kinds(&actions).contains(&"sleep"), "{actions:?}");
        assert!(!kinds(&actions).contains(&"enter_power_saving"));
        let mut g3 = guardian("[profile]\nactive = \"mid\"\n[guard]\nstrategy = \"tier3\"\n");
        let actions3 = g3.on_reading(&battery_info(Some(false), Some(14), Some(false)), at(0));
        let names = kinds(&actions3);
        assert!(names.contains(&"graceful_pause"), "{names:?}");
        assert!(names.contains(&"log"), "{names:?}");
        assert!(!names.contains(&"enter_power_saving"), "{names:?}");
        assert_eq!(g3.state(), GuardState::PausedWait);
    }

    #[test]
    fn pause_disabled_skips_the_ladder() {
        let mut g = guardian("[profile]\nactive = \"mid\"\n[guard]\npause_enabled = false\n");
        let actions = g.on_reading(&battery_info(Some(false), Some(14), Some(false)), at(0));
        assert!(!kinds(&actions).contains(&"graceful_pause"), "{actions:?}");
        assert!(kinds(&actions).contains(&"enter_power_saving"));
    }

    #[test]
    fn scripted_source_walks_forward() {
        let source = ScriptedBattery::new(vec![
            battery_info(Some(false), Some(30), Some(false)),
            battery_info(Some(false), Some(20), Some(false)),
        ]);
        assert_eq!(source.read().level_pct, Some(30));
        assert_eq!(source.read().level_pct, Some(20));
        assert_eq!(source.read().level_pct, Some(20));
        assert!(verify_source(&source).is_ok());
        assert!(verify_source(&ScriptedBattery::new(Vec::new())).is_err());
    }

    #[test]
    fn status_snapshot_reports_thresholds() {
        let mut g = guardian("[profile]\nactive = \"mid\"\n");
        g.on_reading(&battery_info(Some(true), Some(55), Some(true)), at(0));
        let status = g.status();
        assert_eq!(status.level_pct, Some(55));
        assert_eq!(status.thresholds.pause_level, 15);
        assert_eq!(status.thresholds.strategy, "tier1");
        assert!(serde_json::to_string(&status)
            .unwrap()
            .contains("\"Normal\""));
    }
}
