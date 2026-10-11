//! golden 测试（build guide §6.1.4 / §11）：
//!
//! 用 `assert_cmd` 真跑二进制：`zone add <fixture>` → `zone report <name> --json`，
//! 剥掉 `data.volatile`（各类时间戳 / 实时剩余空间 / mtime）与包壳 `ts` 之后，
//! 把稳定段与 `tests/golden/zone_report.json` 逐字节比对。
//!
//! 为了在 CI 的 windows-latest 上稳定复现：
//! - `DEVZONE_HOME` 指向临时目录，绝不碰真实的 `%LOCALAPPDATA%\devzone`；
//! - `DEVZONE_DRY_RUN=1`，任何外部系统改动都变成日志 + 空操作；
//! - 比对前把 fixture 绝对路径 / home 路径 / 盘符替换成固定占位符；
//! - `--alert-free` 用极大阈值，使预警判定恒为「触发」，不依赖机器剩余空间；
//! - 重新生成 golden：`DEVZONE_WRITE_GOLDEN=1 cargo test --test golden_zone_report`。

use std::path::{Component, Path, PathBuf, Prefix};
use std::process::Output;

use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::{json, Value};

/// 与 `src/mod_zone.rs` 单测同一张 fixture 表：结构固定、字节数已知。
const FIXTURE: &[(&str, usize)] = &[
    ("README.md", 24),
    ("src/main.rs", 120),
    ("src/util.rs", 60),
    ("assets/big.dat", 4096),
    ("assets/mid.dat", 1024),
    ("assets/tiny.dat", 1),
    ("docs/notes.txt", 300),
    ("docs/img/logo.png", 300),
];
/// 命中 `[zone].excludes` 的条目；`node_modules` 里那个 8192B 是全树最大的文件，
/// 若它出现在 Top N 里就说明排除逻辑失效了。
const EXCLUDED: &[(&str, usize)] = &[
    ("node_modules/lib/index.js", 8192),
    ("target/debug/build.o", 2048),
    ("dist/app.js", 512),
    (".git/HEAD", 256),
    (".cache/x", 128),
];

fn golden_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
        .join("zone_report.json")
}

fn write_tree(root: &Path, table: &[(&str, usize)]) {
    for (rel, size) in table {
        let path = root.join(Path::new(rel));
        std::fs::create_dir_all(path.parent().expect("fixture 文件必须有父目录")).unwrap();
        std::fs::write(&path, vec![b'x'; *size]).unwrap();
    }
}

fn devzone(home: &Path) -> Command {
    let mut cmd = Command::cargo_bin("devzone").expect("找不到 devzone 二进制");
    cmd.env("DEVZONE_HOME", home)
        .env("DEVZONE_DRY_RUN", "1")
        // 事件通知走 stderr、日志走文件；这里再压低级别，确保 stdout 只有 JSON
        .env("DEVZONE_LOG", "warn");
    cmd
}

fn drive_letter(path: &Path) -> Option<char> {
    if let Component::Prefix(prefix) = path.components().next()? {
        if let Prefix::Disk(letter) = prefix.kind() {
            return Some((letter as char).to_ascii_uppercase());
        }
    }
    None
}

fn escape_for_json(path: &Path) -> String {
    path.display().to_string().replace('\\', "\\\\")
}

/// 把绝对路径 / 盘符换成占位符：JSON 字符串里的反斜杠是转义过的，先替换转义形式。
fn normalise(text: &str, fixture: &Path, home: &Path) -> String {
    let mut out = text.to_string();
    let replacements: Vec<(String, String)> = vec![
        (escape_for_json(fixture), "<FIXTURE>".to_string()),
        (escape_for_json(home), "<HOME>".to_string()),
        (fixture.display().to_string(), "<FIXTURE>".to_string()),
        (home.display().to_string(), "<HOME>".to_string()),
    ];
    for (from, to) in &replacements {
        if !from.is_empty() && from != to {
            out = out.replace(from.as_str(), to.as_str());
        }
    }
    if let Some(letter) = drive_letter(fixture) {
        out = out.replace(
            &format!("\"drive\": \"{letter}:\""),
            "\"drive\": \"<DRIVE>\"",
        );
    }
    out
}

