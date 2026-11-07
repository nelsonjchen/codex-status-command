use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::{Local, TimeZone};
use codex_status_json::{
    Account, CodexStatus, Model, QuotaBucket, QuotaLimit, parse_status_text, plain_text_from_bytes,
    screen_text_from_bytes,
};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use serde_json::{Value, json};

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal usable quota response with optional reset metadata.
    fn status_with_resets(summary: Option<Value>) -> Result<Value> {
        let mut limits = json!({
            "rateLimits": {
                "primary": {"usedPercent": 20, "windowDurationMins": 300, "resetsAt": 1780350290}
            }
        });
        if let Some(summary) = summary {
            limits["rateLimitResetCredits"] = summary;
        }
        Ok(serde_json::to_value(status_from_app_server_values(
            &json!({}),
            &limits,
            None,
        )?)?)
    }

    /// Preserve credit metadata, unknown future strings, and nullable fields.
    #[test]
    fn includes_banked_reset_details() {
        let status = status_with_resets(Some(json!({
            "availableCount": 2,
            "credits": [
                {"id": "a", "resetType": "codexRateLimits", "status": "available",
                 "grantedAt": 1780350290, "expiresAt": 1782942290,
                 "title": "Full reset", "description": "Promotional reset"},
                {"id": "b", "resetType": "futureType", "status": "futureStatus",
                 "grantedAt": 1780350291, "expiresAt": null,
                 "title": null, "description": null}
            ]
        })))
        .unwrap();
        assert_eq!(
            status["banked_resets"],
            json!({
                "available_count": 2,
                "credits": [
                    {"id": "a", "reset_type": "codexRateLimits", "status": "available",
                     "granted_at_unix": 1780350290, "expires_at_unix": 1782942290,
                     "title": "Full reset", "description": "Promotional reset"},
                    {"id": "b", "reset_type": "futureType", "status": "futureStatus",
                     "granted_at_unix": 1780350291, "expires_at_unix": null,
                     "title": null, "description": null}
                ]
            })
        );
        assert_eq!(status["schema_version"], 1);
    }

    /// Unknown summaries must not be mistaken for a known zero balance.
    #[test]
    fn omits_unavailable_banked_resets() {
        for summary in [None, Some(Value::Null)] {
            assert!(
                status_with_resets(summary)
                    .unwrap()
                    .get("banked_resets")
                    .is_none()
            );
        }
    }

    /// Preserve count-only, empty, and capped-detail responses distinctly.
    #[test]
    fn preserves_reset_detail_availability() {
        for (count, credits) in [(3, Value::Null), (0, json!([])), (3, json!([]))] {
            let status = status_with_resets(Some(json!({
                "availableCount": count, "credits": credits
            })))
            .unwrap();
            assert_eq!(status["banked_resets"]["available_count"], count);
            assert_eq!(status["banked_resets"]["credits"], credits);
        }
    }

    /// Reject malformed balances instead of silently reporting misleading data.
    #[test]
    fn rejects_invalid_reset_summary() {
        let err =
            status_with_resets(Some(json!({"availableCount": -1, "credits": []}))).unwrap_err();
        assert!(
            err.to_string()
                .contains("invalid app-server banked reset data")
        );
    }

    /// PTY parsing cannot infer the banked-reset balance from quota windows.
    #[test]
    fn pty_omits_banked_resets() {
        let status = parse_status_text(include_str!("../fixtures/status-panel.txt")).unwrap();
        assert!(
            serde_json::to_value(status)
                .unwrap()
                .get("banked_resets")
                .is_none()
        );
    }
}

const ROWS: u16 = 50;
const COLS: u16 = 160;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(20);

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("codex-status-json: {err:#}");
            ExitCode::from(1)
        }
    }
}

fn run() -> Result<()> {
    let args = Args::parse()?;
    let status = if let Some(path) = args.parse_fixture {
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read fixture {}", path.display()))?;
        parse_status_text(&text)?
    } else {
        capture_status(args.backend, args.timeout, args.debug_dump.as_deref())?
    };

    serde_json::to_writer_pretty(std::io::stdout(), &status)?;
    println!();
    Ok(())
}

