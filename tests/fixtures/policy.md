# Space: proj

This folder is a **space** managed by the `space` tool: a working set of repositories.
Each subdirectory here is a **symlink** to a canonical checkout in `{{POOL}}/`
(possibly grouped, e.g. `{{POOL}}/<group>/<repo>`), or a git worktree of one once a
branch has been made.

## Rules for agents

- The source of truth for every repo is its checkout under `{{POOL}}/`. Edits made
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
- `proj.code-workspace` here is generated: it opens every repo in this space as a VS Code
  multi-root workspace (`code proj.code-workspace`). It regenerates when membership
  changes, so do not edit it by hand.

## Local context and PR handoff

- Keep active cross-repo evidence in `.context/<task>/` (notes, probes, snapshots, and
  checkpoints that should not be committed). Add a short README with provenance, date,
  scope, and current status. Load only the task directory relevant to the current work.
- When reusing or cleaning a space, move completed or superseded task context to
  `archive/<task>/` instead of deleting it. Worktrees may be removed once their work is
  safely committed or otherwise preserved. Do not scan archived tasks by default or reuse
  them as current truth without revalidation.
- For a PR, write `.context/<task>/PR_DESCRIPTION.md` only after the final `base...HEAD`
  diff and verification results are known. Describe the problem, concrete behavior change,
  important design or operational constraints, and exact tests or live checks. For stacked
  PRs, include the bottom-to-top stack with links and mark the current PR. Apply that exact
  file with `gh pr create/edit --body-file`, then reread the rendered PR; never resurrect an
  archived description without checking it against the current diff.

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
- `space add` resolves a bare repo name against the pool (`{{POOL}}/**`) and errors
  if the name is ambiguous or unknown; it links the repo into this space as a symlink.
- `space wt` creates the branch in an isolated worktree named `<repo>-<branch>` (the repo's
  symlink stays as the canonical view) and carries untracked files plus gitignored `*.md`
  notes across so nothing is lost. Work inside that worktree directory; commits land on the
  new branch. One branch can be checked out in only one worktree at a time.

Typical session: `space pull`, work, commit, push. If the work needs a branch, run
`space wt <repo> <branch>` first, then proceed as usual.
