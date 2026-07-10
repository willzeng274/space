//! Shell-integration handoff: when the zsh wrapper passes `--handoff-file`,
//! one-shot effects are reported as data for the interactive shell to act on,
//! instead of the binary exec-ing shells itself.
//!
//! Format: line 1 is the cwd; each remaining line is one argv element
//! (program, then args). No quoting layer exists because the wrapper runs the
//! array directly (`"${cmd[@]}"`) — data, not code. An empty file means
//! "nothing to do".

use std::path::Path;

use anyhow::{Result, bail};

/// Serialize a handoff. `argv` empty means "cd only".
pub fn render(cwd: &Path, argv: &[String]) -> Result<String> {
    let dir = cwd.to_string_lossy();
    for piece in std::iter::once(dir.as_ref()).chain(argv.iter().map(|s| s.as_str())) {
        if piece.contains('\n') {
            bail!("handoff element contains a newline: {piece:?}");
        }
    }
    let mut out = String::new();
    out.push_str(&dir);
    for a in argv {
        out.push('\n');
        out.push_str(a);
    }
    out.push('\n');
    Ok(out)
}

/// Parse a rendered handoff back into (cwd, argv). Mirrors the zsh wrapper's
/// reading; exists so the round trip is pinned by tests.
pub fn parse(s: &str) -> Option<(String, Vec<String>)> {
    let mut lines = s.lines();
    let cwd = lines.next()?.to_string();
    if cwd.is_empty() {
        return None;
    }
    Some((cwd, lines.map(|l| l.to_string()).collect()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn round_trip_cd_only() {
        let s = render(&PathBuf::from("/Users/u/Desktop/proj"), &[]).unwrap();
        assert_eq!(
            parse(&s),
            Some(("/Users/u/Desktop/proj".to_string(), vec![]))
        );
    }

    #[test]
    fn round_trip_agent_launch_with_hostile_path() {
        // Spaces, quotes, dollar signs: all inert because this is data.
        let cwd = PathBuf::from("/tmp/my space/$(boom)'\"");
        let argv = vec![
            "claude".to_string(),
            "--resume".to_string(),
            "abc-123".to_string(),
            "--dangerously-skip-permissions".to_string(),
        ];
        let s = render(&cwd, &argv).unwrap();
        let (c, a) = parse(&s).unwrap();
        assert_eq!(c, cwd.to_string_lossy());
        assert_eq!(a, argv);
    }

    #[test]
    fn newline_in_path_is_rejected() {
        assert!(render(&PathBuf::from("/tmp/evil\ndir"), &[]).is_err());
        assert!(render(&PathBuf::from("/tmp"), &["a\nb".to_string()]).is_err());
    }

    #[test]
    fn empty_file_means_do_nothing() {
        assert_eq!(parse(""), None);
    }

    #[test]
    fn wrapper_script_is_valid_zsh() {
        // Golden check: the emitted wrapper must at least parse.
        let script = include_str!("../shell/space.zsh");
        let out = std::process::Command::new("zsh")
            .arg("-n")
            .arg("-c")
            .arg(script)
            .output();
        if let Ok(out) = out {
            assert!(
                out.status.success(),
                "zsh -n rejected the wrapper: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        } // zsh missing: skip silently (CI without zsh)
    }
}
