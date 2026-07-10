//! Space management: the canonical repo pool, spaces (folders under the
//! compile-time ROOT_DIR, default `~/Desktop`), and the symlink / git-worktree
//! operations that compose them.
//!
//! Model:
//! - `<root>/<pool>/<repo>`: canonical checkouts, the source of truth. We never
//!   clone here; the user populates it.
//! - `<root>/<space>/`: a *space*, marked by a `.space.toml` file. Its members
//!   are symlinks into the pool (default) or git worktrees (once a branch is made).
//!   Membership is derived from the filesystem, so config never drifts from reality.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};

pub const MARKER: &str = ".space.toml";

/// Layout, fixed at compile time. Override when building:
///   SPACE_ROOT=workspaces SPACE_POOL=checkouts cargo build --release
/// ROOT_DIR is relative to $HOME and may contain slashes; POOL_DIR is the
/// pool folder name directly under it.
pub const ROOT_DIR: &str = match option_env!("SPACE_ROOT") {
    Some(v) => v,
    None => "Desktop",
};
pub const POOL_DIR: &str = match option_env!("SPACE_POOL") {
    Some(v) => v,
    None => "repos",
};

/// `~/<ROOT_DIR>` for user-facing messages.
pub fn root_display() -> String {
    format!("~/{ROOT_DIR}")
}

/// `~/<ROOT_DIR>/<POOL_DIR>` for user-facing messages.
pub fn pool_display() -> String {
    format!("~/{ROOT_DIR}/{POOL_DIR}")
}

/// A canonical repo available in the pool.
#[derive(Clone, Debug)]
pub struct PoolRepo {
    pub name: String,
    pub path: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RepoState {
    /// Symlink into the pool.
    Symlink,
    /// A git worktree checked out on `branch`.
    Worktree { branch: String },
    /// Something we didn't create (a real dir / stray file) — shown, never touched.
    Foreign,
}

#[derive(Clone, Debug)]
pub struct SpaceRepo {
    pub name: String,
    pub state: RepoState,
}

#[derive(Clone, Debug)]
pub struct Space {
    pub name: String,
    pub path: PathBuf,
    pub repos: Vec<SpaceRepo>,
}

pub fn desktop_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(ROOT_DIR))
}

pub fn pool_dir() -> Option<PathBuf> {
    desktop_dir().map(|d| d.join(POOL_DIR))
}

/// The pool is always a sibling of spaces (`<desktop>/repos`), so derive it from
/// the space's own location rather than a global — keeps ops relocatable + testable.
fn pool_for(space: &Path) -> Option<PathBuf> {
    space.parent().map(|p| p.join(POOL_DIR))
}

/// Repos available to add. The pool may be flat (`repos/<repo>`) or grouped one
/// level deep (`repos/us/<repo>`, `repos/defi/<repo>`); a group is any pool
/// subdirectory that isn't itself a git repo. `name` is the pool-relative path
/// (`acme/api`), which is what add/picker use.
pub fn list_pool() -> Vec<PoolRepo> {
    let Some(dir) = pool_dir() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    collect_pool(&dir, "", &mut out);
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

fn collect_pool(dir: &Path, prefix: &str, out: &mut Vec<PoolRepo>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let path = e.path();
        if !path.is_dir() {
            continue;
        }
        let Some(base) = path.file_name().map(|n| n.to_string_lossy().to_string()) else {
            continue;
        };
        if base.starts_with('.') {
            continue;
        }
        let name = if prefix.is_empty() {
            base
        } else {
            format!("{prefix}/{base}")
        };
        if path.join(".git").exists() {
            out.push(PoolRepo { name, path });
        } else if prefix.is_empty() {
            collect_pool(&path, &name, out);
        }
    }
}

