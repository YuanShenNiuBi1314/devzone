//! Notification channels (build guide section 6.5.4).
//!
//! Message template is uniform: `[DevZone] <title>\n<body>\n<ts>`.
//! Failures retry with exponential-ish backoff (5s, 15s) and then degrade to
//! `ConsoleNotify`; a failing channel never aborts event dispatch (section 9).

use std::fmt::Write as _;
use std::time::Duration;

use anyhow::{anyhow, Context as _, Result};

use crate::config::{Config, FeishuCfg, SmtpCfg};
use crate::util::now_iso;

pub trait Notify: Send + Sync {
    fn send(&self, title: &str, body: &str) -> Result<()>;
    fn name(&self) -> &'static str;

    /// `devzone doctor` probe. Must not consume the user's quota or spam
    /// inboxes: SMTP is a NOOP connection test, Feishu posts one test line.
    fn probe(&self) -> Result<String> {
        Ok("可用".to_string())
    }
}

/// `[DevZone] <title>\n<body>\n<ts>`
pub fn render_message(title: &str, body: &str) -> String {
    let mut out = String::new();
    writeln!(out, "[DevZone] {title}").ok();
    out.push_str(body);
    write!(out, "\n{}", now_iso()).ok();
    out
}

/// Retry delays mandated by section 6.5.4 (2 retries, 5s then 15s).
pub const DEFAULT_RETRY_DELAYS: [Duration; 2] = [Duration::from_secs(5), Duration::from_secs(15)];

// ---------------------------------------------------------------------------
// Console
// ---------------------------------------------------------------------------

pub struct ConsoleNotify;

impl Notify for ConsoleNotify {
    fn send(&self, title: &str, body: &str) -> Result<()> {
        eprintln!("{}", render_message(title, body));
        Ok(())
    }

    fn name(&self) -> &'static str {
        "console"
    }

    fn probe(&self) -> Result<String> {
        Ok("stderr 输出可用（不写盘、不外发）".to_string())
    }
}

// ---------------------------------------------------------------------------
// SMTP
// ---------------------------------------------------------------------------

/// Indirection so the retry/backoff behaviour is testable without an SMTP server.
pub trait Mailer: Send + Sync {
    fn deliver(&self, subject: &str, text: &str) -> Result<()>;

    /// Reachability check that does not send a message.
    fn probe(&self) -> Result<String> {
        Ok("已配置（该 mailer 不支持连接测试）".to_string())
    }
}

pub struct LettreMailer {
    cfg: SmtpCfg,
}

impl LettreMailer {
    pub fn new(cfg: &SmtpCfg) -> LettreMailer {
        LettreMailer { cfg: cfg.clone() }
    }

    fn build_transport(&self) -> Result<lettre::transport::smtp::SmtpTransport> {
        use lettre::transport::smtp::authentication::Credentials;
        use lettre::transport::smtp::client::{Tls, TlsParameters};
        use lettre::transport::smtp::{SmtpTransport, SmtpTransportBuilder};

        let builder: SmtpTransportBuilder = if self.cfg.tls {
            let parameters = TlsParameters::new(self.cfg.host.clone())
                .map_err(|err| anyhow!("SMTP TLS 初始化失败：{err}"))?;
            // 465 = implicit TLS (Wrapper); 587 = STARTTLS (Required).
            let mode = if self.cfg.port == 465 {
                Tls::Wrapper(parameters)
            } else {
                Tls::Required(parameters)
            };
            SmtpTransport::builder_dangerous(self.cfg.host.clone()).tls(mode)
        } else {
            SmtpTransport::builder_dangerous(self.cfg.host.clone()).tls(Tls::None)
        };
        let mut builder = builder
            .port(self.cfg.port)
            .timeout(Some(Duration::from_secs(20)));
        if !self.cfg.username.is_empty() {
            builder = builder.credentials(Credentials::new(
                self.cfg.username.clone(),
                self.cfg.password.clone(),
            ));
        }
        Ok(builder.build())
    }
}

