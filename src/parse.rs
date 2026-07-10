//! Discovery and defensive parsing of Claude Code and Codex transcripts.
//!
//! Every file is opened READ-ONLY. Schemas are internal and drift between
//! versions, so we match on `type`/`role`, pull fields when present, and skip
//! anything we can't make sense of rather than failing the whole file.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;

use serde_json::Value;
use walkdir::WalkDir;

use crate::data::{Backend, Message, Session, parse_iso_epoch, space_of};

const TITLE_MAX: usize = 80;

pub enum Load {
    Session(Box<Session>),
    Done,
}

pub fn claude_root() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude").join("projects"))
}

pub fn codex_root() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".codex").join("sessions"))
}

/// Walk both roots on a background thread, parse each transcript, and stream
/// finished sessions back so the UI never blocks on I/O.
pub fn spawn_loader(tx: Sender<Load>) {
    std::thread::spawn(move || {
        if let Some(root) = claude_root() {
            for path in jsonl_files(&root) {
                if let Some(s) = parse_claude_file(&path)
                    && tx.send(Load::Session(Box::new(s))).is_err()
                {
                    return;
                }
            }
        }
        if let Some(root) = codex_root() {
            for path in jsonl_files(&root) {
                if let Some(s) = parse_codex_file(&path)
                    && tx.send(Load::Session(Box::new(s))).is_err()
                {
                    return;
                }
            }
        }
        let _ = tx.send(Load::Done);
    });
}

fn jsonl_files(root: &Path) -> Vec<PathBuf> {
    WalkDir::new(root)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| e.into_path())
        .filter(|p| p.extension().map(|x| x == "jsonl").unwrap_or(false))
        .collect()
}

fn file_mtime_epoch(path: &Path) -> i64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn read_lines(path: &Path) -> Option<Vec<Value>> {
    let file = File::open(path).ok()?;
    let reader = BufReader::new(file);
    let mut out = Vec::new();
    for line in reader.lines() {
        let Ok(line) = line else { continue };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(line) {
            out.push(v);
        }
    }
    Some(out)
}

fn truncate_title(s: &str) -> String {
    let one_line = s
        .split('\n')
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    if one_line.chars().count() > TITLE_MAX {
        let clipped: String = one_line.chars().take(TITLE_MAX).collect();
        format!("{clipped}…")
    } else {
        one_line.to_string()
    }
}

/// Genuine prose the user typed — as opposed to harness wrappers
/// (`<local-command-caveat>`, `<command-name>`, …), tool results, or pasted
/// attachments. Non-prose user turns are dropped entirely: they'd pollute the
/// preview, the search haystack, and the message count alike.
fn is_prose(text: &str) -> bool {
    let t = text.trim();
    !t.is_empty()
        && !t.starts_with("Caveat:")
        && !t.starts_with('<')
        && !t.contains("<command-name>")
        && !t.contains("[Request interrupted")
}

fn finalize(
    backend: Backend,
    id: String,
    title: String,
    cwd: String,
    last_activity: i64,
    messages: Vec<Message>,
) -> Session {
    let mut blob = title.to_lowercase();
    for m in &messages {
        blob.push('\n');
        blob.push_str(&m.text.to_lowercase());
    }
    blob.push('\n');
    blob.push_str(&cwd.to_lowercase());
    let space = space_of(&cwd);
    blob.push('\n');
    blob.push_str(&space.to_lowercase());
    Session {
        backend,
        id,
        title,
        cwd,
        space,
        last_activity,
        msg_count: messages.len(),
        messages,
        search_blob: blob,
    }
}

/// Pull plain prose out of a message `content` that may be a bare string or an
/// array of typed parts. Only `text`/`input_text`/`output_text` parts (all of
/// which carry a `text` field) are kept; tool calls, images, and model
/// "thinking" are dropped so the preview and search stay clean.
fn extract_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => {
            let mut buf = String::new();
            for p in parts {
                if let Some(t) = p.get("text").and_then(Value::as_str) {
                    if !buf.is_empty() {
                        buf.push('\n');
                    }
                    buf.push_str(t);
                }
            }
            buf
        }
        _ => String::new(),
    }
}

