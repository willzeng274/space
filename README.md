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

Current platform support is macOS with zsh. The Rust core is largely portable,
but process handoff, URL opening, shell integration, and Windows symlink behavior
have not yet been ported and verified.

The layout is fixed at compile time and defaults to `~/Desktop` with a
`repos` pool. Override when building:

```sh
SPACE_ROOT=work SPACE_POOL=checkouts cargo build --release
```

`SPACE_ROOT` is relative to `$HOME` and may contain slashes. Everything
follows it: paths, messages, help text, generated policy files.

`space adopt [path]` marks an existing direct child of `SPACE_ROOT` as a
space without moving or deleting its contents. It refuses generated-file
collisions and symlinks that do not point to Git repositories in the pool.

macOS: always `rm` before `cp` when replacing the binary. Overwriting in
place reuses the inode and the signature cache kills it (`Killed: 9`).

## CLI

```sh
space                      # TUI
space adopt [path]         # convert an existing folder under the space root
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
| `Enter` | shell/resume the selected item, TUI exits |
| `c` / `o` | launch claude / codex in the space |
| `/` | search all conversations, `Enter` resumes, `^f` forks |
| `l` / `Space` | inspect the selected conversation |
| `r` | rename the selected conversation |
| `R` | refresh spaces and repo membership from disk |
| `D` | delete space |
| `q` | quit |

Inside the conversation inspector, use `j`/`k` for one line, `Ctrl-d`/`Ctrl-u`
for half a page, `Ctrl-f`/`Ctrl-b` or `PageDown`/`PageUp` for a full page, and
`g`/`G` for the top or bottom. `r` renames, `Enter` resumes, `f` forks, and
`h`/`Esc`/`q` closes the inspector.

## Shell integration

`eval "$(space --init zsh)"` in `.zshrc` makes cd and agent launches happen in
your own shell: no nested shells, `$SHLVL` stays put, ctrl-z works on agents.
The handoff is a temp file of argv lines, never `eval`, so hostile paths stay
inert. Without it, `space` execs a shell as before.

## Notes

* Discovery leaves transcripts under `~/.claude` and `~/.codex` untouched.
  Codex discovery reads `state_5.sqlite`; renames mirror Codex's own SQLite and
  `session_index.jsonl` writes. Claude renames append the same `custom-title`
  record used by Claude's `/resume` picker.
* Agents launch with permission prompts skipped.
* Repo/worktree mutations stay inside space folders and never modify the pool.
  Concurrent clients keep independent views; layout writes take a shared lock
  and reject stale snapshots instead of acting on a changed space.
* `cargo test` covers git ops in temp sandboxes.