impl Mailer for LettreMailer {
    fn deliver(&self, subject: &str, text: &str) -> Result<()> {
        use lettre::message::MultiPart;
        use lettre::transport::Transport;
        use std::str::FromStr;

        let from = lettre::message::Mailbox::from_str(&self.cfg.from)
            .map_err(|err| anyhow!("非法发件地址 `{}`：{err}", self.cfg.from))?;
        let mut builder = lettre::message::Message::builder().from(from);
        if self.cfg.to.is_empty() {
            return Err(anyhow!("SMTP 未配置收件人（watchdog.notify.smtp.to）"));
        }
        for to in &self.cfg.to {
            let mailbox = lettre::message::Mailbox::from_str(to)
                .map_err(|err| anyhow!("非法收件地址 `{to}`：{err}"))?;
            builder = builder.to(mailbox);
        }
        let message = builder
            .subject(subject)
            .multipart(MultiPart::alternative_plain_html(
                text.to_string(),
                format!("<pre>{}</pre>", escape_html(text)),
            ))
            .map_err(|err| anyhow!("构建邮件失败：{err}"))?;
        let transport = self.build_transport()?;
        transport
            .send(&message)
            .map_err(|err| anyhow!("SMTP 发送失败：{err}"))?;
        Ok(())
    }

    fn probe(&self) -> Result<String> {
        let transport = self.build_transport()?;
        match transport.test_connection() {
            Ok(true) => Ok(format!(
                "{}:{} 连接测试通过（NOOP，未发信）",
                self.cfg.host, self.cfg.port
            )),
            Ok(false) => Err(anyhow!(
                "连接测试未通过：{}:{}",
                self.cfg.host,
                self.cfg.port
            )),
            Err(err) => Err(anyhow!("连接测试失败：{err}")),
        }
    }
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

pub struct SmtpNotify {
    mailer: std::sync::Arc<dyn Mailer>,
    retry_delays: Vec<Duration>,
}

impl SmtpNotify {
    pub fn new(cfg: &SmtpCfg) -> SmtpNotify {
        SmtpNotify {
            mailer: std::sync::Arc::new(LettreMailer::new(cfg)),
            retry_delays: DEFAULT_RETRY_DELAYS.to_vec(),
        }
    }

    #[cfg(test)]
    pub fn with_mailer(
        mailer: std::sync::Arc<dyn Mailer>,
        retry_delays: Vec<Duration>,
    ) -> SmtpNotify {
        SmtpNotify {
            mailer,
            retry_delays,
        }
    }
}

impl Notify for SmtpNotify {
    fn send(&self, title: &str, body: &str) -> Result<()> {
        let message = render_message(title, body);
        let subject = format!("[DevZone] {title}");
        let mut last_error = None;
        for (attempt, delay) in self
            .retry_delays
            .iter()
            .chain(std::iter::once(&Duration::ZERO))
            .enumerate()
        {
            match self.mailer.deliver(&subject, &message) {
                Ok(()) => {
                    if attempt > 0 {
                        tracing::info!("smtp delivered on attempt {}", attempt + 1);
                    }
                    return Ok(());
                }
                Err(err) => {
                    tracing::warn!("smtp attempt {} failed: {err}", attempt + 1);
                    last_error = Some(err);
                    if !delay.is_zero() {
                        std::thread::sleep(*delay);
                    }
                }
            }
        }
        Err(last_error.unwrap_or_else(|| anyhow!("SMTP 发送失败")))
    }

    fn name(&self) -> &'static str {
        "smtp"
    }

    fn probe(&self) -> Result<String> {
        self.mailer.probe()
    }
}

// ---------------------------------------------------------------------------
// Feishu webhook
// ---------------------------------------------------------------------------