fn parse_claude_file(path: &Path) -> Option<Session> {
    let lines = read_lines(path)?;
    let id = path.file_stem()?.to_string_lossy().to_string();

    let mut cwd = String::new();
    let mut ai_title = String::new();
    let mut messages: Vec<Message> = Vec::new();
    let mut first_prose: Option<String> = None;
    let mut last_ts: i64 = 0;

    for v in &lines {
        let ty = v.get("type").and_then(Value::as_str).unwrap_or("");

        if cwd.is_empty()
            && let Some(c) = v.get("cwd").and_then(Value::as_str)
        {
            cwd = c.to_string();
        }
        if ty == "ai-title"
            && let Some(t) = v.get("aiTitle").and_then(Value::as_str)
        {
            ai_title = t.to_string();
        }
        if let Some(ts) = v.get("timestamp").and_then(Value::as_str)
            && let Some(e) = parse_iso_epoch(ts)
        {
            last_ts = last_ts.max(e);
        }

        if (ty == "user" || ty == "assistant")
            && let Some(msg) = v.get("message")
        {
            let role = msg
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or(ty)
                .to_string();
            let text = msg.get("content").map(extract_text).unwrap_or_default();
            if text.trim().is_empty() || (role == "user" && !is_prose(&text)) {
                continue;
            }
            if role == "user" && first_prose.is_none() {
                first_prose = Some(text.clone());
            }
            messages.push(Message { role, text });
        }
    }

    // Only-metadata files (no cwd, no messages) aren't useful rows.
    if cwd.is_empty() {
        cwd = decode_dir_slug(path);
    }
    if last_ts == 0 {
        last_ts = file_mtime_epoch(path);
    }

    let title = if !ai_title.is_empty() {
        truncate_title(&ai_title)
    } else if let Some(p) = first_prose {
        truncate_title(&p)
    } else {
        format!("(session {})", &id[..id.len().min(8)])
    };

    Some(finalize(Backend::Claude, id, title, cwd, last_ts, messages))
}

/// Fallback when a Claude file carries no `cwd`: the directory name is the cwd
/// with `/` encoded as `-` (e.g. `-Users-u-Desktop-x`).
fn decode_dir_slug(path: &Path) -> String {
    path.parent()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().replace('-', "/"))
        .unwrap_or_default()
}

fn parse_codex_file(path: &Path) -> Option<Session> {
    let lines = read_lines(path)?;

    let mut id = String::new();
    let mut cwd = String::new();
    let mut name = String::new();
    let mut messages: Vec<Message> = Vec::new();
    let mut first_prose: Option<String> = None;
    let mut last_ts: i64 = 0;

    for v in &lines {
        if let Some(ts) = v.get("timestamp").and_then(Value::as_str)
            && let Some(e) = parse_iso_epoch(ts)
        {
            last_ts = last_ts.max(e);
        }

        // Records may be flat or wrapped under `payload`; look in both.
        let payload = v.get("payload").unwrap_or(v);
        let ty = v
            .get("type")
            .and_then(Value::as_str)
            .or_else(|| payload.get("type").and_then(Value::as_str))
            .unwrap_or("");

        if id.is_empty()
            && let Some(x) = payload.get("id").and_then(Value::as_str)
        {
            id = x.to_string();
        }
        if cwd.is_empty()
            && let Some(c) = payload.get("cwd").and_then(Value::as_str)
        {
            cwd = c.to_string();
        }
        if name.is_empty()
            && let Some(n) = payload
                .get("name")
                .or_else(|| payload.get("title"))
                .and_then(Value::as_str)
        {
            name = n.to_string();
        }

        // environment_context carries cwd as an XML-ish blob in older formats.
        if cwd.is_empty() && (ty == "environment_context" || ty.contains("environment")) {
            let text = extract_text(payload.get("content").unwrap_or(payload));
            if let Some(c) = extract_tag(&text, "cwd") {
                cwd = c;
            }
        }

        // Conversation turns: `message`/`response_item` with a role + content.
        let role = payload.get("role").and_then(Value::as_str);
        let content = payload.get("content");
        if let (Some(role), Some(content)) = (role, content) {
            let text = extract_text(content);
            if text.trim().is_empty() {
                continue;
            }
            if cwd.is_empty()
                && let Some(c) = extract_tag(&text, "cwd")
            {
                cwd = c;
            }
            if role == "user" && !is_prose(&text) {
                continue;
            }
            if role == "user" && first_prose.is_none() {
                first_prose = Some(text.clone());
            }
            messages.push(Message {
                role: role.to_string(),
                text,
            });
        }
    }

    // Session id from filename if not embedded: rollout-<ts>-<uuid>.jsonl.
    if id.is_empty() {
        let stem = path.file_stem()?.to_string_lossy().to_string();
        id = stem
            .rsplit('-')
            .take(5)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("-");
        if id.is_empty() {
            id = stem;
        }
    }
    if last_ts == 0 {
        last_ts = file_mtime_epoch(path);
    }

    let title = if !name.is_empty() {
        truncate_title(&name)
    } else if let Some(p) = first_prose {
        truncate_title(&p)
    } else {
        format!("(session {})", &id[..id.len().min(8)])
    };

    Some(finalize(Backend::Codex, id, title, cwd, last_ts, messages))
}