fn stdout_json(output: &Output) -> Value {
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    serde_json::from_str(&text).unwrap_or_else(|err| {
        panic!("--json 时 stdout 必须只有一个 JSON 对象（§5.2）：{err}\n--- stdout ---\n{text}")
    })
}

/// 剥掉包壳 `ts` 与 `data.volatile`，得到可逐字节比对的稳定段。
fn stable_projection(value: Value) -> Value {
    let mut stable = value;
    stable
        .as_object_mut()
        .expect("包壳是对象")
        .remove("ts")
        .expect("包壳里存在 ts 字段");
    stable["data"]
        .as_object_mut()
        .expect("data 是对象")
        .remove("volatile")
        .expect("data.volatile 必须存在，所有时间戳/实时值都收在这里");
    stable
}

#[test]
fn zone_report_json_matches_golden_file() {
    let home = tempfile::tempdir().expect("home tempdir");
    let fixture = tempfile::tempdir().expect("fixture tempdir");
    write_tree(fixture.path(), FIXTURE);
    write_tree(fixture.path(), EXCLUDED);
    let raw_path = fixture.path().display().to_string();

    // 1. zone add：登记 + 立即扫描一次（§6.1.1），阈值极大 => zone_alert 必然触发
    let add_args: Vec<&str> = vec![
        "zone",
        "add",
        raw_path.as_str(),
        "--name",
        "golden",
        "--alert-free",
        "4294967296",
    ];
    devzone(home.path())
        .args(&add_args)
        .assert()
        .success()
        .stdout(predicate::str::contains("已登记开发区 golden"))
        // 预警链路：事件总线 -> ConsoleNotify（stderr），stdout 保持干净
        .stderr(predicate::str::contains("开发区空间预警"));

    // 2. zone report --json --top 4
    let report_args: Vec<&str> = vec!["zone", "report", "golden", "--json", "--top", "4"];
    let output = devzone(home.path())
        .args(&report_args)
        .output()
        .expect("运行 devzone zone report");
    assert!(
        output.status.success(),
        "退出码异常：{:?}\nstderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let value = stdout_json(&output);
    assert_eq!(value["ok"], json!(true));
    assert_eq!(value["command"], json!("zone report"));
    assert!(value["ts"].is_string(), "包壳里应有 ts");
    assert_eq!(value["data"]["schema"], json!("zone-report/1"));

    // 3. 归一化路径后逐字节比对 golden
    let pretty = serde_json::to_string_pretty(&stable_projection(value)).expect("序列化稳定段");
    let normalized = normalise(&pretty, fixture.path(), home.path());

    // 可读断言（golden 不匹配时帮助定位）
    assert!(normalized.contains("\"total_bytes\": 5925"), "{normalized}");
    assert!(normalized.contains("\"file_count\": 8"), "{normalized}");
    assert!(normalized.contains("\"dir_count\": 4"), "{normalized}");
    assert!(
        !normalized.contains("node_modules") && !normalized.contains("target"),
        "excludes 命中的条目泄漏进报告：{normalized}"
    );
    assert!(normalized.contains("\"triggered\": true"), "{normalized}");
    assert!(normalized.contains("\"previous\": null"), "{normalized}");

    if std::env::var("DEVZONE_WRITE_GOLDEN").is_ok() {
        std::fs::create_dir_all(golden_path().parent().expect("golden 目录"))
            .expect("创建 tests/golden 失败");
        std::fs::write(golden_path(), normalized.as_bytes()).expect("写入 golden 失败");
        eprintln!("已写入 {}", golden_path().display());
        return;
    }

    let golden_raw = std::fs::read_to_string(golden_path()).unwrap_or_else(|err| {
        panic!(
            "读取 golden 失败 {}：{err}（可用 DEVZONE_WRITE_GOLDEN=1 重新生成）",
            golden_path().display()
        )
    });
    // 检出时 core.autocrlf 可能把 golden 变成 CRLF；统一按 LF 比对
    let golden = golden_raw
        .replace("\r\n", "\n")
        .trim_end_matches('\n')
        .to_string();
    assert_eq!(
        normalized,
        golden,
        "golden 比对失败：{}",
        golden_path().display()
    );
}

#[test]
fn add_scan_list_remove_roundtrip_via_cli() {
    let home = tempfile::tempdir().expect("home tempdir");
    let fixture = tempfile::tempdir().expect("fixture tempdir");
    write_tree(fixture.path(), FIXTURE);
    write_tree(fixture.path(), EXCLUDED);
    let raw_path = fixture.path().display().to_string();

    // 未登记时 list 给出提示，report 报错退出码 1（§5.2）
    devzone(home.path())
        .args(["zone", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("尚未登记开发区"));
    devzone(home.path())
        .args(["zone", "report", "ghost"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("不存在"));

    let add_args: Vec<&str> = vec!["zone", "add", raw_path.as_str(), "--name", "round"];
    devzone(home.path())
        .args(&add_args)
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "下一步：devzone zone report round",
        ));

    // 重复 add 未 --force => 退出码 1 并提示 --force（§5.2）
    devzone(home.path())
        .args(&add_args)
        .assert()
        .failure()
        .stderr(predicate::str::contains("--force"));
    let forced: Vec<&str> = vec![
        "--force",
        "zone",
        "add",
        raw_path.as_str(),
        "--name",
        "round",
    ];
    devzone(home.path()).args(&forced).assert().success();

    devzone(home.path())
        .args(["zone", "scan", "round"])
        .assert()
        .success()
        .stdout(predicate::str::contains("总占用"));
    devzone(home.path())
        .args(["zone", "scan", "all"])
        .assert()
        .success();
    devzone(home.path())
        .args(["zone", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("round"));
    // 报告里能看到 Δ（add + 两次 scan = 三条快照）
    devzone(home.path())
        .args(["zone", "report", "round"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Δ 相比上次").and(predicate::str::contains("大文件 Top")));

    devzone(home.path())
        .args(["zone", "remove", "round"])
        .assert()
        .success();
    devzone(home.path())
        .args(["zone", "remove", "round"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("不存在"));
}

#[test]
fn json_envelope_is_the_only_thing_on_stdout() {
    let home = tempfile::tempdir().expect("home tempdir");
    let fixture = tempfile::tempdir().expect("fixture tempdir");
    write_tree(fixture.path(), FIXTURE);
    write_tree(fixture.path(), EXCLUDED);
    let raw_path = fixture.path().display().to_string();
    let add_args: Vec<&str> = vec![
        "zone",
        "add",
        raw_path.as_str(),
        "--name",
        "solo",
        "--alert-free",
        "0",
    ];
    devzone(home.path()).args(&add_args).assert().success();
    let output = devzone(home.path())
        .args(["zone", "report", "solo", "--json"])
        .output()
        .expect("运行 devzone");
    assert!(output.status.success());
    let value = stdout_json(&output);
    assert_eq!(
        value["data"]["alert"]["triggered"],
        json!(false),
        "阈值 0 应关闭预警"
    );
    let big = value["data"]["big_files"]
        .as_array()
        .expect("big_files 是数组");
    assert!(!big.is_empty());
    assert!(big[0]["path"]
        .as_str()
        .expect("path")
        .starts_with(raw_path.as_str()));
    assert!(
        big.iter()
            .all(|file| !file["path"].as_str().unwrap_or("").contains("node_modules")),
        "excludes 泄漏：{big:?}"
    );
    assert_eq!(value["data"]["delta"]["bytes"], Value::Null);
    assert!(value["data"]["volatile"]["free_gb"].is_number());
    assert!(value["data"]["volatile"]["big_file_mtimes"]
        .as_array()
        .expect("mtime 列表")
        .iter()
        .all(|entry| entry["mtime"].is_string()));
}