pub struct FeishuNotify {
    cfg: FeishuCfg,
    client: reqwest::blocking::Client,
    retry_delays: Vec<Duration>,
}

impl FeishuNotify {
    pub fn new(cfg: &FeishuCfg) -> Result<FeishuNotify> {
        let mut builder = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(20))
            // A CLI fires one webhook at a time; never reuse an idle connection.
            .pool_max_idle_per_host(0)
            .use_rustls_tls();
        if points_at_loopback(&cfg.webhook_url) {
            // A local mock (tests) or a LAN-side bridge must never be reached
            // through the user's system proxy.
            builder = builder.no_proxy();
        }
        let client = builder.build().context("构建 HTTP 客户端")?;
        Ok(FeishuNotify {
            cfg: cfg.clone(),
            client,
            retry_delays: DEFAULT_RETRY_DELAYS.to_vec(),
        })
    }

    #[cfg(test)]
    pub fn with_delays(mut self, delays: Vec<Duration>) -> FeishuNotify {
        self.retry_delays = delays;
        self
    }

    /// Body shape from section 6.5.4.
    pub fn payload(title: &str, body: &str) -> serde_json::Value {
        serde_json::json!({
            "msg_type": "text",
            "content": { "text": render_message(title, body) }
        })
    }

    /// One POST. Transport problems are worth retrying; a completed request
    /// whose body carries a non-zero `code` is deterministic (bad signature,
    /// revoked webhook) and is returned immediately as a failure.
    fn send_once(&self, title: &str, body: &str) -> std::result::Result<(), FeishuError> {
        let response = self
            .client
            .post(&self.cfg.webhook_url)
            .json(&FeishuNotify::payload(title, body))
            .send()
            .map_err(|err| {
                FeishuError::Transport(
                    anyhow!(err).context(format!("POST {}", self.cfg.webhook_url)),
                )
            })?;
        let status = response.status();
        let text = response
            .text()
            .map_err(|err| FeishuError::Transport(anyhow!(err).context("读取 Webhook 响应体")))?;
        if !status.is_success() {
            return Err(FeishuError::Transport(anyhow!(
                "飞书 Webhook 返回 {status}: {}",
                text.trim()
            )));
        }
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&text) {
            let code = parsed
                .get("code")
                .and_then(|c| c.as_i64())
                .or_else(|| parsed.get("StatusCode").and_then(|c| c.as_i64()))
                .unwrap_or(0);
            if code != 0 {
                return Err(FeishuError::Business(format!(
                    "飞书 Webhook 业务失败 code={code}: {}",
                    parsed.get("msg").and_then(|m| m.as_str()).unwrap_or("")
                )));
            }
        }
        Ok(())
    }
}

/// Retry policy differs by failure kind (sections 6.5.4 and 9).
#[derive(Debug)]
enum FeishuError {
    Transport(anyhow::Error),
    Business(String),
}

impl FeishuError {
    fn into_anyhow(self) -> anyhow::Error {
        match self {
            FeishuError::Transport(err) => err,
            FeishuError::Business(message) => anyhow::anyhow!(message),
        }
    }
}

impl Notify for FeishuNotify {
    fn send(&self, title: &str, body: &str) -> Result<()> {
        let mut last_error: Option<FeishuError> = None;
        for (attempt, delay) in self
            .retry_delays
            .iter()
            .chain(std::iter::once(&Duration::ZERO))
            .enumerate()
        {
            match self.send_once(title, body) {
                Ok(()) => return Ok(()),
                Err(error @ FeishuError::Business(_)) => return Err(error.into_anyhow()),
                Err(error) => {
                    tracing::warn!("feishu attempt {} failed", attempt + 1);
                    if !delay.is_zero() {
                        std::thread::sleep(*delay);
                    }
                    last_error = Some(error);
                }
            }
        }
        Err(last_error
            .map(FeishuError::into_anyhow)
            .unwrap_or_else(|| anyhow!("飞书通知失败")))
    }

