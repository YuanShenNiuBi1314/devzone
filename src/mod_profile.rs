//! M6 分层预设（build guide section 6.6 + section 4 / 4.1）。
//!
//! 设计要点：预设层**只在内存里生效**，合并顺序是
//! 内置默认 -> 预设（`[profile].active`）-> 用户文件（逐项覆盖），见
//! [`crate::config::preset_values`] 与 [`Config::assemble`]。因此
//! `profile apply <low|mid|pro>` 唯一写盘的动作是把 `[profile].active`
//! 这一行改到 `devzone.toml`，绝不把预设摊平覆盖用户自定义段。
//!
//! `profile show` 把三层对齐成一张「配置项 / 生效值 / 来源」表，来源取
//! `default` | `preset(<active>)` | `user`（section 6.6 的 diff 标记）。

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context as _, Result};
use serde_json::{json, Value};

use crate::cli::ProfileCmd;
use crate::config;
use crate::config::Config;
use crate::Ctx;

/// 来源标记（section 6.6）。
const SOURCE_DEFAULT: &str = "default";
const SOURCE_USER: &str = "user";

pub fn handle(cmd: &ProfileCmd, ctx: &Ctx, user_raw: &str) -> Result<()> {
    match cmd {
        ProfileCmd::Apply { name } => apply(ctx, name),
        ProfileCmd::Show { json } => show(ctx, user_raw, *json),
    }
}

// ---------------------------------------------------------------------------
// profile apply
// ---------------------------------------------------------------------------

fn apply(ctx: &Ctx, name: &str) -> Result<()> {
    let name = name.trim();
    // 先校验预设名：非法时在任何写盘动作之前就失败（ConfigError::UnknownProfile）。
    let preset = preset_leaves(name)?;

    let before = ctx.cfg.clone();
    let mut editor = config::Editor::open(&ctx.paths)?;
    editor.set("profile", "active", &format!("\"{name}\""))?;
    editor.save(&ctx.paths)?;

    let new_raw = config::read_user_raw(&ctx.paths)?;
    let after = Config::assemble(&new_raw).context("套用预设后重新解析配置")?;
    let user = user_leaves(&new_raw)?;
    let effective = flatten_config(&after)?;

    println!(
        "已套用预设 `{name}`：{} 写入 [profile].active = \"{name}\"",
        ctx.paths.config.display()
    );
    println!("预设层只在内存中生效（内置默认 -> 预设 -> 用户文件），不会被摊平写进 devzone.toml。");

    let controlled: Vec<&str> = preset
        .keys()
        .map(String::as_str)
        .filter(|path| !covered(&user, path))
        .collect();
    println!(
        "\n由预设 `{name}` 控制的项（{} 项，改预设即生效、不落盘）：",
        controlled.len()
    );
    for path in controlled {
        println!("  {path} = {}", render_value(&preset[path]));
    }

    let overridden: Vec<&String> = preset.keys().filter(|path| covered(&user, path)).collect();
    if overridden.is_empty() {
        println!("\n没有用户自定义项被本预设触及（你的 devzone.toml 里没写这些键）。");
    } else {
        println!(
            "\n以下 {} 项你已在 devzone.toml 中显式设置，预设不覆盖、保持不动：",
            overridden.len()
        );
        for path in overridden {
            println!(
                "  {path} = {}（预设 `{name}` 给的是 {}，生效值以你的文件为准）",
                render_value(effective.get(path).unwrap_or(&Value::Null)),
                render_value(&preset[path])
            );
        }
    }

    let changes = diff_configs(&before, &after)?;
    if changes.is_empty() {
        println!("\n本次套用未改变任何生效值（此前生效的预设已经是 `{name}`）。");
    } else {
        println!("\n生效值变化（{} 项）：", changes.len());
        for (path, from, to) in changes {
            println!("  {path}: {from} -> {to}");
        }
    }
    println!("\n下一步：devzone profile show 查看逐项来源，devzone doctor 自检环境。");
    Ok(())
}