/// Extract the inner text of `<tag>...</tag>` from a blob, if present.
fn extract_tag(text: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = text.find(&open)? + open.len();
    let end = text[start..].find(&close)? + start;
    Some(text[start..end].trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_tmp(name: &str, body: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("space-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        let mut f = File::create(&p).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        p
    }

    #[test]
    fn claude_parse_basic() {
        let root = crate::space::ROOT_DIR;
        let body = format!(
            r#"
{{"type":"mode","mode":"x","sessionId":"abc"}}
{{"type":"user","message":{{"role":"user","content":[{{"type":"text","text":"fix the login bug"}}]}},"cwd":"/Users/me/{root}/proj/app","timestamp":"2026-06-30T19:20:35.301Z"}}
{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"text","text":"on it"}}]}},"timestamp":"2026-06-30T19:21:00.000Z"}}
{{"type":"ai-title","aiTitle":"Fix login bug","sessionId":"abc"}}
not even json
"#
        );
        let p = write_tmp("11111111-2222-3333-4444-555555555555.jsonl", &body);
        let s = parse_claude_file(&p).unwrap();
        assert_eq!(s.backend, Backend::Claude);
        assert_eq!(s.id, "11111111-2222-3333-4444-555555555555");
        assert_eq!(s.title, "Fix login bug"); // ai-title wins over first prose
        assert_eq!(s.cwd, format!("/Users/me/{root}/proj/app"));
        assert_eq!(s.space, "proj");
        assert_eq!(s.msg_count, 2);
        assert_eq!(
            s.last_activity,
            parse_iso_epoch("2026-06-30T19:21:00.000Z").unwrap()
        );
        assert!(s.matches("login"));
    }

    #[test]
    fn claude_title_falls_back_to_first_prose() {
        let body = r#"
{"type":"user","message":{"role":"user","content":"just a string message"},"cwd":"/tmp/x","timestamp":"2026-01-01T00:00:00Z"}
"#;
        let p = write_tmp("aaaaaaaa-0000-0000-0000-000000000000.jsonl", body);
        let s = parse_claude_file(&p).unwrap();
        assert_eq!(s.title, "just a string message");
    }

    #[test]
    fn codex_parse_env_context_and_response_items() {
        let root = crate::space::ROOT_DIR;
        let body = format!(
            r#"
{{"timestamp":"2026-05-01T10:00:00Z","type":"session_meta","payload":{{"id":"dead-beef","cwd":"/Users/me/{root}/tools/proj"}}}}
{{"timestamp":"2026-05-01T10:00:01Z","type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"refactor the parser"}}]}}}}
{{"timestamp":"2026-05-01T10:00:05Z","type":"response_item","payload":{{"type":"message","role":"assistant","content":[{{"type":"output_text","text":"done"}}]}}}}
"#
        );
        let p = write_tmp("rollout-2026-05-01-dead-beef.jsonl", &body);
        let s = parse_codex_file(&p).unwrap();
        assert_eq!(s.backend, Backend::Codex);
        assert_eq!(s.id, "dead-beef");
        assert_eq!(s.cwd, format!("/Users/me/{root}/tools/proj"));
        assert_eq!(s.space, "tools");
        assert_eq!(s.title, "refactor the parser");
        assert_eq!(s.msg_count, 2);
    }

    #[test]
    fn codex_cwd_from_environment_context_tag() {
        let root = crate::space::ROOT_DIR;
        let body = format!(
            r#"
{{"type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"<environment_context>\n<cwd>/Users/me/{root}/sp/repo</cwd>\n</environment_context>"}}]}}}}
{{"type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"hello there"}}]}}}}
"#
        );
        let p = write_tmp("rollout-2026-05-02-cafef00d.jsonl", &body);
        let s = parse_codex_file(&p).unwrap();
        assert_eq!(s.cwd, format!("/Users/me/{root}/sp/repo"));
        assert_eq!(s.space, "sp");
        assert_eq!(s.title, "hello there");
    }

    #[test]
    fn tag_extraction() {
        assert_eq!(
            extract_tag("<cwd>/a/b</cwd>", "cwd"),
            Some("/a/b".to_string())
        );
        assert_eq!(extract_tag("no tag here", "cwd"), None);
    }

    #[test]
    fn parsing_never_touches_the_file() {
        let body = r#"{"type":"user","message":{"role":"user","content":"hi"},"cwd":"/tmp/x","timestamp":"2026-01-01T00:00:00Z"}"#;
        let p = write_tmp("readonly-probe.jsonl", body);
        let before = std::fs::metadata(&p).unwrap().modified().unwrap();
        let _ = parse_claude_file(&p).unwrap();
        let _ = parse_codex_file(&p).unwrap();
        let after = std::fs::metadata(&p).unwrap().modified().unwrap();
        assert_eq!(
            before, after,
            "parsing must not modify the transcript's mtime"
        );
    }
}