    fn name(&self) -> &'static str {
        "feishu"
    }

    fn probe(&self) -> Result<String> {
        self.send_once("doctor 自检", "这是一条来自 devzone doctor 的测试消息")
            .map_err(FeishuError::into_anyhow)?;
        let host = self.cfg.webhook_url.split('/').nth(2).unwrap_or("?");
        Ok(format!("Webhook {host} POST 测试通过"))
    }
}

// ---------------------------------------------------------------------------
// Windows toast (feature `toast`, M2)
// ---------------------------------------------------------------------------

#[cfg(feature = "toast")]
pub struct ToastNotify;

#[cfg(feature = "toast")]
impl Notify for ToastNotify {
    fn send(&self, title: &str, body: &str) -> Result<()> {
        crate::adapters::toast::show(title, body).context("toast 通知失败")
    }

    fn name(&self) -> &'static str {
        "toast"
    }
}

#[cfg(not(feature = "toast"))]
pub struct ToastNotify;

#[cfg(not(feature = "toast"))]
impl Notify for ToastNotify {
    fn send(&self, _title: &str, _body: &str) -> Result<()> {
        Err(anyhow!(
            "toast 通道需要 `--features toast` 构建（M2 能力），当前二进制未编译入内"
        ))
    }

    fn name(&self) -> &'static str {
        "toast"
    }
}

/// True when a webhook URL targets this machine.
fn points_at_loopback(url: &str) -> bool {
    let without_scheme = url.split_once("://").map(|r| r.1).unwrap_or(url);
    let host = without_scheme.split('/').next().unwrap_or("");
    let host = host.rsplit_once('@').map(|r| r.1).unwrap_or(host);
    let host = host.split(':').next().unwrap_or(host);
    host == "localhost" || host.starts_with("127.")
}

/// Enabled channels for the current configuration.
pub fn build_channels(cfg: &Config) -> Vec<Box<dyn Notify>> {
    let mut channels: Vec<Box<dyn Notify>> = Vec::new();
    let notify = &cfg.watchdog.notify;
    if notify.console {
        channels.push(Box::new(ConsoleNotify));
    }
    if notify.smtp.enable {
        channels.push(Box::new(SmtpNotify::new(&notify.smtp)));
    }
    if notify.feishu.enable {
        match FeishuNotify::new(&notify.feishu) {
            Ok(channel) => channels.push(Box::new(channel)),
            Err(err) => {
                tracing::error!("feishu channel unavailable: {err}");
                if !notify.console {
                    channels.push(Box::new(ConsoleNotify));
                }
            }
        }
    }
    if notify.toast {
        channels.push(Box::new(ToastNotify));
    }
    channels
}

/// Send on every channel; a failing channel degrades to the console (section 9).
/// Returns the human readable failures (empty means everything landed).
pub fn dispatch(channels: &[Box<dyn Notify>], title: &str, body: &str) -> Vec<String> {
    let mut failures = Vec::new();
    let mut console_used = false;
    for channel in channels {
        match channel.send(title, body) {
            Ok(()) => {
                if channel.name() == "console" {
                    console_used = true;
                }
            }
            Err(err) => {
                failures.push(format!("{}: {err}", channel.name()));
                tracing::warn!("notify {} failed: {err}", channel.name());
            }
        }
    }
    if !failures.is_empty() && !console_used {
        ConsoleNotify.send(title, body).ok();
    }
    if failures.is_empty() && channels.is_empty() {
        ConsoleNotify.send(title, body).ok();
    }
    failures
}