// ---------------------------------------------------------------------------
// profile show
// ---------------------------------------------------------------------------

/// 一行「配置项 / 生效值 / 来源」。
pub struct Item {
    pub path: String,
    pub effective: Value,
    pub source: String,
}

/// `profile show` 的数据模型：生效配置的每个叶子键 + 来源标记。
pub struct ShowModel {
    pub active: String,
    /// `[profile].active` 是否由用户文件写死（否则来自内置默认）。
    pub active_pinned: bool,
    pub items: Vec<Item>,
    /// 用户文件里写了、但不在 schema 中因而被忽略的键。
    pub ignored: Vec<String>,
}

impl ShowModel {
    /// 三层（内置默认 / 预设 / 用户文件）按点分路径对齐。
    pub fn build(cfg: &Config, user_raw: &str) -> Result<ShowModel> {
        let active = cfg.profile.active.clone();
        let defaults = flatten_config(&Config::builtin())?;
        let preset = Preset::build(&active)?;
        let user = user_leaves(user_raw)?;
        let effective = flatten_config(cfg)?;

        let mut paths: BTreeSet<String> = BTreeSet::new();
        paths.extend(defaults.keys().cloned());
        paths.extend(preset.leaves.keys().cloned());
        paths.extend(effective.keys().cloned());
        paths.extend(user.iter().cloned());

        let mut items = Vec::new();
        let mut ignored = Vec::new();
        for path in paths {
            let value = effective.get(&path);
            if value.is_none() {
                // 内置的空表占位（如 [zone.zones]）：其子项已单独成行，跳过以免重复。
                if has_child(&effective, &path) {
                    continue;
                }
                // 用户写了但生效配置里没有 -> 不在 schema 内，被 serde 忽略。
                if covered(&user, &path) {
                    ignored.push(path);
                }
                continue;
            }
            let source = if covered(&user, &path) {
                SOURCE_USER.to_string()
            } else if covered(&preset.cover, &path) {
                format!("preset({active})")
            } else if defaults.contains_key(&path) {
                SOURCE_DEFAULT.to_string()
            } else {
                "unknown".to_string()
            };
            items.push(Item {
                path,
                effective: value.expect("checked above").clone(),
                source,
            });
        }

        Ok(ShowModel {
            active_pinned: covered(&user, "profile.active"),
            active,
            items,
            ignored,
        })
    }

    pub fn count_by_source(&self) -> BTreeMap<String, usize> {
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        for item in &self.items {
            *counts.entry(item.source.clone()).or_insert(0) += 1;
        }
        counts
    }

    /// `--json` 的 `data` 载荷（包壳由 `util::print_json` 负责，section 5.3）。
    pub fn to_json(&self, config_path: &Path) -> Value {
        json!({
            "active": self.active,
            "active_pinned": self.active_pinned,
            "config_path": config_path.display().to_string(),
            "items": self
                .items
                .iter()
                .map(|item| json!({
                    "path": item.path,
                    "effective": item.effective,
                    "source": item.source,
                }))
                .collect::<Vec<Value>>(),
            "counts": self
                .count_by_source()
                .iter()
                .map(|(source, n)| (source.clone(), json!(*n)))
                .collect::<serde_json::Map<String, Value>>(),
            "ignored_user_keys": self.ignored,
        })
    }