#[derive(Debug)]
struct Args {
    timeout: Duration,
    backend: Backend,
    parse_fixture: Option<PathBuf>,
    debug_dump: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Backend {
    Auto,
    AppServer,
    Pty,
}

impl Args {
    fn parse() -> Result<Self> {
        let mut timeout = DEFAULT_TIMEOUT;
        let mut backend = Backend::AppServer;
        let mut parse_fixture = None;
        let mut debug_dump = None;
        let mut args = std::env::args().skip(1);

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "-h" | "--help" => {
                    print_help();
                    std::process::exit(0);
                }
                "-V" | "--version" => {
                    println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
                    std::process::exit(0);
                }
                "--timeout-ms" => {
                    let value = args.next().context("--timeout-ms requires a value")?;
                    let millis: u64 = value
                        .parse()
                        .with_context(|| format!("invalid timeout value {value:?}"))?;
                    timeout = Duration::from_millis(millis);
                }
                "--backend" => {
                    let value = args.next().context("--backend requires a value")?;
                    backend = match value.as_str() {
                        "auto" => Backend::Auto,
                        "app-server" => Backend::AppServer,
                        "pty" => Backend::Pty,
                        _ => {
                            anyhow::bail!("invalid backend {value:?}; use auto, app-server, or pty")
                        }
                    };
                }
                "--parse-fixture" => {
                    parse_fixture = Some(PathBuf::from(
                        args.next().context("--parse-fixture requires a path")?,
                    ));
                }
                "--debug-dump" => {
                    debug_dump = Some(PathBuf::from(
                        args.next().context("--debug-dump requires a path")?,
                    ));
                }
                _ => anyhow::bail!("unknown argument {arg:?}; try --help"),
            }
        }

        Ok(Self {
            timeout,
            backend,
            parse_fixture,
            debug_dump,
        })
    }
}

fn print_help() {
    println!(
        "\
codex-status-json

Usage:
  codex-status-json [--backend app-server|auto|pty] [--timeout-ms 20000]
  codex-status-json --parse-fixture path/to/status.txt

Prints Codex account/quota JSON. Defaults to app-server JSON-RPC.
"
    );
}

fn capture_status(
    backend: Backend,
    timeout: Duration,
    debug_dump: Option<&std::path::Path>,
) -> Result<CodexStatus> {
    match backend {
        Backend::AppServer => capture_status_app_server(timeout),
        Backend::Pty => {
            let text = capture_status_panel(timeout, debug_dump)?;
            parse_status_text(&text)
        }
        Backend::Auto => capture_status_app_server(timeout).or_else(|app_server_err| {
            let text = capture_status_panel(timeout, debug_dump)
                .with_context(|| format!("app-server failed first: {app_server_err:#}"))?;
            parse_status_text(&text)
        }),
    }
}

fn capture_status_app_server(timeout: Duration) -> Result<CodexStatus> {
    let mut rpc = AppServerRpc::start()?;
    let init = rpc.request(
        "initialize",
        json!({
            "clientInfo": {
                "name": "codex_status_json",
                "title": "Codex Status JSON",
                "version": env!("CARGO_PKG_VERSION")
            }
        }),
        timeout,
    )?;
    let _ = init;
    rpc.notify("initialized", None)?;

    let account = rpc.request("account/read", json!({ "refreshToken": false }), timeout)?;
    let rate_limits = rpc.request("account/rateLimits/read", json!({}), timeout)?;
    let config = rpc.request("config/read", json!({}), timeout).ok();

    let status = status_from_app_server_values(&account, &rate_limits, config.as_ref())?;
    rpc.shutdown();
    Ok(status)
}