/// Short description of each channel state, used by `doctor` and `status`.
pub fn channel_report(cfg: &Config) -> Vec<serde_json::Value> {
    let notify = &cfg.watchdog.notify;
    vec![
        serde_json::json!({ "channel": "console", "enabled": notify.console }),
        serde_json::json!({
            "channel": "smtp",
            "enabled": notify.smtp.enable,
            "host": notify.smtp.host,
            "port": notify.smtp.port,
            "recipients": notify.smtp.to.len(),
        }),
        serde_json::json!({
            "channel": "feishu",
            "enabled": notify.feishu.enable,
            "webhook_set": !notify.feishu.webhook_url.trim().is_empty(),
        }),
        serde_json::json!({
            "channel": "toast",
            "enabled": notify.toast,
            "built": cfg!(feature = "toast"),
        }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    struct FlakyMailer {
        attempts: AtomicUsize,
        failures_before_success: usize,
        last: Mutex<Vec<(String, String)>>,
    }

    impl Mailer for FlakyMailer {
        fn deliver(&self, subject: &str, text: &str) -> Result<()> {
            let count = self.attempts.fetch_add(1, Ordering::SeqCst);
            self.last
                .lock()
                .unwrap()
                .push((subject.to_string(), text.to_string()));
            if count < self.failures_before_success {
                Err(anyhow!("mock smtp failure #{count}"))
            } else {
                Ok(())
            }
        }
    }

    #[derive(Default)]
    struct AlwaysFails(Mutex<usize>);

    impl Mailer for AlwaysFails {
        fn deliver(&self, _subject: &str, _text: &str) -> Result<()> {
            *self.0.lock().unwrap() += 1;
            Err(anyhow!("boom"))
        }
    }

    #[test]
    fn message_template() {
        let text = render_message("低电量", "level=14");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "[DevZone] 低电量");
        assert_eq!(lines[1], "level=14");
        assert!(
            lines[2].contains('+') || lines[2].contains('Z'),
            "{}",
            lines[2]
        );
    }

    #[test]
    fn smtp_retries_then_succeeds() {
        let mailer = std::sync::Arc::new(FlakyMailer {
            attempts: AtomicUsize::new(0),
            failures_before_success: 1,
            last: Mutex::new(Vec::new()),
        });
        let notify = SmtpNotify::with_mailer(
            mailer.clone(),
            vec![Duration::from_millis(1), Duration::from_millis(1)],
        );
        notify.send("t", "b").unwrap();
        assert_eq!(mailer.attempts.load(Ordering::SeqCst), 2);
        assert_eq!(mailer.last.lock().unwrap()[1].0, "[DevZone] t");
        assert!(mailer.last.lock().unwrap()[1]
            .1
            .starts_with("[DevZone] t\nb\n"));
    }

    #[test]
    fn smtp_gives_up_after_two_retries() {
        let mailer = std::sync::Arc::new(AlwaysFails::default());
        let notify = SmtpNotify::with_mailer(
            mailer.clone(),
            vec![Duration::from_millis(1), Duration::from_millis(1)],
        );
        let err = notify.send("t", "b").unwrap_err();
        assert!(err.to_string().contains("boom"), "{err}");
        // 1 initial attempt + 2 retries = 3
        assert_eq!(*mailer.0.lock().unwrap(), 3);
    }

    #[test]
    fn feishu_payload_shape() {
        let payload = FeishuNotify::payload("AI 停住了", "app=trae.exe");
        assert_eq!(payload["msg_type"], "text");
        let text = payload["content"]["text"].as_str().unwrap();
        assert!(
            text.starts_with("[DevZone] AI 停住了\napp=trae.exe\n"),
            "{text}"
        );
    }

    #[test]
    fn feishu_posts_the_documented_payload_to_a_mock_webhook() {
        use tiny_http::{Response, Server};

        let server = Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_ip().unwrap();
        let notify = FeishuNotify::new(&FeishuCfg {
            enable: true,
            webhook_url: format!("http://{addr}/hook"),
        })
        .unwrap()
        .with_delays(vec![Duration::from_millis(1)]);

        let receiver = std::thread::spawn(move || {
            let mut request = server.recv().unwrap();
            let mut body = String::new();
            let length = request.body_length().unwrap_or(0);
            let mut raw = vec![0u8; length];
            request.as_reader().read_exact(&mut raw).unwrap();
            body.push_str(&String::from_utf8_lossy(&raw));
            request
                .respond(Response::from_string(r#"{"code":0,"msg":"success"}"#))
                .unwrap();
            body
        });

        notify.send("hello", "world").unwrap();
        let captured = receiver.join().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&captured).unwrap();
        assert_eq!(parsed["msg_type"], "text");
        let text = parsed["content"]["text"].as_str().unwrap();
        assert!(
            text.starts_with(
                "[DevZone] hello
world
"
            ),
            "{text}"
        );
    }

    #[test]
    fn feishu_business_error_code_is_a_failure() {
        // A webhook that answers HTTP 200 + {"code":19021} must count as a
        // failure: Feishu reports rejections in the body, not the status line.
        // Raw socket instead of tiny_http here so the response is fully
        // deterministic (keep-alive handling differs between the two stacks).
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let mut stream = listener.accept().unwrap().0;
            let mut buffer = [0u8; 4096];
            let _ = std::io::Read::read(&mut stream, &mut buffer);
            let body = r#"{"code":19021,"msg":"sign match fail"}"#;
            let reply = [
                "HTTP/1.1 200 OK",
                "Content-Type: application/json",
                &format!("Content-Length: {}", body.len()),
                "Connection: close",
                "",
                body,
            ]
            .join("\r\n");
            std::io::Write::write_all(&mut stream, reply.as_bytes()).unwrap();
            std::io::Write::flush(&mut stream).unwrap();
        });
        let notify = FeishuNotify::new(&FeishuCfg {
            enable: true,
            webhook_url: format!("http://{addr}/hook"),
        })
        .unwrap()
        .with_delays(vec![Duration::from_millis(1)]);
        let err = format!("{:#}", notify.send("hello", "world").unwrap_err());
        server.join().unwrap();
        assert!(err.contains("19021"), "{err}");
        assert!(err.contains("sign match fail"), "{err}");
    }

    #[test]
    fn dispatch_degrades_to_console_on_failure() {
        let mailer = std::sync::Arc::new(AlwaysFails::default());
        let smtp: Box<dyn Notify> = Box::new(SmtpNotify::with_mailer(
            mailer,
            vec![Duration::from_millis(1), Duration::from_millis(1)],
        ));
        let failures = dispatch(&[smtp], "t", "b");
        assert_eq!(failures.len(), 1);
        assert!(failures[0].starts_with("smtp:"), "{failures:?}");
    }

    #[test]
    fn dispatch_reports_no_failure_when_console_ok() {
        let console: Box<dyn Notify> = Box::new(ConsoleNotify);
        assert!(dispatch(&[console], "t", "b").is_empty());
        assert!(dispatch(&[], "t", "b").is_empty());
    }

    #[test]
    fn channels_follow_config() {
        let dir = tempfile::tempdir().unwrap();
        let paths = crate::util::Paths::with_root(dir.path().to_path_buf());
        std::fs::create_dir_all(&paths.config_dir).unwrap();
        std::fs::write(
            &paths.config,
            "[profile]\nactive = \"low\"\n[watchdog.notify]\nconsole = false\n",
        )
        .unwrap();
        let cfg = Config::load(&paths).unwrap();
        let channels = build_channels(&cfg);
        assert!(channels.is_empty());
        let report = channel_report(&cfg);
        let console = report
            .iter()
            .find(|row| row["channel"] == "console")
            .unwrap();
        assert_eq!(console["enabled"], false);
        let toast = report.iter().find(|row| row["channel"] == "toast").unwrap();
        assert_eq!(toast["enabled"], false);
    }

    #[test]
    fn html_escape_is_applied_to_alternative_part() {
        assert_eq!(escape_html("<a & b>"), "&lt;a &amp; b&gt;");
    }
}