    pub fn print(&self, config_path: &Path) {
        let width = |text: &Value| display_width(&render_value(text));
        let path_w = self
            .items
            .iter()
            .map(|item| display_width(&item.path))
            .max()
            .unwrap_or(0)
            .max(display_width("配置项"))
            .saturating_add(2);
        let value_w = self
            .items
            .iter()
            .map(|item| width(&item.effective).min(48))
            .max()
            .unwrap_or(0)
            .max(display_width("生效值"))
            .saturating_add(2);

        println!(
            "devzone profile show — 当前生效配置（预设：{}）",
            self.active
        );
        println!("配置文件：{}", config_path.display());
        if !self.active_pinned {
            println!(
                "[i] 你的 devzone.toml 未写 [profile].active，当前 `{}` 来自内置默认；`devzone profile apply {}` 可固定。",
                self.active, self.active
            );
        }
        println!(
            "来源：{}=内置默认，preset({})=预设层（不落盘），{}=用户文件显式设置（优先级最高）",
            SOURCE_DEFAULT, self.active, SOURCE_USER
        );
        println!();
        println!("{}{}来源", pad("配置项", path_w), pad("生效值", value_w));
        for item in &self.items {
            let rendered = render_value(&item.effective);
            let clipped = clip(&rendered, 48);
            println!(
                "{}{}{}",
                pad(&item.path, path_w),
                pad(&clipped, value_w),
                item.source
            );
        }
        let counts = self.count_by_source();
        let summary: Vec<String> = counts
            .iter()
            .map(|(source, n)| format!("{source}={n}"))
            .collect();
        println!("\n合计 {} 项：{}", self.items.len(), summary.join(" / "));
        if !self.ignored.is_empty() {
            println!(
                "[!] devzone.toml 中以下键不在配置 schema 内，已被忽略：{}",
                self.ignored.join(", ")
            );
        }
        println!("提示：预设项不写入 devzone.toml；切换预设时你的自定义项保持原样。");
    }
}

fn show(ctx: &Ctx, user_raw: &str, as_json: bool) -> Result<()> {
    let model = ShowModel::build(&ctx.cfg, user_raw)?;
    if ctx.emit_json("profile show", model.to_json(&ctx.paths.config), as_json)? {
        return Ok(());
    }
    model.print(&ctx.paths.config);
    Ok(())
}

// ---------------------------------------------------------------------------
// 纯函数（供单元测试直接调用）
// ---------------------------------------------------------------------------

/// 预设层：展平后的叶子值 + 覆盖判定用的键集合。
pub struct Preset {
    /// 点分叶子路径 -> 预设给出的值。
    pub leaves: BTreeMap<String, Value>,
    /// `config::preset_values` 的原始键，加上全部叶子路径。一个生效项只要等于
    /// 其中某个键、或落在某个键所代表的表之下，就算「由预设控制」。
    /// 注意不能反过来按祖先匹配，否则 `watchdog.mode` 会让 `watchdog` 下的
    /// 所有默认项都被误标成预设。
    pub cover: BTreeSet<String>,
}

impl Preset {
    /// 构建预设层，同时校验预设名（非法时返回 `ConfigError::UnknownProfile`）。
    pub fn build(name: &str) -> Result<Preset> {
        let patches = config::preset_values(name)?;
        let mut tree = json!({});
        let mut cover: BTreeSet<String> = BTreeSet::new();
        for (path, value) in patches {
            cover.insert(path.to_string());
            insert_dotted(&mut tree, path, value);
        }
        let mut leaves = BTreeMap::new();
        flatten(&tree, "", &mut leaves);
        cover.extend(leaves.keys().cloned());
        Ok(Preset { leaves, cover })
    }
}

/// 预设层展平成「点分路径 -> 值」。同时用于校验预设名。
pub fn preset_leaves(name: &str) -> Result<BTreeMap<String, Value>> {
    Ok(Preset::build(name)?.leaves)
}

/// 用户文件显式设置的叶子键集合（section 6.6 的「被用户覆盖」判定依据）。
pub fn user_leaves(user_raw: &str) -> Result<BTreeSet<String>> {
    Ok(Config::user_override_paths(user_raw)?.into_iter().collect())
}

/// 生效配置展平成「点分路径 -> 值」。
pub fn flatten_config(cfg: &Config) -> Result<BTreeMap<String, Value>> {
    let tree = serde_json::to_value(cfg).context("序列化生效配置")?;
    let mut out = BTreeMap::new();
    flatten(&tree, "", &mut out);
    Ok(out)
}