/// Convert read-only account responses, preserving optional banked-reset metadata.
fn status_from_app_server_values(
    account_value: &Value,
    rate_limits_value: &Value,
    config_value: Option<&Value>,
) -> Result<CodexStatus> {
    let account = account_value.get("account").and_then(|account| {
        let email = account
            .get("email")
            .and_then(Value::as_str)
            .map(str::to_string);
        let plan = account
            .get("planType")
            .and_then(Value::as_str)
            .map(title_case_plan);
        if email.is_none() && plan.is_none() {
            return None;
        }
        let raw = match (&email, &plan) {
            (Some(email), Some(plan)) => format!("{email} ({plan})"),
            (Some(email), None) => email.clone(),
            (None, Some(plan)) => plan.clone(),
            (None, None) => unreachable!(),
        };
        Some(Account { email, plan, raw })
    });

    let model = config_value
        .and_then(|value| value.get("config"))
        .and_then(|config| {
            let name = config.get("model")?.as_str()?.to_string();
            let mut details = Vec::new();
            if let Some(reasoning) = config.get("model_reasoning_effort").and_then(Value::as_str) {
                details.push(format!("reasoning {reasoning}"));
            }
            if let Some(summary) = config
                .get("model_reasoning_summary")
                .and_then(Value::as_str)
            {
                details.push(format!("summaries {summary}"));
            }
            let details = if details.is_empty() {
                None
            } else {
                Some(details.join(", "))
            };
            let raw = details
                .as_ref()
                .map(|details| format!("{name} ({details})"))
                .unwrap_or_else(|| name.clone());
            Some(Model { name, details, raw })
        });

    let mut limits = Vec::<(&str, &Value)>::new();
    if let Some(by_id) = rate_limits_value
        .get("rateLimitsByLimitId")
        .and_then(Value::as_object)
    {
        if let Some(default) = by_id.get("codex") {
            limits.push(("codex", default));
        }
        let mut secondary: Vec<(&str, &Value)> = by_id
            .iter()
            .filter(|(id, _)| id.as_str() != "codex")
            .map(|(id, value)| (id.as_str(), value))
            .collect();
        secondary.sort_by_key(|(_, value)| {
            value
                .get("limitName")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        });
        limits.extend(secondary);
    } else if let Some(default) = rate_limits_value.get("rateLimits") {
        limits.push(("codex", default));
    }

    let buckets: Vec<QuotaBucket> = limits
        .into_iter()
        .filter_map(|(id, value)| bucket_from_rate_limit(id, value))
        .collect();

    if buckets.is_empty() {
        anyhow::bail!("app-server did not return usable quota data");
    }

    Ok(CodexStatus {
        schema_version: 1,
        account,
        model,
        buckets,
        banked_resets: rate_limits_value
            .get("rateLimitResetCredits")
            .filter(|value| !value.is_null())
            .map(|value| serde_json::from_value(value.clone()))
            .transpose()
            .context("invalid app-server banked reset data")?,
    })
}

fn bucket_from_rate_limit(id: &str, value: &Value) -> Option<QuotaBucket> {
    let name = value
        .get("limitName")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| {
            if id == "codex" {
                "default".to_string()
            } else {
                id.to_string()
            }
        });

    let mut limits = Vec::new();
    if let Some(primary) = value.get("primary").and_then(limit_from_window) {
        limits.push(primary);
    }
    if let Some(secondary) = value.get("secondary").and_then(limit_from_window) {
        limits.push(secondary);
    }

    if limits.is_empty() {
        None
    } else {
        Some(QuotaBucket { name, limits })
    }
}

fn limit_from_window(value: &Value) -> Option<QuotaLimit> {
    let used_percent = value.get("usedPercent")?.as_u64()?;
    let window_mins = value.get("windowDurationMins")?.as_u64()?;
    let reset_at_unix = value.get("resetsAt")?.as_i64()?;
    let remaining_percent = 100_u8.saturating_sub(used_percent.min(100) as u8);
    let kind = match window_mins {
        300 => "5h".to_string(),
        10080 => "weekly".to_string(),
        mins if mins % 60 == 0 => format!("{}h", mins / 60),
        mins => format!("{mins}m"),
    };
    let resets = format_reset(reset_at_unix, &kind);

    Some(QuotaLimit {
        kind,
        remaining_percent,
        resets,
        reset_at_unix: Some(reset_at_unix),
    })
}

fn format_reset(unix: i64, kind: &str) -> String {
    let Some(dt) = Local.timestamp_opt(unix, 0).single() else {
        return unix.to_string();
    };
    if kind == "weekly" {
        dt.format("%H:%M on %-d %b").to_string()
    } else {
        dt.format("%H:%M").to_string()
    }
}