/// All spaces: folders directly under the root carrying the marker file
/// (the `repos` pool is excluded even if it somehow has one).
pub fn list_spaces() -> Vec<Space> {
    let Some(desktop) = desktop_dir() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(&desktop) else {
        return out;
    };
    for e in entries.flatten() {
        let path = e.path();
        if !path.is_dir() || path.file_name().map(|n| n == POOL_DIR).unwrap_or(false) {
            continue;
        }
        if path.join(MARKER).is_file() {
            out.push(read_space(&path));
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

pub fn is_space(dir: &Path) -> bool {
    dir.join(MARKER).is_file()
}

/// Walk up from `start` to the nearest enclosing space, if any.
pub fn enclosing_space(start: &Path) -> Option<PathBuf> {
    let mut cur = Some(start);
    while let Some(dir) = cur {
        if is_space(dir) {
            return Some(dir.to_path_buf());
        }
        cur = dir.parent();
    }
    None
}

fn read_space(path: &Path) -> Space {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let mut repos = Vec::new();
    if let Ok(entries) = std::fs::read_dir(path) {
        for e in entries.flatten() {
            let p = e.path();
            let fname = e.file_name().to_string_lossy().to_string();
            // Skip the marker, generated policy/workspace files, and dotfiles.
            if fname.starts_with('.')
                || fname == "CLAUDE.md"
                || fname == "AGENTS.md"
                || fname.ends_with(".code-workspace")
            {
                continue;
            }
            let Ok(meta) = std::fs::symlink_metadata(&p) else {
                continue;
            };
            // Plain files at the space root are sanctioned cross-repo scratch
            // (notes, drafts); they are not members and never touched.
            if meta.is_file() {
                continue;
            }
            let state = if meta.file_type().is_symlink() {
                RepoState::Symlink
            } else if meta.is_dir()
                && let Some(branch) = linked_worktree_branch(&p)
            {
                RepoState::Worktree { branch }
            } else {
                // Plain files/dirs, and full clones (`.git` directory) alike:
                // not created by this tool.
                RepoState::Foreign
            };
            repos.push(SpaceRepo { name: fname, state });
        }
    }
    repos.sort_by(|a, b| a.name.cmp(&b.name));
    Space {
        name,
        path: path.to_path_buf(),
        repos,
    }
}

// ---- git helpers ----------------------------------------------------------

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

/// Branch of a *linked* git worktree, detected without spawning git: a linked
/// worktree has a `.git` file (`gitdir: <path>`) rather than a directory, and
/// that gitdir's HEAD names the branch. Returns None for full clones and
/// non-repos, so stray clones inside a space stay classified as unmanaged.
fn linked_worktree_branch(dir: &Path) -> Option<String> {
    let git_file = dir.join(".git");
    if !git_file.is_file() {
        return None;
    }
    let gitdir = std::fs::read_to_string(&git_file).ok()?;
    let gitdir = gitdir.strip_prefix("gitdir:")?.trim();
    let head = std::fs::read_to_string(Path::new(gitdir).join("HEAD")).ok()?;
    let head = head.trim();
    Some(match head.strip_prefix("ref: refs/heads/") {
        Some(branch) => branch.to_string(),
        // Detached HEAD: show the short commit id.
        None => head.chars().take(7).collect(),
    })
}

fn is_git_worktree(dir: &Path) -> bool {
    git(dir, &["rev-parse", "--is-inside-work-tree"])
        .map(|s| s == "true")
        .unwrap_or(false)
}

// ---- mutations ------------------------------------------------------------

/// Create a new space folder with the marker and both agent policy files.
pub fn create_space(name: &str) -> Result<PathBuf> {
    let name = name.trim();
    if name.is_empty() {
        bail!("space name cannot be empty");
    }
    if name == POOL_DIR {
        bail!("`{POOL_DIR}` is reserved for the canonical repo pool");
    }
    if name.contains('/') || name.starts_with('.') {
        bail!("invalid space name");
    }
    let desktop = desktop_dir().ok_or_else(|| anyhow!("cannot locate {}", root_display()))?;
    let path = desktop.join(name);
    if path.exists() {
        bail!("{}/{name} already exists", root_display());
    }
    std::fs::create_dir(&path).with_context(|| format!("creating {}", path.display()))?;
    write_space_files(&path, name)?;
    refresh_policy_files(&path)?;
    Ok(path)
}

/// (Re)write the marker + CLAUDE.md/AGENTS.md so the policy stays in sync.
pub fn write_space_files(space: &Path, name: &str) -> Result<()> {
    std::fs::write(
        space.join(MARKER),
        format!("# managed by `space`; presence marks this folder as a space\nname = \"{name}\"\n"),
    )?;
    let policy = policy_markdown(name);
    std::fs::write(space.join("CLAUDE.md"), &policy)?;
    std::fs::write(space.join("AGENTS.md"), &policy)?;
    Ok(())
}

/// Atomically replace `path` with `content` when it differs: write to a
/// dot-tmp sibling, then rename. VS Code watches the `.code-workspace` file,
/// so a plain truncate-and-write could be read half-written and momentarily
/// drop every workspace root.
fn write_if_changed(path: &Path, content: &str) -> Result<()> {
    if std::fs::read_to_string(path).ok().as_deref() == Some(content) {
        return Ok(());
    }
    let tmp = path.with_file_name(format!(
        ".{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy()
    ));
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

/// Bring an existing space's generated files (CLAUDE.md/AGENTS.md and the
/// VS Code `.code-workspace`) up to date with this binary's template and the
/// space's current membership. No-op (no writes, no mtime churn) when current.
pub fn refresh_policy_files(space: &Path) -> Result<()> {
    let name = space
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let policy = policy_markdown(&name);
    for f in ["CLAUDE.md", "AGENTS.md"] {
        write_if_changed(&space.join(f), &policy)?;
    }
    let ws = workspace_json(&read_space(space));
    write_if_changed(&space.join(workspace_filename(&name)), &ws)?;
    Ok(())
}

pub fn workspace_filename(name: &str) -> String {
    format!("{name}.code-workspace")
}

/// VS Code multi-root workspace over the space's repos. Folder entries are
/// relative names, so symlinks and worktrees both open as first-class roots
/// (each with its own git integration).
fn workspace_json(space: &Space) -> String {
    let js = |s: &str| serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into());
    let members: Vec<&SpaceRepo> = space
        .repos
        .iter()
        .filter(|r| r.state != RepoState::Foreign)
        .collect();

    // Space root first so CLAUDE.md/AGENTS.md are visible at top level; the
    // member repos are excluded from that root's view since they follow as
    // their own workspace roots (VS Code keeps folders in file order).
    let mut folders = vec![format!(
        "    {{ \"name\": {}, \"path\": \".\" }}",
        js(&format!("{} (space)", space.name))
    )];
    // Worktree roots are labeled with their branch so the sidebar shows at a
    // glance which repos are branch-isolated in this space (symlinks stay plain).
    folders.extend(members.iter().map(|r| match &r.state {
        RepoState::Worktree { branch } => format!(
            "    {{ \"name\": {}, \"path\": {} }}",
            js(&format!("{} ({})", r.name, branch)),
            js(&r.name)
        ),
        _ => format!("    {{ \"path\": {} }}", js(&r.name)),
    }));

    let excludes: Vec<String> = members
        .iter()
        .map(|r| format!("      {}: true", js(&r.name)))
        .collect();
    format!(
        "{{\n  \"folders\": [\n{}\n  ],\n  \"settings\": {{\n    \"files.exclude\": {{\n{}\n    }}\n  }}\n}}\n",
        folders.join(",\n"),
        excludes.join(",\n")
    )
}

/// Resolve a repo name against the pool: accepts a pool-relative path
/// (`acme/api`) or a bare basename (`api`) when unambiguous.
pub fn resolve_pool_repo(space: &Path, name: &str) -> Result<String> {
    let pool = pool_for(space).ok_or_else(|| anyhow!("cannot locate the repo pool"))?;
    if pool.join(name).join(".git").exists() {
        return Ok(name.to_string());
    }
    let mut all: Vec<PoolRepo> = Vec::new();
    collect_pool(&pool, "", &mut all);
    let hits: Vec<&PoolRepo> = all
        .iter()
        .filter(|p| p.name.rsplit('/').next() == Some(name))
        .collect();
    match hits.as_slice() {
        [one] => Ok(one.name.clone()),
        [] => bail!(
            "no repo `{name}` in the pool; available: {}",
            all.iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
        many => bail!(
            "`{name}` is ambiguous: {}",
            many.iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Read a space's members (public wrapper used by the CLI).
pub fn members(space: &Path) -> Vec<SpaceRepo> {
    read_space(space).repos
}

/// Add a pool repo to a space as a symlink. `repo` is pool-relative and may
/// include a group (`acme/api`); the link in the space is named by the
/// repo's basename.
pub fn add_repo(space: &Path, repo: &str) -> Result<()> {
    let pool = pool_for(space).ok_or_else(|| anyhow!("cannot locate the repo pool"))?;
    let src = pool.join(repo);
    if !src.is_dir() {
        bail!("no repo `{repo}` in {}", pool_display());
    }
    let base = Path::new(repo)
        .file_name()
        .ok_or_else(|| anyhow!("invalid repo name `{repo}`"))?;
    let dst = space.join(base);
    if dst.exists() || std::fs::symlink_metadata(&dst).is_ok() {
        bail!("`{}` is already in this space", base.to_string_lossy());
    }
    symlink(&src, &dst).with_context(|| format!("symlinking {repo}"))?;
    Ok(())
}

/// Remove a repo from a space (unlink a symlink, or `git worktree remove`).
pub fn remove_repo(space: &Path, repo: &str) -> Result<()> {
    let dst = space.join(repo);
    let meta = std::fs::symlink_metadata(&dst)
        .with_context(|| format!("`{repo}` is not in this space"))?;
    if meta.file_type().is_symlink() {
        std::fs::remove_file(&dst)?;
        return Ok(());
    }
    if meta.is_dir() && is_git_worktree(&dst) {
        let canonical = canonical_of_worktree(&dst)?;
        remove_worktree_safely(&canonical, &dst, repo)?;
        return Ok(());
    }
    bail!("`{repo}` is not a space-managed symlink or worktree; leaving it alone");
}

/// Remove a worktree without ever discarding work:
/// - uncommitted tracked changes refuse with a clear message (never forced)
/// - untracked files (loose notes) are carried back to the canonical repo first
fn remove_worktree_safely(canonical: &Path, dst: &Path, repo: &str) -> Result<()> {
    if git(canonical, &["worktree", "remove", &dst.to_string_lossy()]).is_ok() {
        return Ok(());
    }
    let status = git(dst, &["status", "--porcelain"])?;
    let dirty = status.lines().filter(|l| !l.starts_with("??")).count();
    if dirty > 0 {
        bail!(
            "`{repo}` has {dirty} uncommitted change{}; commit, stash, or discard first",
            if dirty == 1 { "" } else { "s" }
        );
    }
    for line in status.lines() {
        let Some(rel) = line.strip_prefix("?? ") else {
            continue;
        };
        let from = dst.join(rel);
        let to = canonical.join(rel);
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::copy(&from, &to)
            .with_context(|| format!("carrying {rel} back to the canonical repo"))?;
    }
    git(
        canonical,
        &["worktree", "remove", "--force", &dst.to_string_lossy()],
    )?;
    Ok(())
}

/// Directory name for a worktree: `<repo>-<branch>` with path separators
/// flattened, so every worktree names its branch in the dir listing.
pub fn worktree_dir_name(repo: &str, branch: &str) -> String {
    let slug: String = branch
        .chars()
        .map(|c| {
            if c == '/' || c.is_whitespace() {
                '-'
            } else {
                c
            }
        })
        .collect();
    format!("{repo}-{slug}")
}

/// Add a worktree of `repo` on `branch` as `<repo>-<branch-slug>`, next to the
/// repo's symlink, which always stays as the canonical view. Run again with
/// another branch for another parallel worktree; git's only constraint is that
/// a branch can be checked out in at most one worktree at a time. Loose
/// untracked files and gitignored `*.md` notes are copied in.
pub fn promote_to_worktree(space: &Path, repo: &str, branch: &str) -> Result<String> {
    let branch = branch.trim();
    if branch.is_empty() {
        bail!("branch name cannot be empty");
    }
    // Resolve the canonical repo from whatever form the member has (symlink,
    // existing worktree, legacy swap-style worktree) or straight from the pool.
    let member = space.join(repo);
    let canonical = match std::fs::symlink_metadata(&member) {
        Ok(m) if m.file_type().is_symlink() => std::fs::read_link(&member)?,
        Ok(m) if m.is_dir() && linked_worktree_branch(&member).is_some() => {
            canonical_of_worktree(&member)?
        }
        _ => {
            let pool = pool_for(space).ok_or_else(|| anyhow!("cannot locate the repo pool"))?;
            pool.join(resolve_pool_repo(space, repo)?)
        }
    };
    if !canonical.join(".git").exists() {
        bail!("cannot resolve a canonical repo for `{repo}`");
    }
    let repo_base = canonical
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| repo.to_string());

    let name = worktree_dir_name(&repo_base, branch);
    let dst = space.join(&name);
    if std::fs::symlink_metadata(&dst).is_ok() {
        bail!("`{name}` already exists in this space");
    }

    let dst_str = dst.to_string_lossy().to_string();
    // New branch if possible, else check out an existing one into the worktree.
    git(&canonical, &["worktree", "add", &dst_str, "-b", branch])
        .or_else(|_| git(&canonical, &["worktree", "add", &dst_str, branch]))?;
    copy_loose_files(&canonical, &dst)?;
    Ok(name)
}

/// Remove a worktree member and ensure the repo's plain symlink exists, so
/// closing branch work always lands back on the canonical view. Handles both
/// branch-named worktrees and legacy swap-style ones under the bare repo name.
pub fn revert_to_symlink(space: &Path, member: &str) -> Result<()> {
    let dst = space.join(member);
    if !is_git_worktree(&dst) {
        bail!("`{member}` is not a worktree");
    }
    let canonical = canonical_of_worktree(&dst)?;
    remove_worktree_safely(&canonical, &dst, member)?;
    if dst.exists() {
        std::fs::remove_dir_all(&dst).ok();
    }
    let base = canonical
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| member.to_string());
    let link = space.join(&base);
    if std::fs::symlink_metadata(&link).is_err() {
        symlink(&canonical, &link)?;
    }
    Ok(())
}

/// Delete a space: unlink every managed member (worktrees via git), drop the
/// generated files, then remove the now-empty folder. Refuses up front, before
/// touching anything, if the space holds files or dirs the tool didn't create,
/// so a rejected delete never leaves a half-dismantled space.
pub fn delete_space(space: &Path) -> Result<()> {
    if !is_space(space) {
        bail!("not a space");
    }
    let name = space
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let ws = workspace_filename(&name);

    let s = read_space(space);
    if let Some(foreign) = s.repos.iter().find(|r| r.state == RepoState::Foreign) {
        bail!(
            "`{}` is not space-managed; remove it by hand first",
            foreign.name
        );
    }
    // Scratch notes at the root are the user's; refuse before dismantling anything.
    let notes: Vec<String> = std::fs::read_dir(space)
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| e.path().is_file())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .filter(|f| !f.starts_with('.') && f != "CLAUDE.md" && f != "AGENTS.md" && *f != ws)
                .collect()
        })
        .unwrap_or_default();
    if !notes.is_empty() {
        bail!(
            "space contains your files ({}); move or delete them first",
            notes.join(", ")
        );
    }

    for r in &s.repos {
        remove_repo(space, &r.name)?;
    }
    for f in [MARKER, "CLAUDE.md", "AGENTS.md", ws.as_str()] {
        let p = space.join(f);
        if p.exists() {
            std::fs::remove_file(&p).ok();
        }
    }
    std::fs::remove_dir(space)
        .map_err(|_| anyhow!("space not empty after removing managed content; left in place"))
}

/// Pull the latest default-branch commits into a space member, safely:
/// - fetch `origin` first (never fails the working tree)
/// - on main/master: fast-forward only; refuse if dirty or diverged
/// - on any other branch (worktrees): fetch only, never merge under the agent
pub fn pull_main(dir: &Path) -> Result<String> {
    git(dir, &["fetch", "origin", "--prune"])?;

    let default = ["main", "master"]
        .iter()
        .find(|b| {
            git(
                dir,
                &["show-ref", "--verify", &format!("refs/remotes/origin/{b}")],
            )
            .is_ok()
        })
        .copied()
        .ok_or_else(|| anyhow!("origin has neither main nor master"))?;
    let current = git(dir, &["rev-parse", "--abbrev-ref", "HEAD"])?;

    if current != default {
        return Ok(format!(
            "fetched origin/{default} (on {current}, not merged)"
        ));
    }

    let behind = git(
        dir,
        &["rev-list", "--count", &format!("HEAD..origin/{default}")],
    )?;
    if behind == "0" {
        return Ok("already up to date".to_string());
    }
    if !git(dir, &["status", "--porcelain"])?.is_empty() {
        bail!("{behind} commits behind, but working tree has uncommitted changes");
    }
    git(dir, &["merge", "--ff-only", &format!("origin/{default}")]).map_err(|_| {
        anyhow!("{behind} commits behind, but local {default} has diverged (no fast-forward)")
    })?;
    Ok(format!(
        "updated {behind} commit{}",
        if behind == "1" { "" } else { "s" }
    ))
}

fn canonical_of_worktree(dir: &Path) -> Result<PathBuf> {
    // git-common-dir points at the shared `.git`; its parent is the canonical repo.
    let common = git(dir, &["rev-parse", "--git-common-dir"])?;
    let common_path = {
        let p = PathBuf::from(&common);
        if p.is_absolute() { p } else { dir.join(p) }
    };
    common_path
        .parent()
        .map(|p| p.to_path_buf())
        .ok_or_else(|| anyhow!("cannot resolve canonical repo for worktree"))
}

/// Copy untracked files (and ignored `*.md` notes) from canonical into a fresh
/// worktree. A worktree only materializes tracked files, so loose notes would
/// otherwise vanish; we deliberately skip bulky ignored trees (node_modules,
/// target, …) by only bringing ignored markdown across.
fn copy_loose_files(canonical: &Path, dest: &Path) -> Result<()> {
    let untracked = git(canonical, &["ls-files", "--others", "--exclude-standard"])?;
    let ignored_md: Vec<String> = git(
        canonical,
        &["ls-files", "--others", "--ignored", "--exclude-standard"],
    )
    .unwrap_or_default()
    .lines()
    .filter(|l| l.ends_with(".md"))
    .map(|s| s.to_string())
    .collect();

    for rel in untracked
        .lines()
        .chain(ignored_md.iter().map(|s| s.as_str()))
    {
        if rel.is_empty() {
            continue;
        }
        let from = canonical.join(rel);
        let to = dest.join(rel);
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::copy(&from, &to).ok();
    }
    Ok(())
}

#[cfg(unix)]
fn symlink(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(src, dst)
}

#[cfg(not(unix))]
fn symlink(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_dir(src, dst)
}

pub fn policy_markdown(name: &str) -> String {
    let pool = pool_display();
    format!(
        "# Space: {name}

This folder is a **space** managed by the `space` tool: a working set of repositories.
Each subdirectory here is a **symlink** to a canonical checkout in `{pool}/`
(possibly grouped, e.g. `{pool}/<group>/<repo>`), or a git worktree of one once a
branch has been made.

## Rules for agents

- The source of truth for every repo is its checkout under `{pool}/`. Edits made
  through the symlinks in this space modify those canonical checkouts directly, and other
  spaces may reference the same checkout.
- **Repo-specific documentation goes inside that repo's own directory** so it can be
  committed. Cross-repo scratch (research notes, task lists, drafts) may live at the space
  root, but it is not version-controlled and only visible on this machine.
- **Before creating a git branch, add a worktree for it** (`space wt`, below) so the branch
  is isolated to this space and does not flip the shared checkout out from under other
  spaces. Never run `git checkout -b` / `git switch -c` in a symlinked repo. Each worktree
  lives at `<repo>-<branch>` next to the repo's symlink; repeat `space wt` with another
  branch for parallel worktrees of the same repo.
- Do not `rm`, move, or re-point the symlinks by hand; use the `space` commands to manage
  membership.
- Normal git workflows (status, diff, add, commit, push) work as usual inside each repo.
- `{name}.code-workspace` here is generated: it opens every repo in this space as a VS Code
  multi-root workspace (`code {name}.code-workspace`). It regenerates when membership
  changes, so do not edit it by hand.

## Commands (run from anywhere inside this space)

```sh
space pull               # update every repo in this space from origin
space pull <repo>        # update just one repo
space add <repo>         # add a pool repo to this space (bare name or us/<repo>)
space ls                 # list this space's repos and the available pool
space wt <repo> <branch> # REQUIRED before branching: adds a <repo>-<branch> worktree
```

- `space pull` is the safe way to get latest: it fetches origin, then fast-forwards only
  when the repo is on main/master with a clean tree. On a feature-branch worktree it only
  fetches and reports, never merges. It refuses (with the reason) on dirty or diverged
  repos instead of guessing.
- `space add` resolves a bare repo name against the pool (`{pool}/**`) and errors
  if the name is ambiguous or unknown; it links the repo into this space as a symlink.
- `space wt` creates the branch in an isolated worktree named `<repo>-<branch>` (the repo's
  symlink stays as the canonical view) and carries untracked files plus gitignored `*.md`
  notes across so nothing is lost. Work inside that worktree directory; commits land on the
  new branch. One branch can be checked out in only one worktree at a time.

Typical session: `space pull`, work, commit, push. If the work needs a branch, run
`space wt <repo> <branch>` first, then proceed as usual.
",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // Give each test its own throwaway "desktop" so we never touch the real one.
    struct Sandbox {
        root: PathBuf,
    }
    impl Sandbox {
        fn new(tag: &str) -> Self {
            // PID-suffixed so overlapping `cargo test` runs never share a sandbox.
            let root = std::env::temp_dir()
                .join(format!("space-space-tests-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(root.join(POOL_DIR)).unwrap();
            Sandbox { root }
        }
        fn pool(&self) -> PathBuf {
            self.root.join(POOL_DIR)
        }
        /// A minimal committed git repo with one gitignored note.
        fn make_repo(&self, name: &str) -> PathBuf {
            let p = self.pool().join(name);
            std::fs::create_dir_all(&p).unwrap();
            let run = |args: &[&str]| {
                let out = Command::new("git")
                    .current_dir(&p)
                    .args(args)
                    .output()
                    .unwrap();
                assert!(
                    out.status.success(),
                    "git {:?} failed in {}: {}",
                    args,
                    p.display(),
                    String::from_utf8_lossy(&out.stderr)
                );
            };
            run(&["init", "-q", "-b", "main"]);
            run(&["config", "user.email", "t@t"]);
            run(&["config", "user.name", "t"]);
            // Parallel test commits overwhelm gpg-agent when the user's global
            // config signs commits; sandbox repos never sign.
            run(&["config", "commit.gpgsign", "false"]);
            std::fs::write(p.join("README.md"), "hi").unwrap();
            std::fs::write(p.join(".gitignore"), "NOTES.md\n").unwrap();
            std::fs::write(p.join("NOTES.md"), "local ignored note").unwrap();
            run(&["add", "-A"]);
            run(&["commit", "-qm", "init"]);
            p
        }
    }
    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn add_symlink_and_read_membership() {
        let sb = Sandbox::new("add");
        sb.make_repo("api");
        let space = sb.root.join("proj");
        std::fs::create_dir(&space).unwrap();
        write_space_files(&space, "proj").unwrap();

        add_repo(&space, "api").unwrap();
        let read = read_space(&space);
        assert_eq!(read.repos.len(), 1);
        assert_eq!(read.repos[0].name, "api");
        assert_eq!(read.repos[0].state, RepoState::Symlink);

        // Policy files are written and excluded from membership.
        assert!(space.join("CLAUDE.md").is_file());
        assert!(space.join("AGENTS.md").is_file());
        assert!(is_space(&space));

        // Adding twice is refused, not silently duplicated.
        assert!(add_repo(&space, "api").is_err());
    }

    #[test]
    fn pull_main_paths() {
        let sb = Sandbox::new("pull");
        let upstream = sb.make_repo("upstream");
        let clone = sb.pool().join("clone");
        assert!(
            Command::new("git")
                .args([
                    "clone",
                    "-q",
                    &upstream.to_string_lossy(),
                    &clone.to_string_lossy()
                ])
                .status()
                .unwrap()
                .success()
        );
        let cfg = |args: &[&str]| {
            assert!(
                Command::new("git")
                    .current_dir(&clone)
                    .args(args)
                    .status()
                    .unwrap()
                    .success()
            );
        };
        cfg(&["config", "user.email", "t@t"]);
        cfg(&["config", "user.name", "t"]);
        cfg(&["config", "commit.gpgsign", "false"]);

        assert_eq!(pull_main(&clone).unwrap(), "already up to date");

        // Upstream moves forward: clean clone fast-forwards.
        std::fs::write(upstream.join("new.txt"), "x").unwrap();
        let up = |args: &[&str]| {
            assert!(
                Command::new("git")
                    .current_dir(&upstream)
                    .args(args)
                    .status()
                    .unwrap()
                    .success()
            );
        };
        up(&["add", "-A"]);
        up(&["commit", "-qm", "more"]);
        assert_eq!(pull_main(&clone).unwrap(), "updated 1 commit");
        assert!(clone.join("new.txt").is_file());

        // Behind again but dirty: refused with a clear message.
        std::fs::write(upstream.join("new2.txt"), "y").unwrap();
        up(&["add", "-A"]);
        up(&["commit", "-qm", "even more"]);
        std::fs::write(clone.join("README.md"), "local edit").unwrap();
        let err = pull_main(&clone).unwrap_err().to_string();
        assert!(err.contains("uncommitted changes"), "got: {err}");

        // On a feature branch: fetch only, never merge.
        cfg(&["checkout", "-q", "-b", "feature/pull"]);
        let msg = pull_main(&clone).unwrap();
        assert!(msg.contains("not merged"), "got: {msg}");
    }

    #[test]
    fn worktrees_are_branch_named_and_parallel() {
        let sb = Sandbox::new("wt");
        sb.make_repo("gw");
        let space = sb.root.join("infra");
        std::fs::create_dir(&space).unwrap();
        write_space_files(&space, "infra").unwrap();
        add_repo(&space, "gw").unwrap();

        assert_eq!(worktree_dir_name("gw", "feature/x"), "gw-feature-x");
        let name = promote_to_worktree(&space, "gw", "feature/x").unwrap();
        assert_eq!(name, "gw-feature-x");

        // The symlink stays as the canonical view; the worktree sits beside it.
        let read = read_space(&space);
        let get = |n: &str| read.repos.iter().find(|r| r.name == n).unwrap();
        assert_eq!(get("gw").state, RepoState::Symlink);
        assert!(matches!(&get("gw-feature-x").state,
            RepoState::Worktree { branch } if branch == "feature/x"));
        // Loose notes followed into the worktree; tracked files present.
        assert!(space.join("gw-feature-x").join("NOTES.md").is_file());
        assert!(space.join("gw-feature-x").join("README.md").is_file());

        // A second branch = a second parallel worktree of the same repo.
        promote_to_worktree(&space, "gw", "feature/y").unwrap();
        assert_eq!(read_space(&space).repos.len(), 3);
        // Same branch twice is refused by the name collision.
        assert!(promote_to_worktree(&space, "gw", "feature/x").is_err());

        // Closing a worktree keeps the symlink (already present) and removes the dir.
        revert_to_symlink(&space, "gw-feature-x").unwrap();
        let read = read_space(&space);
        assert_eq!(read.repos.len(), 2);
        assert_eq!(
            read.repos
                .iter()
                .filter(|r| r.state == RepoState::Symlink)
                .count(),
            1
        );
    }

    #[test]
    fn remove_symlink() {
        let sb = Sandbox::new("rm");
        sb.make_repo("r");
        let space = sb.root.join("s");
        std::fs::create_dir(&space).unwrap();
        write_space_files(&space, "s").unwrap();
        add_repo(&space, "r").unwrap();
        remove_repo(&space, "r").unwrap();
        assert!(read_space(&space).repos.is_empty());
    }

    #[test]
    fn grouped_pool_add_and_promote() {
        let sb = Sandbox::new("grouped");
        sb.make_repo("acme/api");
        sb.make_repo("oss/sdk");
        sb.make_repo("flat-repo");

        // collect_pool sees groups and flat repos side by side.
        let mut pool: Vec<PoolRepo> = Vec::new();
        collect_pool(&sb.pool(), "", &mut pool);
        let mut names: Vec<&str> = pool.iter().map(|p| p.name.as_str()).collect();
        names.sort();
        assert_eq!(names, ["acme/api", "flat-repo", "oss/sdk"]);

        let space = sb.root.join("proj");
        std::fs::create_dir(&space).unwrap();
        write_space_files(&space, "proj").unwrap();

        // Add by pool-relative name; the space links by basename.
        add_repo(&space, "acme/api").unwrap();
        let read = read_space(&space);
        assert_eq!(read.repos[0].name, "api");
        assert_eq!(read.repos[0].state, RepoState::Symlink);

        // Promotion resolves the canonical repo through the symlink.
        promote_to_worktree(&space, "api", "feature/z").unwrap();
        let read = read_space(&space);
        assert!(matches!(
            &read.repos.iter().find(|r| r.name == "api-feature-z").unwrap().state,
            RepoState::Worktree { branch } if branch == "feature/z"
        ));
        assert!(space.join("api-feature-z").join("NOTES.md").is_file());
    }

    #[test]
    fn delete_space_spares_canonical_and_refuses_user_files() {
        let sb = Sandbox::new("del");
        sb.make_repo("r");
        let space = sb.root.join("s");
        std::fs::create_dir(&space).unwrap();
        write_space_files(&space, "s").unwrap();
        add_repo(&space, "r").unwrap();
        promote_to_worktree(&space, "r", "feature/y").unwrap();
        assert_eq!(read_space(&space).repos.len(), 2); // symlink + worktree

        // Scratch notes are sanctioned: not a member, don't show in pull/repos.
        std::fs::write(space.join("NOTES-ROOT.md"), "cross-repo scratch").unwrap();
        assert_eq!(read_space(&space).repos.len(), 2);

        // But they block deletion, up front, leaving the space fully intact.
        let err = delete_space(&space).unwrap_err().to_string();
        assert!(err.contains("NOTES-ROOT.md"), "got: {err}");
        assert!(is_space(&space));
        assert!(space.join("NOTES-ROOT.md").is_file());
        assert!(
            space.join("r-feature-y").is_dir(),
            "worktree must survive refused delete"
        );

        std::fs::remove_file(space.join("NOTES-ROOT.md")).unwrap();
        delete_space(&space).unwrap();
        assert!(!space.exists());
        // Canonical checkout survives untouched.
        assert!(sb.pool().join("r").join("README.md").is_file());
    }

    #[test]
    fn worktree_removal_never_discards_work() {
        let sb = Sandbox::new("safe-rm");
        sb.make_repo("r");
        let space = sb.root.join("s");
        std::fs::create_dir(&space).unwrap();
        write_space_files(&space, "s").unwrap();
        add_repo(&space, "r").unwrap();
        let wt = promote_to_worktree(&space, "r", "feature/keep").unwrap();

        // Dirty tracked file: removal refuses instead of forcing.
        std::fs::write(space.join(&wt).join("README.md"), "edited").unwrap();
        let err = remove_repo(&space, &wt).unwrap_err().to_string();
        assert!(err.contains("uncommitted change"), "got: {err}");
        assert!(space.join(&wt).join("README.md").is_file());

        // Restore the tracked file; a brand-new untracked note must survive
        // removal by being carried back to the canonical repo.
        std::fs::write(space.join(&wt).join("README.md"), "hi").unwrap();
        std::fs::write(space.join(&wt).join("SCRATCH-NEW.md"), "new note").unwrap();
        remove_repo(&space, &wt).unwrap();
        // The repo's symlink remains the sole member.
        assert_eq!(read_space(&space).repos.len(), 1);
        assert_eq!(
            std::fs::read_to_string(sb.pool().join("r").join("SCRATCH-NEW.md")).unwrap(),
            "new note"
        );
    }

    #[test]
    fn refresh_is_idempotent_no_mtime_churn() {
        let sb = Sandbox::new("churn");
        let space = sb.root.join("s");
        std::fs::create_dir(&space).unwrap();
        write_space_files(&space, "s").unwrap();
        refresh_policy_files(&space).unwrap();
        let ws = space.join("s.code-workspace");
        let before = std::fs::metadata(&ws).unwrap().modified().unwrap();
        // Same content: repeated refresh must not rewrite (VS Code watches this).
        refresh_policy_files(&space).unwrap();
        refresh_policy_files(&space).unwrap();
        let after = std::fs::metadata(&ws).unwrap().modified().unwrap();
        assert_eq!(before, after, "unchanged refresh must not touch the file");
        // No stray tmp files left behind.
        assert!(
            !std::fs::read_dir(&space)
                .unwrap()
                .flatten()
                .any(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
        );
    }

    #[test]
    fn policy_documents_agent_commands() {
        let md = policy_markdown("proj");
        assert!(md.contains("space wt <repo> <branch>"));
        assert!(md.contains("space pull"));
        assert!(md.contains("space add <repo>"));
        assert!(md.contains("space ls"));
        assert!(md.contains(&pool_display()));
    }

    #[test]
    fn refresh_policy_updates_stale_files() {
        let sb = Sandbox::new("refresh");
        let space = sb.root.join("s");
        std::fs::create_dir(&space).unwrap();
        write_space_files(&space, "s").unwrap();
        std::fs::write(space.join("CLAUDE.md"), "old policy").unwrap();
        refresh_policy_files(&space).unwrap();
        let now = std::fs::read_to_string(space.join("CLAUDE.md")).unwrap();
        assert!(now.contains("space pull"));
        assert_eq!(
            now,
            std::fs::read_to_string(space.join("AGENTS.md")).unwrap()
        );
    }

    #[test]
    fn code_workspace_tracks_membership() {
        let sb = Sandbox::new("ws");
        sb.make_repo("a-repo");
        sb.make_repo("b-repo");
        let space = sb.root.join("s");
        std::fs::create_dir(&space).unwrap();
        write_space_files(&space, "s").unwrap();
        add_repo(&space, "a-repo").unwrap();
        add_repo(&space, "b-repo").unwrap();
        refresh_policy_files(&space).unwrap();

        let ws_path = space.join("s.code-workspace");
        let ws = std::fs::read_to_string(&ws_path).unwrap();
        // Valid JSON: space root first (named), then members in order.
        let parsed: serde_json::Value = serde_json::from_str(&ws).unwrap();
        let folders: Vec<&str> = parsed["folders"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["path"].as_str().unwrap())
            .collect();
        assert_eq!(folders, [".", "a-repo", "b-repo"]);
        assert_eq!(parsed["folders"][0]["name"], "s (space)");
        // Symlinked members carry no label; worktrees are labeled with branch.
        assert!(parsed["folders"][1].get("name").is_none());
        let wt = promote_to_worktree(&space, "a-repo", "feature/ws-label").unwrap();
        refresh_policy_files(&space).unwrap();
        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&ws_path).unwrap()).unwrap();
        // Alphabetical: a-repo, a-repo-feature-ws-label, b-repo (after the root).
        assert_eq!(
            parsed["folders"][2]["name"],
            "a-repo-feature-ws-label (feature/ws-label)"
        );
        revert_to_symlink(&space, &wt).unwrap();
        refresh_policy_files(&space).unwrap();
        // Members hidden inside the root view (they're their own roots).
        assert_eq!(parsed["settings"]["files.exclude"]["a-repo"], true);

        // Not counted as a member, so it can't show as unmanaged or block deletes.
        assert!(
            read_space(&space)
                .repos
                .iter()
                .all(|r| r.name != "s.code-workspace")
        );

        // Membership change regenerates it; delete_space cleans it up.
        remove_repo(&space, "b-repo").unwrap();
        refresh_policy_files(&space).unwrap();
        let ws = std::fs::read_to_string(&ws_path).unwrap();
        assert!(!ws.contains("b-repo"));
        delete_space(&space).unwrap();
        assert!(!space.exists());
    }

    #[test]
    fn resolve_pool_repo_bare_and_ambiguous() {
        let sb = Sandbox::new("resolve");
        sb.make_repo("acme/api");
        sb.make_repo("us/go-common");
        sb.make_repo("defi/go-common");
        let space = sb.root.join("s");
        std::fs::create_dir(&space).unwrap();
        write_space_files(&space, "s").unwrap();

        assert_eq!(resolve_pool_repo(&space, "api").unwrap(), "acme/api");
        assert_eq!(resolve_pool_repo(&space, "acme/api").unwrap(), "acme/api");
        let err = resolve_pool_repo(&space, "go-common")
            .unwrap_err()
            .to_string();
        assert!(err.contains("ambiguous"), "got: {err}");
        assert!(resolve_pool_repo(&space, "nope").is_err());
    }
}