/// 两份配置的逐项差异（路径、旧值、新值），用于说明预设开关了哪些项。
pub fn diff_configs(before: &Config, after: &Config) -> Result<Vec<(String, String, String)>> {
    let before = flatten_config(before)?;
    let after = flatten_config(after)?;
    let mut paths: BTreeSet<&String> = BTreeSet::new();
    paths.extend(before.keys());
    paths.extend(after.keys());
    let mut out = Vec::new();
    for path in paths {
        let left = before.get(path).cloned().unwrap_or_default();
        let right = after.get(path).cloned().unwrap_or_default();
        if left == right {
            continue;
        }
        // 空表占位（如 [guard.whitelist.apps]）在另一侧展开成了子项：
        // 子项已各自成行，这里不算一次变化。
        if is_empty_placeholder(&left) && has_child(&after, path)
            || is_empty_placeholder(&right) && has_child(&before, path)
        {
            continue;
        }
        out.push((path.clone(), render_value(&left), render_value(&right)));
    }
    Ok(out)
}

fn is_empty_placeholder(value: &Value) -> bool {
    value.as_object().map(|map| map.is_empty()).unwrap_or(false)
}

fn has_child(map: &BTreeMap<String, Value>, path: &str) -> bool {
    let prefix = format!("{path}.");
    map.keys().any(|key| key.starts_with(&prefix))
}

/// `path` 自身或任一点分祖先命中集合即为 true。
fn covered(set: &BTreeSet<String>, path: &str) -> bool {
    let mut prefix = path;
    loop {
        if set.contains(prefix) {
            return true;
        }
        match prefix.rfind('.') {
            Some(idx) => prefix = &prefix[..idx],
            None => return false,
        }
    }
}

fn insert_dotted(tree: &mut Value, dotted: &str, value: Value) {
    let parts: Vec<&str> = dotted.split('.').collect();
    let mut cursor = tree;
    for part in &parts[..parts.len() - 1] {
        let Some(map) = cursor.as_object_mut() else {
            return;
        };
        if !map.get(*part).map(Value::is_object).unwrap_or(false) {
            map.insert((*part).to_string(), json!({}));
        }
        let Some(next) = map.get_mut(*part) else {
            return;
        };
        cursor = next;
    }
    let Some(key) = parts.last() else {
        return;
    };
    if let Some(map) = cursor.as_object_mut() {
        map.insert((*key).to_string(), value);
    }
}

/// 展平成叶子路径：空表也作为一行（例如 `[zone.zones]` 尚无内容时）。
fn flatten(value: &Value, prefix: &str, out: &mut BTreeMap<String, Value>) {
    match value {
        Value::Object(map) if !map.is_empty() => {
            for (key, child) in map {
                let path = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                flatten(child, &path, out);
            }
        }
        other => {
            if !prefix.is_empty() {
                out.insert(prefix.to_string(), other.clone());
            }
        }
    }
}

/// 值渲染成一行可读文本（字符串带引号，数组/对象用紧凑 JSON）。
fn render_value(value: &Value) -> String {
    match value {
        Value::String(text) => format!("\"{text}\""),
        Value::Null => "-".to_string(),
        other => other.to_string(),
    }
}

/// 东亚字符按两列宽计算，保证表格对齐（近似 wcwidth 的常用区间）。
fn display_width(text: &str) -> usize {
    text.chars().map(|ch| if is_wide(ch) { 2 } else { 1 }).sum()
}

fn is_wide(ch: char) -> bool {
    matches!(ch as u32,
        0x1100..=0x115F
            | 0x2E80..=0x303E
            | 0x3041..=0x33FF
            | 0x3400..=0x4DBF
            | 0x4E00..=0x9FFF
            | 0xA000..=0xA4CF
            | 0xAC00..=0xD7A3
            | 0xF900..=0xFAFF
            | 0xFE30..=0xFE6F
            | 0xFF00..=0xFF60
            | 0xFFE8..=0xFFEF
            | 0x1F300..=0x1FAFF
            | 0x20000..=0x2FFFD
    )
}