fn title_case_plan(plan: &str) -> String {
    let mut chars = plan.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

struct AppServerRpc {
    child: Child,
    stdin: ChildStdin,
    rx: mpsc::Receiver<Result<Value, String>>,
    stderr: Arc<Mutex<String>>,
    next_id: u64,
}

impl AppServerRpc {
    fn start() -> Result<Self> {
        let mut child = Command::new("codex")
            .args(["app-server", "--listen", "stdio://", "-c", "mcp_servers={}"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("failed to start `codex app-server --listen stdio://`")?;

        let stdin = child
            .stdin
            .take()
            .context("failed to open app-server stdin")?;
        let stdout = child
            .stdout
            .take()
            .context("failed to open app-server stdout")?;
        let mut stderr_reader = child
            .stderr
            .take()
            .context("failed to open app-server stderr")?;
        let (tx, rx) = mpsc::channel();
        let stderr = Arc::new(Mutex::new(String::new()));
        let stderr_for_thread = Arc::clone(&stderr);

        thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                let line = match line {
                    Ok(line) => line,
                    Err(err) => {
                        let _ = tx.send(Err(err.to_string()));
                        break;
                    }
                };
                if line.trim().is_empty() {
                    continue;
                }
                let parsed = serde_json::from_str::<Value>(&line)
                    .map_err(|err| format!("invalid app-server JSON line: {line:?}: {err}"));
                if tx.send(parsed).is_err() {
                    break;
                }
            }
        });

        thread::spawn(move || {
            let mut text = String::new();
            let _ = stderr_reader.read_to_string(&mut text);
            if !text.trim().is_empty() {
                if let Ok(mut stderr) = stderr_for_thread.lock() {
                    *stderr = text;
                }
            }
        });

        Ok(Self {
            child,
            stdin,
            rx,
            stderr,
            next_id: 1,
        })
    }

    fn request(&mut self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let message = json!({
            "id": id,
            "method": method,
            "params": params,
        });
        writeln!(self.stdin, "{message}")?;
        self.stdin.flush()?;

        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .context("timed out waiting for app-server response")?;
            let value = match self.rx.recv_timeout(remaining) {
                Ok(value) => value.map_err(anyhow::Error::msg)?,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    anyhow::bail!(
                        "timed out waiting for app-server response to {method}{}",
                        self.stderr_suffix()
                    );
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    anyhow::bail!(
                        "app-server exited before responding to {method}{}",
                        self.stderr_suffix()
                    );
                }
            };
            if value.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = value.get("error") {
                anyhow::bail!("app-server {method} failed: {error}");
            }
            return Ok(value.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    fn notify(&mut self, method: &str, params: Option<Value>) -> Result<()> {
        let message = match params {
            Some(params) => json!({ "method": method, "params": params }),
            None => json!({ "method": method }),
        };
        writeln!(self.stdin, "{message}")?;
        self.stdin.flush()?;
        Ok(())
    }

    fn shutdown(&mut self) {
        self.child.kill().ok();
        self.child.wait().ok();
    }

    fn stderr_suffix(&self) -> String {
        let stderr = self
            .stderr
            .lock()
            .ok()
            .map(|stderr| stderr.trim().to_string())
            .unwrap_or_default();
        if stderr.is_empty() {
            String::new()
        } else {
            format!("; app-server stderr: {stderr}")
        }
    }
}

impl Drop for AppServerRpc {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn capture_status_panel(timeout: Duration, debug_dump: Option<&std::path::Path>) -> Result<String> {
    let pty_system = native_pty_system();
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let pair = pty_system
        .openpty(PtySize {
            rows: ROWS,
            cols: COLS,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("failed to open PTY")?;

    let mut command = CommandBuilder::new("codex");
    command.arg("--no-alt-screen");
    command.arg("--cd");
    command.arg(&cwd);
    command.arg("-c");
    command.arg("mcp_servers={}");
    command.arg("-c");
    command.arg(format!(
        "projects.{:?}.trust_level=\"trusted\"",
        cwd.display().to_string()
    ));
    command.env(
        "TERM",
        std::env::var("CODEX_STATUS_TERM").unwrap_or_else(|_| "xterm-256color".to_string()),
    );

    let mut child = pair
        .slave
        .spawn_command(command)
        .context("failed to start `codex --no-alt-screen`")?;
    drop(pair.slave);

    let mut reader = pair
        .master
        .try_clone_reader()
        .context("failed to clone PTY reader")?;
    let mut writer = pair
        .master
        .take_writer()
        .context("failed to take PTY writer")?;

    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let reader_thread = thread::spawn(move || {
        let mut buf = [0_u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let start = Instant::now();
    let mut raw = Vec::<u8>::new();
    let mut sent_continue = false;
    let mut sent_status = false;
    let mut sent_update_dismiss = false;
    let mut status_capture = None::<String>;
    let mut status_capture_updated_at = None::<Instant>;
    let mut trust_prompt_seen = false;

    while start.elapsed() < timeout {
        if let Ok(chunk) = rx.recv_timeout(Duration::from_millis(100)) {
            raw.extend_from_slice(&chunk);
        }

        let screen = screen_text_from_bytes(&raw, ROWS, COLS);
        let plain = plain_text_from_bytes(&raw).unwrap_or_default();
        let combined = format!("{plain}\n{screen}");

        if !sent_continue && combined.contains("Continue anyway? [y/N]") {
            writer.write_all(b"y\r")?;
            writer.flush()?;
            sent_continue = true;
            continue;
        }

        if looks_like_trust_prompt(&combined) {
            writer.write_all(b"2\r")?;
            writer.flush()?;
            trust_prompt_seen = true;
            break;
        }

        if !sent_update_dismiss && looks_like_update_notice(&combined) {
            writer.write_all(b"\x1b\r")?;
            writer.flush()?;
            sent_update_dismiss = true;
            continue;
        }

        if !sent_status && looks_ready_for_slash_command(&combined) {
            writer.write_all(b"/status\r")?;
            writer.flush()?;
            sent_status = true;
            continue;
        }

        if sent_status && has_quota_data(&combined) {
            let changed = status_capture.as_ref() != Some(&screen);
            if changed {
                status_capture = Some(screen);
                status_capture_updated_at = Some(Instant::now());
            } else if status_capture_updated_at
                .is_some_and(|updated_at| updated_at.elapsed() >= Duration::from_millis(900))
            {
                break;
            }
        }
    }

    writer.write_all(b"\x1b/quit\r").ok();
    writer.flush().ok();

    let wait_deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < wait_deadline {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => thread::sleep(Duration::from_millis(100)),
            Err(_) => break,
        }
    }
    if matches!(child.try_wait(), Ok(None)) {
        child.kill().ok();
    }

    drop(writer);
    reader_thread.join().ok();

    if trust_prompt_seen {
        anyhow::bail!(
            "Codex asked to trust the working directory; refusing because that would modify Codex config"
        );
    } else if let Some(text) = status_capture {
        if let Some(path) = debug_dump {
            std::fs::write(path, &text)
                .with_context(|| format!("failed to write debug dump {}", path.display()))?;
        }
        Ok(text)
    } else {
        if let Some(path) = debug_dump {
            let screen = screen_text_from_bytes(&raw, ROWS, COLS);
            let plain = plain_text_from_bytes(&raw).unwrap_or_default();
            std::fs::write(
                path,
                format!("--- plain ---\n{plain}\n--- screen ---\n{screen}"),
            )
            .with_context(|| format!("failed to write debug dump {}", path.display()))?;
        }
        anyhow::bail!("timed out waiting for Codex /status quota data")
    }
}

fn looks_like_update_notice(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("update available")
        || lower.contains("new version")
        || lower.contains("codex update")
}

fn looks_like_trust_prompt(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("do you trust the contents of this directory")
        || lower.contains("trusting the directory allows")
        || lower.contains("yes, continue")
}

fn looks_ready_for_slash_command(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    if lower.contains("starting mcp servers")
        || lower.contains("do you trust the contents of this directory")
        || lower.contains("continue anyway? [y/n]")
    {
        return false;
    }

    lower.contains("type a message")
        || lower.contains("ask codex")
        || lower.contains("press /")
        || text.contains("\n› ")
}

fn has_quota_data(text: &str) -> bool {
    text.contains("5h limit:") && text.contains("Weekly limit:")
}
