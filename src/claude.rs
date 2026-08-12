use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;

#[derive(Serialize)]
struct CustomTitle<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    #[serde(rename = "customTitle")]
    custom_title: &'a str,
    #[serde(rename = "sessionId")]
    session_id: &'a str,
}

pub fn projects_root() -> Option<PathBuf> {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".claude")))
        .map(|root| root.join("projects"))
}

pub fn set_session_name(session_id: &str, title: &str) -> Result<()> {
    let root = projects_root().context("finding the Claude config directory")?;
    set_session_name_at(&root, session_id, title)
}

fn set_session_name_at(root: &Path, session_id: &str, title: &str) -> Result<()> {
    if !is_uuid(session_id) {
        bail!("invalid Claude session id {session_id:?}");
    }
    let title = title.trim();
    if title.is_empty() {
        bail!("conversation name cannot be empty");
    }

    let filename = format!("{session_id}.jsonl");
    let transcript = std::fs::read_dir(root)
        .with_context(|| format!("reading {}", root.display()))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path().join(&filename))
        .find(|path| {
            std::fs::metadata(path)
                .map(|metadata| metadata.is_file() && metadata.len() > 0)
                .unwrap_or(false)
        })
        .with_context(|| format!("Claude session {session_id} was not found"))?;

    let entry = CustomTitle {
        kind: "custom-title",
        custom_title: title,
        session_id,
    };
    let mut line = serde_json::to_vec(&entry)?;
    line.push(b'\n');
    OpenOptions::new()
        .append(true)
        .open(&transcript)
        .with_context(|| format!("opening {}", transcript.display()))?
        .write_all(&line)
        .with_context(|| format!("renaming Claude session {session_id}"))?;
    Ok(())
}

fn is_uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "space-claude-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn rename_appends_claudes_native_custom_title_entry() {
        let root = temp_root("rename");
        let project = root.join("-tmp-project");
        std::fs::create_dir(&project).unwrap();
        let id = "11111111-2222-3333-4444-555555555555";
        let transcript = project.join(format!("{id}.jsonl"));
        std::fs::write(&transcript, "{\"type\":\"user\"}\n").unwrap();

        set_session_name_at(&root, id, "  useful name  ").unwrap();

        let body = std::fs::read_to_string(&transcript).unwrap();
        assert!(body.ends_with(
            "{\"type\":\"custom-title\",\"customTitle\":\"useful name\",\"sessionId\":\"11111111-2222-3333-4444-555555555555\"}\n"
        ));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rename_does_not_create_a_missing_transcript() {
        let root = temp_root("missing");
        let error =
            set_session_name_at(&root, "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee", "name").unwrap_err();
        assert!(error.to_string().contains("was not found"));
        std::fs::remove_dir_all(root).unwrap();
    }
}
