# space

fzf/lazygit style TUI for multi-repo agent work. A space is a working set of
repos. Drop Claude Code or Codex into one, browse and resume past sessions.

## Model

```
~/Desktop/repos/            pool: canonical checkouts, you clone these yourself
  acme/api/
~/Desktop/proj/             a space (marked by .space.toml)
  api                       symlink into the pool
  api-feat-x                worktree on feat/x, created by `space wt`
  CLAUDE.md / AGENTS.md     generated agent policy, read automatically
```

Repos join a space as symlinks, so edits land in the canonical checkout and
gitignored notes are already there. Branch work gets a worktree named
`<repo>-<branch>` beside the symlink. Repeat `space wt` for parallel branches.

## Install

```sh
git clone https://github.com/willzeng274/space.git && cd space
cargo build --release
rm -f ~/.local/bin/space && cp target/release/space ~/.local/bin/
echo 'eval "$(space --init zsh)"' >> ~/.zshrc
mkdir -p ~/Desktop/repos
```

Needs Rust and git. Optional: `claude`, `codex`, `gh` (PR info), `delta`
(diff paging). Missing tools degrade gracefully.

macOS: always `rm` before `cp` when replacing the binary. Overwriting in
place reuses the inode and the signature cache kills it (`Killed: 9`).

## CLI

```sh
space                      # TUI
space pull [repo]          # update from origin, ff-only on main, fetch-only on branches
space add <repo>           # link a pool repo in (bare name or us/<repo>)
space ls                   # members and pool
space wt <repo> <branch>   # add a <repo>-<branch> worktree, run before branching
space stack [repo]         # branch/PR stack with URLs
```

## Keys

Three panes: spaces, repos, conversations. `Tab` cycles, `j`/`k` move.

| Key | Action |
| --- | --- |
| `n` | new space |
| `a` | add repo (type to filter) |
| `p` | pull all repos |
| `w` | add worktree (asks for branch) |
| `s` | stack view: `Enter` opens PR or compare page, `d` diffs vs parent |
| `u` | close worktree |
| `x` | remove repo from space |
| `Enter` | shell cd'd into the selection, TUI exits |
| `c` / `o` | launch claude / codex in the space |
| `/` | search all conversations, `Enter` resumes, `^f` forks |
| `D` | delete space |
| `q` | quit |

## Shell integration

`eval "$(space --init zsh)"` in `.zshrc` makes cd and agent launches happen in
your own shell: no nested shells, `$SHLVL` stays put, ctrl-z works on agents.
The handoff is a temp file of argv lines, never `eval`, so hostile paths stay
inert. Without it, `space` execs a shell as before.

## Notes

* Transcripts under `~/.claude` and `~/.codex` are read-only, mtime untouched.
* Agents launch with permission prompts skipped.
* `space` writes only inside space folders, never the pool.
* `cargo test` covers git ops in temp sandboxes.