fn pad(text: &str, width: usize) -> String {
    let missing = width.saturating_sub(display_width(text)).max(1);
    format!("{text}{}", " ".repeat(missing))
}

fn clip(text: &str, width: usize) -> String {
    if display_width(text) <= width {
        return text.to_string();
    }
    let mut out = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let w = display_width(&ch.to_string());
        if used + w + 1 > width {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::Paths;
    use std::path::PathBuf;

    const USER_FILE: &str = "\
[profile]
active = \"low\"

[zone]
top_n = 5

[guard]
pause_level = 12
";

    fn fixture(user_raw: &str) -> (tempfile::TempDir, Paths, String) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::with_root(PathBuf::from(dir.path()));
        paths.ensure_dirs().unwrap();
        std::fs::write(&paths.config, user_raw).unwrap();
        (dir, paths, user_raw.to_string())
    }

    fn ctx_of(paths: &Paths) -> Ctx {
        Ctx {
            cfg: Config::load(paths).unwrap(),
            paths: paths.clone(),
            force: false,
            elevated: false,
            config_error: None,
        }
    }

    fn source_of(model: &ShowModel, path: &str) -> Option<String> {
        model
            .items
            .iter()
            .find(|item| item.path == path)
            .map(|item| item.source.clone())
    }

    #[test]
    fn apply_writes_only_the_active_key() {
        let (_dir, paths, raw) = fixture(USER_FILE);
        let ctx = ctx_of(&paths);
        handle(&ProfileCmd::Apply { name: "mid".into() }, &ctx, &raw).unwrap();

        let text = std::fs::read_to_string(&paths.config).unwrap();
        assert!(text.contains("active = \"mid\""), "{text}");
        assert!(
            text.contains("pause_level = 12"),
            "user override lost: {text}"
        );
        assert!(text.contains("top_n = 5"), "user section lost: {text}");
        // 预设层不得被摊平写进用户文件。
        assert!(!text.contains("pause_enabled"), "preset leaked: {text}");
        assert!(!text.contains("git_checkpoint"), "preset leaked: {text}");

        let cfg = Config::load(&paths).unwrap();
        assert_eq!(cfg.profile.active, "mid");
        assert_eq!(cfg.guard.pause_level, 12, "user file beats the preset");
        assert_eq!(cfg.guard.warn_level, 20, "untouched key takes the preset");
        assert!(cfg.guard.enable);
        assert!(cfg.guard.pause_enabled);
        assert_eq!(cfg.watchdog.mode, "auto");
        assert!(cfg.plugin.enable);
    }

    #[test]
    fn apply_is_idempotent_and_switching_preset_keeps_overrides() {
        let (_dir, paths, raw) = fixture(USER_FILE);
        let ctx = ctx_of(&paths);
        handle(&ProfileCmd::Apply { name: "mid".into() }, &ctx, &raw).unwrap();
        handle(&ProfileCmd::Apply { name: "mid".into() }, &ctx, &raw).unwrap();
        let first = std::fs::read_to_string(&paths.config).unwrap();
        assert_eq!(first.matches("active = \"mid\"").count(), 1, "{first}");
        assert!(first.contains("pause_level = 12"));

        handle(&ProfileCmd::Apply { name: "low".into() }, &ctx, &raw).unwrap();
        let cfg = Config::load(&paths).unwrap();
        assert_eq!(cfg.profile.active, "low");
        assert!(!cfg.guard.enable, "low preset switches the guardian off");
        assert_eq!(
            cfg.guard.pause_level, 12,
            "user override survives the switch"
        );
    }

    #[test]
    fn apply_rejects_an_unknown_profile_before_writing() {
        let (_dir, paths, raw) = fixture(USER_FILE);
        let ctx = ctx_of(&paths);
        let err = handle(
            &ProfileCmd::Apply {
                name: "ultra".into(),
            },
            &ctx,
            &raw,
        )
        .unwrap_err();
        assert!(err.to_string().contains("未知预设"), "{err}");
        assert!(err.to_string().contains("low | mid | pro"), "{err}");
        let text = std::fs::read_to_string(&paths.config).unwrap();
        assert!(
            text.contains("active = \"low\""),
            "file must be untouched: {text}"
        );
    }

    #[test]
    fn apply_creates_the_profile_section_when_absent() {
        let (_dir, paths, raw) = fixture("[guard]\npause_level = 10\n");
        let ctx = ctx_of(&paths);
        handle(&ProfileCmd::Apply { name: "pro".into() }, &ctx, &raw).unwrap();
        let text = std::fs::read_to_string(&paths.config).unwrap();
        assert!(text.contains("active = \"pro\""), "{text}");
        assert!(text.contains("pause_level = 10"), "{text}");
        let cfg = Config::load(&paths).unwrap();
        assert_eq!(cfg.watchdog.mode, "full");
        assert_eq!(cfg.crunch.pause_days, 21);
    }

    #[test]
    fn show_marks_default_preset_and_user_sources() {
        let (_dir, paths, raw) = fixture(USER_FILE);
        let ctx = ctx_of(&paths);
        handle(&ProfileCmd::Apply { name: "mid".into() }, &ctx, &raw).unwrap();

        let loaded = config::read_user_raw(&paths).unwrap();
        let cfg = Config::load(&paths).unwrap();
        let model = ShowModel::build(&cfg, &loaded).unwrap();

        assert_eq!(model.active, "mid");
        assert!(model.active_pinned);
        assert_eq!(
            source_of(&model, "guard.pause_level").as_deref(),
            Some("user")
        );
        assert_eq!(source_of(&model, "zone.top_n").as_deref(), Some("user"));
        assert_eq!(
            source_of(&model, "guard.pause_enabled").as_deref(),
            Some("preset(mid)")
        );
        assert_eq!(
            source_of(&model, "guard.whitelist.apps.doubao.exe.enabled_levels").as_deref(),
            Some("preset(mid)")
        );
        assert_eq!(
            source_of(&model, "zone.default_alert_free_gb").as_deref(),
            Some("default")
        );
        assert_eq!(
            source_of(&model, "watchdog.poll_sec_lite").as_deref(),
            Some("default")
        );

        let sources: BTreeSet<String> =
            model.items.iter().map(|item| item.source.clone()).collect();
        assert!(sources.contains(SOURCE_DEFAULT), "{sources:?}");
        assert!(sources.contains("preset(mid)"), "{sources:?}");
        assert!(sources.contains(SOURCE_USER), "{sources:?}");

        let counts = model.count_by_source();
        assert!(counts[SOURCE_USER] >= 2, "{counts:?}");
        assert!(counts["preset(mid)"] >= 10, "{counts:?}");
        assert!(counts[SOURCE_DEFAULT] >= 10, "{counts:?}");
        assert!(model.ignored.is_empty(), "{:?}", model.ignored);

        // 表格打印路径不能 panic（输出由 cargo test 捕获）。
        model.print(&paths.config);
    }

    #[test]
    fn show_reports_an_unpinned_active_and_ignored_keys() {
        let (_dir, paths, _raw) =
            fixture("[unknown_section]\nfoo = 1\n[watchdog]\nmode = \"lite\"\n");
        let cfg = Config::load(&paths).unwrap();
        let loaded = config::read_user_raw(&paths).unwrap();
        let model = ShowModel::build(&cfg, &loaded).unwrap();
        assert!(!model.active_pinned, "active came from the builtin default");
        assert_eq!(source_of(&model, "watchdog.mode").as_deref(), Some("user"));
        assert_eq!(model.ignored, vec!["unknown_section.foo".to_string()]);
        assert_eq!(
            source_of(&model, "guard.pause_enabled").as_deref(),
            Some("preset(mid)")
        );
    }

    #[test]
    fn show_json_carries_path_effective_and_source() {
        let (_dir, paths, raw) = fixture(USER_FILE);
        let ctx = ctx_of(&paths);
        handle(&ProfileCmd::Apply { name: "mid".into() }, &ctx, &raw).unwrap();
        let loaded = config::read_user_raw(&paths).unwrap();
        let cfg = Config::load(&paths).unwrap();
        let model = ShowModel::build(&cfg, &loaded).unwrap();
        let value = model.to_json(&paths.config);
        assert_eq!(value["active"], "mid");
        assert!(value["config_path"]
            .as_str()
            .unwrap()
            .ends_with("devzone.toml"));
        let items = value["items"].as_array().unwrap();
        assert!(!items.is_empty());
        let user_item = items
            .iter()
            .find(|item| item["source"] == json!("user") && item["path"] == "guard.pause_level")
            .expect("user override row");
        assert_eq!(user_item["effective"], json!(12));
        // zone.top_n / guard.pause_level / profile.active 都来自用户文件
        assert_eq!(value["counts"]["user"].as_u64().unwrap(), 3);
    }

    #[test]
    fn apply_diff_lists_exactly_what_the_preset_moves() {
        let low =
            Config::assemble("[profile]\nactive = \"low\"\n[guard]\npause_level = 12\n").unwrap();
        let mid =
            Config::assemble("[profile]\nactive = \"mid\"\n[guard]\npause_level = 12\n").unwrap();
        let changes = diff_configs(&low, &mid).unwrap();
        let paths: Vec<&str> = changes.iter().map(|(path, _, _)| path.as_str()).collect();
        assert!(paths.contains(&"guard.enable"), "{paths:?}");
        assert!(paths.contains(&"watchdog.mode"), "{paths:?}");
        assert!(paths.contains(&"plugin.enable"), "{paths:?}");
        assert!(
            !paths.contains(&"guard.pause_level"),
            "user override must not move"
        );
        // 空表 -> 有子项 不算一次值变化（子项已各自成行）
        assert!(!paths.contains(&"guard.whitelist.apps"), "{paths:?}");
        assert!(changes
            .iter()
            .any(|(path, from, to)| path == "guard.enable" && from == "false" && to == "true"));
        assert!(diff_configs(&mid, &mid).unwrap().is_empty());
    }

    #[test]
    fn preset_layer_shape() {
        let low = preset_leaves("low").unwrap();
        assert_eq!(low["guard.enable"], json!(false));
        assert_eq!(low["guard.power_saving.enable"], json!(false));
        assert_eq!(low["watchdog.mode"], json!("lite"));
        let pro = preset_leaves("pro").unwrap();
        assert_eq!(pro["watchdog.mode"], json!("full"));
        assert_eq!(
            pro["guard.whitelist.apps.trae.exe.enabled_levels"],
            json!(["hotkey", "uia", "wm_close", "ocr"])
        );
        assert!(preset_leaves("nope").is_err());
    }

    #[test]
    fn coverage_covers_ancestors_and_leaves() {
        let set: BTreeSet<String> = ["guard.whitelist.apps"]
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        assert!(covered(&set, "guard.whitelist.apps"));
        assert!(covered(
            &set,
            "guard.whitelist.apps.doubao.exe.enabled_levels"
        ));
        assert!(!covered(&set, "guard.pause_level"));
        assert!(!covered(&BTreeSet::new(), "guard.pause_level"));
    }

    #[test]
    fn rendering_helpers() {
        assert_eq!(render_value(&json!("tier1")), "\"tier1\"");
        assert_eq!(render_value(&Value::Null), "-");
        assert_eq!(render_value(&json!(true)), "true");
        assert_eq!(render_value(&json!([1, 2])), "[1,2]");
        assert_eq!(display_width("配置项"), 6);
        assert_eq!(pad("ab", 4), "ab  ");
        assert_eq!(pad("开发", 6), "开发  ");
        assert_eq!(pad("longer-than-column", 4), "longer-than-column ");
        assert_eq!(clip("abcdefghij", 5), "abcd…");
        assert_eq!(clip("中文中文中文", 5), "中文…");
    }
}
