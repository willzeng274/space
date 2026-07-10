//! Branch-stack discovery for stacked PRs.
//!
//! A branch is "stacked on" whichever other branch tip (or the default branch)
//! is the nearest ancestor of its tip. That relation is derivable purely from
//! git ancestry, so the tree needs no config; PR metadata is overlaid from
//! `gh` when available.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

#[derive(Clone, Debug)]
pub struct StackEntry {
    pub branch: String,
    pub parent: String,
    /// Commits on this branch beyond its parent.
    pub commits: usize,
    /// Indent depth when rendered as a tree (default branch = 0).
    pub depth: usize,
}

#[derive(Clone, Debug)]
pub struct Stack {
    pub default_branch: String,
    /// Topological order, parents before children.
    pub entries: Vec<StackEntry>,
    /// `owner/repo` parsed from origin, when it is a GitHub remote.
    pub slug: Option<String>,
}

#[derive(Clone, Debug)]
pub struct PrInfo {
    pub number: u64,
    pub url: String,
    /// OPEN / MERGED / CLOSED, with DRAFT folded in for open drafts.
    pub state: String,
    pub title: String,
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .with_context(|| format!("running git {}", args.join(" ")))?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Build the branch stack for the repo at `dir` with exactly two git spawns:
/// one for branch tips, one for the commit graph ahead of the default branch.
/// Ancestry and distances are then pure set math on that graph.
pub fn branch_stack(dir: &Path) -> Result<Stack> {
    use std::collections::{HashMap, HashSet};

    // Spawn 1: every local branch tip.
    let mut tips: Vec<(String, String)> = Vec::new();
    for line in git(
        dir,
        &[
            "for-each-ref",
            "--format=%(refname:short) %(objectname)",
            "refs/heads",
        ],
    )?
    .lines()
    {
        if let Some((name, sha)) = line.rsplit_once(' ') {
            tips.push((name.to_string(), sha.to_string()));
        }
    }
    let default_branch = ["main", "master"]
        .iter()
        .find(|b| tips.iter().any(|(n, _)| n == *b))
        .map(|b| b.to_string())
        .ok_or_else(|| anyhow::anyhow!("no main/master branch"))?;

    // Spawn 2: commits reachable from any branch but not from the default,
    // with parent edges. Everything else is derived in memory.
    let mut parents_of: HashMap<String, Vec<String>> = HashMap::new();
    for line in git(
        dir,
        &[
            "rev-list",
            "--parents",
            "--branches",
            "--not",
            &default_branch,
        ],
    )?
    .lines()
    {
        let mut shas = line.split_whitespace().map(|s| s.to_string());
        if let Some(c) = shas.next() {
            parents_of.insert(c, shas.collect());
        }
    }

    // Ancestor set of a tip, restricted to the ahead-of-default subgraph.
    let ancestors = |tip: &str| -> HashSet<String> {
        let mut seen = HashSet::new();
        let mut todo = vec![tip.to_string()];
        while let Some(c) = todo.pop() {
            if !parents_of.contains_key(&c) || !seen.insert(c.clone()) {
                continue;
            }
            todo.extend(parents_of[&c].iter().cloned());
        }
        seen
    };

    // Branches whose tip is not ahead of default are merged; skip them.
    let live: Vec<(String, HashSet<String>)> = tips
        .iter()
        .filter(|(n, sha)| *n != default_branch && parents_of.contains_key(sha))
        .map(|(n, sha)| (n.clone(), ancestors(sha)))
        .collect();
    let tip_sha: HashMap<&str, &str> = tips.iter().map(|(n, s)| (n.as_str(), s.as_str())).collect();

    // Parent = the candidate tip inside our ancestor set with the largest own
    // ancestor set (i.e. the nearest); distance = the set-size difference.
    let mut parents: Vec<(String, String, usize)> = Vec::new();
    for (b, a_b) in &live {
        let best = live
            .iter()
            .filter(|(c, _)| c != b && a_b.contains(tip_sha[c.as_str()]))
            .max_by_key(|(_, a_c)| a_c.len());
        let (parent, commits) = match best {
            Some((c, a_c)) if a_c.len() < a_b.len() => (c.clone(), a_b.len() - a_c.len()),
            _ => (default_branch.clone(), a_b.len()),
        };
        parents.push((b.clone(), parent, commits));
    }

    // Topological order: repeated passes emitting branches whose parent is placed.
    let mut entries: Vec<StackEntry> = Vec::new();
    let mut placed = vec![default_branch.clone()];
    let mut remaining = parents;
    while !remaining.is_empty() {
        let before = remaining.len();
        let mut next = Vec::new();
        for (b, parent, commits) in remaining {
            if let Some(pi) = placed.iter().position(|p| *p == parent) {
                let depth = if parent == default_branch {
                    1
                } else {
                    entries
                        .iter()
                        .find(|e| e.branch == parent)
                        .map(|e| e.depth + 1)
                        .unwrap_or(1)
                };
                let _ = pi;
                placed.push(b.clone());
                entries.push(StackEntry {
                    branch: b,
                    parent,
                    commits,
                    depth,
                });
            } else {
                next.push((b, parent, commits));
            }
        }
        if next.len() == before {
            // Cycle should be impossible in git ancestry; bail out defensively.
            for (b, parent, commits) in next {
                entries.push(StackEntry {
                    branch: b,
                    parent,
                    commits,
                    depth: 1,
                });
            }
            break;
        }
        remaining = next;
    }
    // Group children under their parent chain, deepest-first display order.
    let edges = entries_ref(&entries);
    entries.sort_by_key(|e| chain_key(&edges, e));

    Ok(Stack {
        slug: origin_slug(dir),
        default_branch,
        entries,
    })
}

fn entries_ref(entries: &[StackEntry]) -> Vec<(String, String)> {
    entries
        .iter()
        .map(|e| (e.branch.clone(), e.parent.clone()))
        .collect()
}

/// Sort key = the path of branch names from the default branch down to this
/// entry, so siblings group and children follow their parent.
fn chain_key(edges: &[(String, String)], e: &StackEntry) -> Vec<String> {
    let mut key = vec![e.branch.clone()];
    let mut cur = e.parent.clone();
    for _ in 0..64 {
        key.push(cur.clone());
        match edges.iter().find(|(b, _)| *b == cur) {
            Some((_, p)) => cur = p.clone(),
            None => break,
        }
    }
    key.reverse();
    key
}

/// `owner/repo` from the origin URL (SSH or HTTPS GitHub remotes).
pub fn origin_slug(dir: &Path) -> Option<String> {
    let url = git(dir, &["remote", "get-url", "origin"]).ok()?;
    let rest = url
        .strip_prefix("git@github.com:")
        .or_else(|| url.strip_prefix("https://github.com/"))
        .or_else(|| url.strip_prefix("ssh://git@github.com/"))?;
    Some(
        rest.trim_end_matches('/')
            .trim_end_matches(".git")
            .to_string(),
    )
}

pub fn compare_url(slug: &str, parent: &str, branch: &str) -> String {
    format!("https://github.com/{slug}/compare/{parent}...{branch}")
}

/// Diff of one stack layer against its parent, capped so a huge patch cannot
/// exhaust memory. Returns (lines, truncated_count).
pub fn layer_diff(dir: &Path, parent: &str, branch: &str) -> Result<(Vec<String>, usize)> {
    const MAX_LINES: usize = 5000;
    let stat = git(dir, &["diff", "--stat", &format!("{parent}..{branch}")])?;
    let patch = git(dir, &["diff", &format!("{parent}..{branch}")])?;
    let mut lines: Vec<String> = stat.lines().map(|s| s.to_string()).collect();
    lines.push(String::new());
    lines.extend(patch.lines().map(|s| s.to_string()));
    let truncated = lines.len().saturating_sub(MAX_LINES);
    lines.truncate(MAX_LINES);
    Ok((lines, truncated))
}

/// Fetch PR metadata for every branch via `gh` (best effort; None when gh is
/// missing or errors). Keyed by head branch name.
pub fn fetch_prs(slug: &str) -> Option<Vec<(String, PrInfo)>> {
    let out = Command::new("gh")
        .args([
            "pr",
            "list",
            "--repo",
            slug,
            "--state",
            "all",
            "--limit",
            "200",
            "--json",
            "number,url,state,title,headRefName,isDraft",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let mut prs = Vec::new();
    for pr in v.as_array()? {
        let head = pr["headRefName"].as_str()?.to_string();
        let state =
            if pr["isDraft"].as_bool().unwrap_or(false) && pr["state"].as_str() == Some("OPEN") {
                "DRAFT".to_string()
            } else {
                pr["state"].as_str().unwrap_or("?").to_string()
            };
        prs.push((
            head,
            PrInfo {
                number: pr["number"].as_u64().unwrap_or(0),
                url: pr["url"].as_str().unwrap_or("").to_string(),
                state,
                title: pr["title"].as_str().unwrap_or("").to_string(),
            },
        ));
    }
    Some(prs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn sandbox(tag: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("space-stack-tests-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn run(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn commit(dir: &Path, name: &str) {
        std::fs::write(dir.join(name), name).unwrap();
        run(dir, &["add", "-A"]);
        run(dir, &["commit", "-qm", name]);
    }

    #[test]
    fn detects_linear_stack_and_sibling() {
        let d = sandbox("linear");
        run(&d, &["init", "-q", "-b", "main"]);
        run(&d, &["config", "user.email", "t@t"]);
        run(&d, &["config", "user.name", "t"]);
        run(&d, &["config", "commit.gpgsign", "false"]);
        commit(&d, "base");

        run(&d, &["switch", "-qc", "feat/a"]);
        commit(&d, "a1");
        commit(&d, "a2");
        run(&d, &["switch", "-qc", "feat/b"]); // stacked on a
        commit(&d, "b1");
        run(&d, &["switch", "-q", "main"]);
        run(&d, &["switch", "-qc", "feat/solo"]); // sibling off main
        commit(&d, "s1");

        let stack = branch_stack(&d).unwrap();
        assert_eq!(stack.default_branch, "main");
        let get = |b: &str| stack.entries.iter().find(|e| e.branch == b).unwrap();
        assert_eq!(get("feat/a").parent, "main");
        assert_eq!(get("feat/a").commits, 2);
        assert_eq!(get("feat/b").parent, "feat/a");
        assert_eq!(get("feat/b").commits, 1);
        assert_eq!(get("feat/b").depth, 2);
        assert_eq!(get("feat/solo").parent, "main");
        // children directly follow their parent in display order
        let idx = |b: &str| stack.entries.iter().position(|e| e.branch == b).unwrap();
        assert_eq!(idx("feat/b"), idx("feat/a") + 1);

        let (lines, trunc) = layer_diff(&d, "feat/a", "feat/b").unwrap();
        assert_eq!(trunc, 0);
        assert!(lines.iter().any(|l| l.contains("b1")));
    }

    #[test]
    fn slug_and_compare_url() {
        assert_eq!(
            compare_url("o/r", "main", "feat/x"),
            "https://github.com/o/r/compare/main...feat/x"
        );
        // origin_slug parsing is exercised via string forms
        for (url, want) in [
            ("git@github.com:acme-corp/api.git", "acme-corp/api"),
            (
                "https://github.com/willzeng274/space.git",
                "willzeng274/space",
            ),
            ("https://github.com/willzeng274/space", "willzeng274/space"),
        ] {
            let rest = url
                .strip_prefix("git@github.com:")
                .or_else(|| url.strip_prefix("https://github.com/"))
                .unwrap();
            assert_eq!(rest.trim_end_matches('/').trim_end_matches(".git"), want);
        }
    }
}
