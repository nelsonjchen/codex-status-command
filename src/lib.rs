use regex::Regex;
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CodexStatus {
    pub schema_version: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<Account>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<Model>,
    pub buckets: Vec<QuotaBucket>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Account {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    pub raw: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Model {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<String>,
    pub raw: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct QuotaBucket {
    pub name: String,
    pub limits: Vec<QuotaLimit>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct QuotaLimit {
    pub kind: String,
    pub remaining_percent: u8,
    pub resets: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reset_at_unix: Option<i64>,
}

pub fn parse_status_text(input: &str) -> anyhow::Result<CodexStatus> {
    let text = normalize_terminal_text(input)?;
    let lines: Vec<String> = text
        .lines()
        .map(clean_line)
        .filter(|line| !line.is_empty())
        .collect();

    let account = parse_account(&lines);
    let model = parse_model(&lines);
    let buckets = parse_buckets(&lines)?;

    if buckets.is_empty() {
        anyhow::bail!("could not find quota lines in Codex status output");
    }

    Ok(CodexStatus {
        schema_version: 1,
        account,
        model,
        buckets,
    })
}

pub fn screen_text_from_bytes(bytes: &[u8], rows: u16, cols: u16) -> String {
    let mut parser = vt100::Parser::new(rows, cols, 0);
    parser.process(bytes);
    parser.screen().contents()
}

pub fn plain_text_from_bytes(bytes: &[u8]) -> anyhow::Result<String> {
    let stripped = strip_ansi_escapes::strip(bytes);
    Ok(String::from_utf8_lossy(&stripped).into_owned())
}

fn normalize_terminal_text(input: &str) -> anyhow::Result<String> {
    if input.contains('\u{1b}') {
        plain_text_from_bytes(input.as_bytes())
    } else {
        Ok(input.to_string())
    }
}

fn parse_account(lines: &[String]) -> Option<Account> {
    let re = Regex::new(r"(?i)\bAccount:\s*(.+)$").ok()?;
    let account_re =
        Regex::new(r"^(?P<email>[^()\s]+@[^()\s]+)(?:\s*\((?P<plan>[^)]+)\))?$").ok()?;

    lines.iter().rev().find_map(|line| {
        let raw = re.captures(line)?.get(1)?.as_str().trim().to_string();
        let captures = account_re.captures(&raw);
        Some(Account {
            email: captures
                .as_ref()
                .and_then(|cap| cap.name("email"))
                .map(|m| m.as_str().to_string()),
            plan: captures
                .as_ref()
                .and_then(|cap| cap.name("plan"))
                .map(|m| m.as_str().to_string()),
            raw,
        })
    })
}

fn parse_model(lines: &[String]) -> Option<Model> {
    let re = Regex::new(r"(?i)\bModel:\s*(.+)$").ok()?;
    let model_re = Regex::new(r"^(?P<name>[^\s(]+)(?:\s*\((?P<details>[^)]+)\))?$").ok()?;

    lines.iter().rev().find_map(|line| {
        let raw = re.captures(line)?.get(1)?.as_str().trim().to_string();
        let captures = model_re.captures(&raw);
        Some(Model {
            name: captures
                .as_ref()
                .and_then(|cap| cap.name("name"))
                .map(|m| m.as_str().to_string())
                .unwrap_or_else(|| raw.clone()),
            details: captures
                .as_ref()
                .and_then(|cap| cap.name("details"))
                .map(|m| m.as_str().to_string()),
            raw,
        })
    })
}

fn parse_buckets(lines: &[String]) -> anyhow::Result<Vec<QuotaBucket>> {
    let limit_re = Regex::new(
        r"(?i)^(?P<kind>5h|weekly)\s+limit:\s*(?:\[[^\]]*\]\s*)?(?P<pct>\d{1,3})%\s+left\s*\(resets\s*(?P<reset>[^)]+)\)",
    )?;
    let bucket_re = Regex::new(r"(?i)^(?P<name>.+?)\s+limit:\s*$")?;
    let mut buckets = Vec::<QuotaBucket>::new();
    let mut current = QuotaBucket {
        name: "default".to_string(),
        limits: Vec::new(),
    };

    for line in lines {
        if let Some(captures) = limit_re.captures(line) {
            let percent: u8 = captures["pct"].parse()?;
            current.limits.push(QuotaLimit {
                kind: captures["kind"].to_ascii_lowercase(),
                remaining_percent: percent,
                resets: captures["reset"].trim().to_string(),
                reset_at_unix: None,
            });
            continue;
        }

        if let Some(captures) = bucket_re.captures(line) {
            let name = captures["name"].trim();
            if !name.eq_ignore_ascii_case("5h") && !name.eq_ignore_ascii_case("weekly") {
                if !current.limits.is_empty() {
                    buckets.push(current);
                }
                current = QuotaBucket {
                    name: name.to_string(),
                    limits: Vec::new(),
                };
            }
            continue;
        }
    }

    if !current.limits.is_empty() {
        buckets.push(current);
    }

    Ok(buckets)
}

fn clean_line(line: &str) -> String {
    let without_box = line
        .trim()
        .trim_matches(|ch: char| {
            matches!(
                ch,
                '│' | '┃'
                    | '║'
                    | '┆'
                    | '┊'
                    | '╎'
                    | '╏'
                    | '┌'
                    | '┐'
                    | '└'
                    | '┘'
                    | '─'
                    | '═'
                    | '╭'
                    | '╮'
                    | '╰'
                    | '╯'
            )
        })
        .trim();

    Regex::new(r"\s+")
        .expect("valid whitespace regex")
        .replace_all(without_box, " ")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_status_panel_fixture() {
        let sample = include_str!("../fixtures/status-panel.txt");
        let status = parse_status_text(sample).expect("sample should parse");

        assert_eq!(
            status.account,
            Some(Account {
                email: Some("crazysim@gmail.com".to_string()),
                plan: Some("Pro".to_string()),
                raw: "crazysim@gmail.com (Pro)".to_string()
            })
        );
        assert_eq!(
            status.model,
            Some(Model {
                name: "gpt-5.5".to_string(),
                details: Some("reasoning high, summaries auto".to_string()),
                raw: "gpt-5.5 (reasoning high, summaries auto)".to_string()
            })
        );
        assert_eq!(status.buckets.len(), 2);
        assert_eq!(status.buckets[0].name, "default");
        assert_eq!(status.buckets[0].limits[0].kind, "5h");
        assert_eq!(status.buckets[0].limits[0].remaining_percent, 100);
        assert_eq!(status.buckets[0].limits[0].resets, "14:44");
        assert_eq!(status.buckets[0].limits[1].kind, "weekly");
        assert_eq!(status.buckets[0].limits[1].remaining_percent, 70);
        assert_eq!(status.buckets[0].limits[1].resets, "08:23 on 7 Jun");
        assert_eq!(status.buckets[1].name, "GPT-5.3-Codex-Spark");
        assert_eq!(status.buckets[1].limits[0].remaining_percent, 100);
        assert_eq!(status.buckets[1].limits[1].resets, "09:47 on 8 Jun");
    }
}
