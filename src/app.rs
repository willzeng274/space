use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::widgets::ListState;

use crate::data::{Backend, Session, now_epoch};
use crate::space::{self, PoolRepo, RepoState, Space, SpaceRepo};

#[derive(PartialEq, Eq, Clone, Copy)]
pub enum Pane {
    Spaces,
    Repos,
    Convos,
}

pub enum InputKind {
    NewSpace,
    Branch,
}

pub enum Modal {
    None,
    Input {
        kind: InputKind,
        buffer: String,
    },
    RepoPicker {
        /// Every pool repo, available-first; `added` = already in the space.
        all: Vec<PickerItem>,
        /// `all` narrowed by `filter`; what the list shows and Enter picks from.
        candidates: Vec<PickerItem>,
        filter: String,
        state: ListState,
    },
    /// Global conversation search: filter over every session's title + text,
    /// with a live preview. Enter resumes, ^f forks.
    ConvoSearch {
        filter: String,
        /// Indices into `App::sessions`, newest first.
        matches: Vec<usize>,
        state: ListState,
        preview_scroll: u16,
    },
    ConfirmDeleteSpace {
        name: String,
    },
    /// Confirm a worktree-affecting repo action (`x` remove / `u` to-symlink).
    ConfirmRepo {
        kind: RepoConfirm,
        repo: String,
        branch: String,
    },
    /// Per-repo outcome of a space pull; any key closes.
    PullResults {
        space: String,
        results: Vec<(String, Result<String, String>)>,
    },
    /// Branch stack of one repo, with PR info overlaid as it arrives.
    Stack {
        repo: String,
        dir: PathBuf,
        stack: crate::stack::Stack,
        prs: std::collections::HashMap<String, crate::stack::PrInfo>,
        prs_loading: bool,
        state: ListState,
    },
    /// Diff of one stack layer against its parent; Esc returns to the stack.
    StackDiff {
        repo: String,
        dir: PathBuf,
        branch: String,
        parent: String,
        lines: Vec<String>,
        truncated: usize,
        scroll: u16,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RepoConfirm {
    /// `x`: remove the worktree from the space.
    Remove,
    /// `u`: convert the worktree back to a symlink.
    ToSymlink,
}

#[derive(Clone)]
pub struct PickerItem {
    /// Pool-relative name (`acme/api`).
    pub name: String,
    /// Already a member of the selected space (shown grayed, not addable).
    pub added: bool,
}

/// Something the event loop must do outside the TUI (it owns the terminal).
/// Everything except `None`/`Quit` is one-shot: the TUI tears down and execs,
/// never returning (fzf-style; quitting an agent lands in a shell at its cwd).
pub enum Effect {
    None,
    Quit,
    /// Replace the TUI with the user's shell, cd'd into `cwd`.
    Shell {
        cwd: PathBuf,
    },
    /// Start a fresh agent session with `cwd` set to the space.
    LaunchNew {
        backend: Backend,
        cwd: PathBuf,
    },
    /// Resume (or fork) an existing conversation in its own cwd.
    Resume {
        backend: Backend,
        cwd: PathBuf,
        id: String,
        fork: bool,
    },
    /// Suspend the TUI, page a layer diff through delta, then return to the
    /// stack view (the one temporary suspend; everything else is one-shot).
    Diff {
        repo: String,
        dir: PathBuf,
        parent: String,
        branch: String,
    },
}

/// PR metadata keyed by head branch, as produced by the gh fetch thread.
pub type PrFetch = Option<Vec<(String, crate::stack::PrInfo)>>;

pub enum PullMsg {
    One {
        repo: String,
        result: Result<String, String>,
    },
    Done,
}

pub struct App {
    pub pane: Pane,
    pub modal: Modal,
    pub status: Option<String>,
    pub now: i64,
    pub claude_ok: bool,
    pub codex_ok: bool,
    pub delta_ok: bool,

    pub spaces: Vec<Space>,
    pub pool: Vec<PoolRepo>,
    pub space_state: ListState,
    pub repo_state: ListState,
    /// Selection within the selected space's conversation pane.
    pub convo_state: ListState,

    /// All sessions, newest first (loaded in the background).
    pub sessions: Vec<Session>,
    pub loading: bool,

    // In-flight `p` pull: receiver + progress + accumulated results.
    pull_rx: Option<std::sync::mpsc::Receiver<PullMsg>>,
    pull_space: String,
    pull_total: usize,
    pull_results: Vec<(String, Result<String, String>)>,

    /// In-flight `gh pr list` for the stack view, tagged with its repo slug.
    pr_rx: Option<(String, std::sync::mpsc::Receiver<PrFetch>)>,
    /// PR results per slug, so stack reopens and diff round trips within the
    /// TTL reuse the network fetch instead of repeating it.
    pr_cache: std::collections::HashMap<
        String,
        (std::time::Instant, Vec<(String, crate::stack::PrInfo)>),
    >,
}

fn selected0() -> ListState {
    let mut s = ListState::default();
    s.select(Some(0));
    s
}

impl App {
    pub fn new(claude_ok: bool, codex_ok: bool, delta_ok: bool) -> Self {
        let mut app = App {
            pane: Pane::Spaces,
            modal: Modal::None,
            status: None,
            now: now_epoch(),
            claude_ok,
            codex_ok,
            delta_ok,
            spaces: Vec::new(),
            pool: Vec::new(),
            space_state: selected0(),
            repo_state: selected0(),
            convo_state: selected0(),
            sessions: Vec::new(),
            loading: true,
            pull_rx: None,
            pull_space: String::new(),
            pull_total: 0,
            pull_results: Vec::new(),
            pr_rx: None,
            pr_cache: std::collections::HashMap::new(),
        };
        app.reload_spaces();
        app
    }

    /// Re-scan spaces + pool from disk, preserving the selected space by name.
    pub fn reload_spaces(&mut self) {
        let keep = self.selected_space().map(|s| s.name.clone());
        self.spaces = space::list_spaces();
        self.pool = space::list_pool();
        // Bring generated CLAUDE.md/AGENTS.md up to date with this binary's
        // policy template (no-op when already current).
        for s in &self.spaces {
            let _ = space::refresh_policy_files(&s.path);
        }
        let idx = keep
            .and_then(|name| self.spaces.iter().position(|s| s.name == name))
            .unwrap_or(0);
        self.space_state.select(
            (!self.spaces.is_empty()).then_some(idx.min(self.spaces.len().saturating_sub(1))),
        );
        self.clamp_repo_selection();
    }

    fn clamp_repo_selection(&mut self) {
        let n = self.selected_space().map(|s| s.repos.len()).unwrap_or(0);
        self.repo_state
            .select((n > 0).then(|| self.repo_state.selected().unwrap_or(0).min(n - 1)));
    }

    pub fn selected_space(&self) -> Option<&Space> {
        self.space_state.selected().and_then(|i| self.spaces.get(i))
    }

    pub fn selected_repo(&self) -> Option<&SpaceRepo> {
        let space = self.selected_space()?;
        self.repo_state.selected().and_then(|i| space.repos.get(i))
    }

    // ---- sessions ------------------------------------------------------------

    pub fn add_session(&mut self, s: Session) {
        self.sessions.push(s);
        self.sessions
            .sort_by_key(|s| std::cmp::Reverse(s.last_activity));
        // Keep an open search consistent while sessions stream in.
        if let Modal::ConvoSearch {
            filter, matches, ..
        } = &mut self.modal
        {
            *matches = Self::search_matches(&self.sessions, filter);
        }
    }

    /// Indices (into `sessions`) of conversations whose cwd is inside `space`.
    pub fn space_session_indices(&self, space: &Space) -> Vec<usize> {
        let prefix = space.path.to_string_lossy().to_string();
        self.sessions
            .iter()
            .enumerate()
            .filter(|(_, s)| s.cwd == prefix || s.cwd.starts_with(&format!("{prefix}/")))
            .map(|(i, _)| i)
            .collect()
    }

    /// The conversation highlighted in the selected space's pane.
    pub fn selected_space_session(&self) -> Option<&Session> {
        self.selected_space_session_index()
            .map(|i| &self.sessions[i])
    }

    fn selected_space_session_index(&self) -> Option<usize> {
        let space = self.selected_space()?;
        let indices = self.space_session_indices(space);
        let pos = self.convo_state.selected()?;
        indices.get(pos).copied()
    }

    fn search_matches(sessions: &[Session], filter: &str) -> Vec<usize> {
        let needle = filter.to_lowercase();
        sessions
            .iter()
            .enumerate()
            .filter(|(_, s)| s.matches(&needle))
            .map(|(i, _)| i)
            .collect()
    }

    fn move_list(state: &mut ListState, len: usize, delta: isize) {
        if len == 0 {
            state.select(None);
            return;
        }
        let cur = state.selected().unwrap_or(0) as isize;
        // Clamp at the edges; no wrap-around.
        state.select(Some((cur + delta).clamp(0, len as isize - 1) as usize));
    }

    fn backend_ok(&self, b: Backend) -> bool {
        match b {
            Backend::Claude => self.claude_ok,
            Backend::Codex => self.codex_ok,
        }
    }

    // ---- pull ----------------------------------------------------------------

    pub fn pulling(&self) -> bool {
        self.pull_rx.is_some()
    }

    /// Status-bar text while a pull is in flight.
    pub fn pull_progress(&self) -> Option<String> {
        self.pull_rx.as_ref().map(|_| {
            format!(
                "pulling {} {}/{}",
                self.pull_space,
                self.pull_results.len(),
                self.pull_total
            )
        })
    }

    fn start_pull(&mut self) {
        if self.pulling() {
            self.status = Some("a pull is already running".into());
            return;
        }
        let Some(space) = self.selected_space() else {
            self.status = Some("no space selected".into());
            return;
        };
        if space.repos.is_empty() {
            self.status = Some("space has no repos".into());
            return;
        }
        let name = space.name.clone();
        let jobs: Vec<(String, PathBuf, bool)> = space
            .repos
            .iter()
            .map(|r| {
                (
                    r.name.clone(),
                    space.path.join(&r.name),
                    r.state == RepoState::Foreign,
                )
            })
            .collect();
        self.pull_space = name;
        self.pull_total = jobs.len();
        self.pull_results.clear();

        let (tx, rx) = std::sync::mpsc::channel::<PullMsg>();
        self.pull_rx = Some(rx);
        std::thread::spawn(move || {
            for (repo, dir, foreign) in jobs {
                let result = if foreign {
                    Err("not space-managed, skipped".to_string())
                } else {
                    space::pull_main(&dir).map_err(|e| e.to_string())
                };
                if tx.send(PullMsg::One { repo, result }).is_err() {
                    return;
                }
            }
            let _ = tx.send(PullMsg::Done);
        });
    }

    /// Drain pull progress; called every frame by the event loop.
    pub fn poll_pull(&mut self) {
        let Some(rx) = &self.pull_rx else { return };
        let mut done = false;
        while let Ok(msg) = rx.try_recv() {
            match msg {
                PullMsg::One { repo, result } => self.pull_results.push((repo, result)),
                PullMsg::Done => done = true,
            }
        }
        if done {
            self.pull_rx = None;
            self.modal = Modal::PullResults {
                space: std::mem::take(&mut self.pull_space),
                results: std::mem::take(&mut self.pull_results),
            };
        }
    }

    // ---- stack view ------------------------------------------------------------

    fn open_stack(&mut self) {
        let (Some(space), Some(repo)) = (self.selected_space(), self.selected_repo()) else {
            return;
        };
        if repo.state == RepoState::Foreign {
            self.status = Some("not a repo".into());
            return;
        }
        let dir = space.path.join(&repo.name);
        let name = repo.name.clone();
        self.open_stack_at(name, dir);
    }

    pub fn open_stack_at(&mut self, repo: String, dir: PathBuf) {
        const PR_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(60);
        match crate::stack::branch_stack(&dir) {
            Ok(stack) => {
                let mut prs = std::collections::HashMap::new();
                let mut prs_loading = false;
                if let Some(slug) = stack.slug.clone() {
                    match self.pr_cache.get(&slug) {
                        Some((at, list)) if at.elapsed() < PR_CACHE_TTL => {
                            for (head, info) in list {
                                prs.entry(head.clone()).or_insert(info.clone());
                            }
                        }
                        _ => {
                            let (tx, rx) = std::sync::mpsc::channel();
                            self.pr_rx = Some((slug.clone(), rx));
                            prs_loading = true;
                            std::thread::spawn(move || {
                                let _ = tx.send(crate::stack::fetch_prs(&slug));
                            });
                        }
                    }
                }
                self.modal = Modal::Stack {
                    repo,
                    dir,
                    stack,
                    prs,
                    prs_loading,
                    state: selected0(),
                };
            }
            Err(e) => self.status = Some(e.to_string()),
        }
    }

    /// Drain PR metadata for an open stack view; called every frame.
    pub fn poll_prs(&mut self) {
        let Some((slug, rx)) = &self.pr_rx else {
            return;
        };
        let Ok(result) = rx.try_recv() else { return };
        let slug = slug.clone();
        self.pr_rx = None;
        if let Some(list) = &result {
            self.pr_cache
                .insert(slug, (std::time::Instant::now(), list.clone()));
        }
        if let Modal::Stack {
            prs, prs_loading, ..
        } = &mut self.modal
        {
            *prs_loading = false;
            if let Some(list) = result {
                for (head, info) in list {
                    // Keep the first (most recent) PR per head branch.
                    prs.entry(head).or_insert(info);
                }
            }
        }
    }

    /// Open a URL in the default browser without leaving the TUI.
    fn open_url(&mut self, url: &str) {
        match std::process::Command::new("open").arg(url).spawn() {
            Ok(_) => self.status = Some(format!("opened {url}")),
            Err(e) => self.status = Some(format!("failed to open browser: {e}")),
        }
    }

    fn on_stack_key(&mut self, code: KeyCode) -> Effect {
        match code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.pr_rx = None;
                self.modal = Modal::None;
            }
            KeyCode::Up | KeyCode::Char('k') | KeyCode::Down | KeyCode::Char('j') => {
                if let Modal::Stack { stack, state, .. } = &mut self.modal {
                    let delta = match code {
                        KeyCode::Up | KeyCode::Char('k') => -1,
                        _ => 1,
                    };
                    Self::move_list(state, stack.entries.len(), delta);
                }
            }
            KeyCode::Enter => {
                // PR url when one exists, otherwise the GitHub compare page.
                let url = if let Modal::Stack {
                    stack, prs, state, ..
                } = &self.modal
                {
                    state
                        .selected()
                        .and_then(|i| stack.entries.get(i))
                        .map(|e| match prs.get(&e.branch) {
                            Some(pr) if !pr.url.is_empty() => Ok(pr.url.clone()),
                            _ => match &stack.slug {
                                Some(slug) => {
                                    Ok(crate::stack::compare_url(slug, &e.parent, &e.branch))
                                }
                                None => Err("origin is not a GitHub remote".to_string()),
                            },
                        })
                } else {
                    None
                };
                match url {
                    Some(Ok(u)) => self.open_url(&u),
                    Some(Err(msg)) => self.status = Some(msg),
                    None => {}
                }
            }
            KeyCode::Char('d') => {
                let picked = if let Modal::Stack {
                    repo,
                    dir,
                    stack,
                    state,
                    ..
                } = &self.modal
                {
                    state
                        .selected()
                        .and_then(|i| stack.entries.get(i))
                        .map(|e| {
                            (
                                repo.clone(),
                                dir.clone(),
                                e.branch.clone(),
                                e.parent.clone(),
                            )
                        })
                } else {
                    None
                };
                if let Some((repo, dir, branch, parent)) = picked {
                    if self.delta_ok {
                        // Real pager with the user's delta config (side-by-side,
                        // syntax highlighting); the event loop suspends the TUI.
                        return Effect::Diff {
                            repo,
                            dir,
                            parent,
                            branch,
                        };
                    }
                    match crate::stack::layer_diff(&dir, &parent, &branch) {
                        Ok((lines, truncated)) => {
                            self.modal = Modal::StackDiff {
                                repo,
                                dir,
                                branch,
                                parent,
                                lines,
                                truncated,
                                scroll: 0,
                            };
                        }
                        Err(e) => self.status = Some(e.to_string()),
                    }
                }
            }
            _ => {}
        }
        Effect::None
    }

    fn on_stack_diff_key(&mut self, code: KeyCode, mods: KeyModifiers) -> Effect {
        match code {
            KeyCode::Esc | KeyCode::Char('q') => {
                // Back to the stack view for the same repo dir.
                let (repo, dir) = if let Modal::StackDiff { repo, dir, .. } = &self.modal {
                    (repo.clone(), dir.clone())
                } else {
                    return Effect::None;
                };
                self.modal = Modal::None;
                self.open_stack_at(repo, dir);
            }
            _ => {
                if let Modal::StackDiff { scroll, .. } = &mut self.modal {
                    match code {
                        KeyCode::Up | KeyCode::Char('k') => *scroll = scroll.saturating_sub(1),
                        KeyCode::Down | KeyCode::Char('j') => *scroll = scroll.saturating_add(1),
                        KeyCode::Char('u') if mods.contains(KeyModifiers::CONTROL) => {
                            *scroll = scroll.saturating_sub(20)
                        }
                        KeyCode::Char('d') if mods.contains(KeyModifiers::CONTROL) => {
                            *scroll = scroll.saturating_add(20)
                        }
                        KeyCode::PageUp => *scroll = scroll.saturating_sub(40),
                        KeyCode::PageDown => *scroll = scroll.saturating_add(40),
                        _ => {}
                    }
                }
            }
        }
        Effect::None
    }

    // ---- key handling ----------------------------------------------------------

    pub fn on_key(&mut self, code: KeyCode, mods: KeyModifiers) -> Effect {
        self.status = None;
        if !matches!(self.modal, Modal::None) {
            return self.on_modal_key(code, mods);
        }
        match code {
            KeyCode::Char('q') | KeyCode::Esc => return Effect::Quit,
            KeyCode::Tab => {
                self.pane = match self.pane {
                    Pane::Spaces => Pane::Repos,
                    Pane::Repos => Pane::Convos,
                    Pane::Convos => Pane::Spaces,
                }
            }
            KeyCode::Char('/') => self.open_convo_search(),
            KeyCode::Char('n') => {
                self.modal = Modal::Input {
                    kind: InputKind::NewSpace,
                    buffer: String::new(),
                }
            }
            KeyCode::Char('a') => self.open_repo_picker(),
            KeyCode::Char('p') => self.start_pull(),
            KeyCode::Char('c') => return self.launch(Backend::Claude),
            KeyCode::Char('o') => return self.launch(Backend::Codex),
            KeyCode::Char('D') => {
                if let Some(s) = self.selected_space() {
                    self.modal = Modal::ConfirmDeleteSpace {
                        name: s.name.clone(),
                    };
                }
            }
            _ => {
                return match self.pane {
                    Pane::Spaces => self.on_spaces_key(code),
                    Pane::Repos => self.on_repos_key(code),
                    Pane::Convos => self.on_convos_key(code),
                };
            }
        }
        Effect::None
    }

    fn on_spaces_key(&mut self, code: KeyCode) -> Effect {
        match code {
            KeyCode::Down | KeyCode::Char('j') => {
                Self::move_list(&mut self.space_state, self.spaces.len(), 1);
                self.repo_state.select(Some(0));
                self.convo_state.select(Some(0));
                self.clamp_repo_selection();
            }
            KeyCode::Up | KeyCode::Char('k') => {
                Self::move_list(&mut self.space_state, self.spaces.len(), -1);
                self.repo_state.select(Some(0));
                self.convo_state.select(Some(0));
                self.clamp_repo_selection();
            }
            KeyCode::Right | KeyCode::Char('l') => {
                if self
                    .selected_space()
                    .map(|s| !s.repos.is_empty())
                    .unwrap_or(false)
                {
                    self.pane = Pane::Repos;
                }
            }
            KeyCode::Enter => {
                if let Some(s) = self.selected_space() {
                    return Effect::Shell {
                        cwd: s.path.clone(),
                    };
                }
            }
            _ => {}
        }
        Effect::None
    }

    fn on_repos_key(&mut self, code: KeyCode) -> Effect {
        let n = self.selected_space().map(|s| s.repos.len()).unwrap_or(0);
        match code {
            KeyCode::Down | KeyCode::Char('j') => Self::move_list(&mut self.repo_state, n, 1),
            KeyCode::Up | KeyCode::Char('k') => Self::move_list(&mut self.repo_state, n, -1),
            KeyCode::Left | KeyCode::Char('h') => self.pane = Pane::Spaces,
            KeyCode::Char('w') => {
                // Any managed member works: a new `<repo>-<branch>` worktree is
                // added beside it (parallel worktrees are fine).
                if matches!(
                    self.selected_repo().map(|r| &r.state),
                    Some(RepoState::Symlink) | Some(RepoState::Worktree { .. })
                ) {
                    self.modal = Modal::Input {
                        kind: InputKind::Branch,
                        buffer: String::new(),
                    };
                } else {
                    self.status = Some("select a repo to add a worktree for".into());
                }
            }
            KeyCode::Char('s') => self.open_stack(),
            KeyCode::Char('u') => {
                if let Some(r) = self.selected_repo() {
                    match &r.state {
                        RepoState::Worktree { branch } => {
                            self.modal = Modal::ConfirmRepo {
                                kind: RepoConfirm::ToSymlink,
                                repo: r.name.clone(),
                                branch: branch.clone(),
                            };
                        }
                        _ => self.status = Some("that repo is not a worktree".into()),
                    }
                }
            }
            KeyCode::Char('x') => {
                // Removing a symlink is trivially reversible; worktrees confirm.
                if let Some(r) = self.selected_repo() {
                    match &r.state {
                        RepoState::Worktree { branch } => {
                            self.modal = Modal::ConfirmRepo {
                                kind: RepoConfirm::Remove,
                                repo: r.name.clone(),
                                branch: branch.clone(),
                            };
                        }
                        _ => self.remove_selected_repo(),
                    }
                }
            }
            KeyCode::Enter => {
                if let (Some(space), Some(repo)) = (self.selected_space(), self.selected_repo()) {
                    return Effect::Shell {
                        cwd: space.path.join(&repo.name),
                    };
                }
            }
            _ => {}
        }
        Effect::None
    }

    fn on_convos_key(&mut self, code: KeyCode) -> Effect {
        let n = self
            .selected_space()
            .map(|s| self.space_session_indices(s).len())
            .unwrap_or(0);
        match code {
            KeyCode::Down | KeyCode::Char('j') => Self::move_list(&mut self.convo_state, n, 1),
            KeyCode::Up | KeyCode::Char('k') => Self::move_list(&mut self.convo_state, n, -1),
            KeyCode::Left | KeyCode::Char('h') => self.pane = Pane::Spaces,
            KeyCode::Enter => return self.resume_index(self.selected_space_session_index(), false),
            KeyCode::Char('f') => {
                return self.resume_index(self.selected_space_session_index(), true);
            }
            _ => {}
        }
        Effect::None
    }

    fn on_modal_key(&mut self, code: KeyCode, mods: KeyModifiers) -> Effect {
        // Stack views borrow-and-mutate self.modal; handled outside the big match.
        if matches!(self.modal, Modal::Stack { .. }) {
            return self.on_stack_key(code);
        }
        if matches!(self.modal, Modal::StackDiff { .. }) {
            return self.on_stack_diff_key(code, mods);
        }
        match &mut self.modal {
            Modal::Input { buffer, .. } => match code {
                KeyCode::Esc => self.modal = Modal::None,
                KeyCode::Backspace => {
                    buffer.pop();
                }
                KeyCode::Char(c) => buffer.push(c),
                KeyCode::Enter => self.commit_input(),
                _ => {}
            },
            Modal::RepoPicker {
                all,
                candidates,
                filter,
                state,
            } => match code {
                KeyCode::Esc => self.modal = Modal::None,
                KeyCode::Up => Self::move_list(state, candidates.len(), -1),
                KeyCode::Down => Self::move_list(state, candidates.len(), 1),
                KeyCode::Enter => self.commit_picker(),
                // Printable keys type into the filter; navigate with the arrows.
                KeyCode::Char(c) => {
                    filter.push(c);
                    Self::refilter_picker(all, candidates, filter, state);
                }
                KeyCode::Backspace => {
                    filter.pop();
                    Self::refilter_picker(all, candidates, filter, state);
                }
                _ => {}
            },
            Modal::ConvoSearch {
                filter,
                matches,
                state,
                preview_scroll,
            } => match code {
                KeyCode::Esc => self.modal = Modal::None,
                KeyCode::Up => {
                    Self::move_list(state, matches.len(), -1);
                    *preview_scroll = 0;
                }
                KeyCode::Down => {
                    Self::move_list(state, matches.len(), 1);
                    *preview_scroll = 0;
                }
                KeyCode::Char('d') if mods.contains(KeyModifiers::CONTROL) => {
                    *preview_scroll = preview_scroll.saturating_add(10);
                }
                KeyCode::Char('u') if mods.contains(KeyModifiers::CONTROL) => {
                    *preview_scroll = preview_scroll.saturating_sub(10);
                }
                KeyCode::Char('f') if mods.contains(KeyModifiers::CONTROL) => {
                    let idx = state.selected().and_then(|p| matches.get(p)).copied();
                    self.modal = Modal::None;
                    return self.resume_index(idx, true);
                }
                KeyCode::Enter => {
                    let idx = state.selected().and_then(|p| matches.get(p)).copied();
                    self.modal = Modal::None;
                    return self.resume_index(idx, false);
                }
                KeyCode::Char(c) => {
                    filter.push(c);
                    *matches = Self::search_matches(&self.sessions, filter);
                    state.select((!matches.is_empty()).then_some(0));
                    *preview_scroll = 0;
                }
                KeyCode::Backspace => {
                    filter.pop();
                    *matches = Self::search_matches(&self.sessions, filter);
                    state.select((!matches.is_empty()).then_some(0));
                    *preview_scroll = 0;
                }
                _ => {}
            },
            Modal::ConfirmDeleteSpace { .. } => match code {
                KeyCode::Char('y') => self.do_delete_space(),
                _ => self.modal = Modal::None,
            },
            Modal::ConfirmRepo { kind, .. } => match code {
                KeyCode::Char('y') => {
                    let kind = *kind;
                    self.modal = Modal::None;
                    match kind {
                        RepoConfirm::Remove => self.remove_selected_repo(),
                        RepoConfirm::ToSymlink => self.revert_selected_repo(),
                    }
                }
                _ => self.modal = Modal::None,
            },
            Modal::PullResults { .. } => self.modal = Modal::None,
            // Routed to dedicated handlers before this match; unreachable here.
            Modal::Stack { .. } | Modal::StackDiff { .. } => {}
            Modal::None => {}
        }
        Effect::None
    }

    // ---- actions -----------------------------------------------------------

    fn open_convo_search(&mut self) {
        let matches = Self::search_matches(&self.sessions, "");
        let mut state = ListState::default();
        state.select((!matches.is_empty()).then_some(0));
        self.modal = Modal::ConvoSearch {
            filter: String::new(),
            matches,
            state,
            preview_scroll: 0,
        };
    }

    /// Case-insensitive substring filter; keeps the selection on a valid row.
    /// (Same note as conversation search: `nucleo` could make this fuzzy later.)
    fn refilter_picker(
        all: &[PickerItem],
        candidates: &mut Vec<PickerItem>,
        filter: &str,
        state: &mut ListState,
    ) {
        let needle = filter.to_lowercase();
        *candidates = all
            .iter()
            .filter(|c| c.name.to_lowercase().contains(&needle))
            .cloned()
            .collect();
        state.select((!candidates.is_empty()).then(|| {
            state
                .selected()
                .unwrap_or(0)
                .min(candidates.len().saturating_sub(1))
        }));
    }

    fn open_repo_picker(&mut self) {
        let Some(space) = self.selected_space() else {
            self.status = Some("create a space first (n)".into());
            return;
        };
        if self.pool.is_empty() {
            self.status = Some(format!("no repos in {} to add", space::pool_display()));
            return;
        }
        let present: Vec<&str> = space.repos.iter().map(|r| r.name.as_str()).collect();
        // Pool names may be group-qualified (`acme/api`); the space links by basename.
        let mut all: Vec<PickerItem> = self
            .pool
            .iter()
            .map(|p| {
                let base = p.name.rsplit('/').next().unwrap_or(&p.name);
                PickerItem {
                    name: p.name.clone(),
                    added: present.contains(&base),
                }
            })
            .collect();
        // Available repos first; already-added trail grayed out (both stay
        // alphabetical, the pool list is already sorted by name).
        all.sort_by_key(|i| i.added);
        self.modal = Modal::RepoPicker {
            all: all.clone(),
            candidates: all,
            filter: String::new(),
            state: selected0(),
        };
    }

    fn commit_input(&mut self) {
        let Modal::Input { kind, buffer } = &self.modal else {
            return;
        };
        let buffer = buffer.clone();
        let result = match kind {
            InputKind::NewSpace => space::create_space(&buffer).map(|_| ()),
            InputKind::Branch => match (self.selected_space(), self.selected_repo()) {
                (Some(space), Some(repo)) => {
                    space::promote_to_worktree(&space.path.clone(), &repo.name.clone(), &buffer)
                        .map(|_| ())
                }
                _ => Ok(()),
            },
        };
        match result {
            Ok(()) => {
                self.modal = Modal::None;
                self.reload_spaces();
            }
            Err(e) => {
                self.status = Some(e.to_string());
                self.modal = Modal::None;
            }
        }
    }

    fn commit_picker(&mut self) {
        let (repo, space_path, keep_filter) = {
            let Modal::RepoPicker {
                candidates,
                state,
                filter,
                ..
            } = &self.modal
            else {
                return;
            };
            let Some(item) = state.selected().and_then(|i| candidates.get(i)).cloned() else {
                return;
            };
            if item.added {
                self.status = Some(format!("{} is already in this space", item.name));
                return;
            }
            let Some(space) = self.selected_space() else {
                return;
            };
            (item.name, space.path.clone(), filter.clone())
        };
        match space::add_repo(&space_path, &repo) {
            Ok(()) => {
                self.reload_spaces();
                // Reopen with a fresh candidate list (and the filter kept) so
                // several repos can be added in a row.
                self.open_repo_picker();
                if let Modal::RepoPicker {
                    all,
                    candidates,
                    filter,
                    state,
                } = &mut self.modal
                {
                    *filter = keep_filter;
                    Self::refilter_picker(all, candidates, filter, state);
                }
            }
            Err(e) => {
                self.status = Some(e.to_string());
                self.modal = Modal::None;
            }
        }
    }

    fn revert_selected_repo(&mut self) {
        let Some(repo) = self.selected_repo() else {
            return;
        };
        if !matches!(repo.state, RepoState::Worktree { .. }) {
            self.status = Some("that repo is not a worktree".into());
            return;
        }
        let (name, path) = (
            repo.name.clone(),
            self.selected_space().unwrap().path.clone(),
        );
        if let Err(e) = space::revert_to_symlink(&path, &name) {
            self.status = Some(e.to_string());
        }
        self.reload_spaces();
    }

    fn remove_selected_repo(&mut self) {
        let Some(repo) = self.selected_repo() else {
            return;
        };
        let (name, path) = (
            repo.name.clone(),
            self.selected_space().unwrap().path.clone(),
        );
        if let Err(e) = space::remove_repo(&path, &name) {
            self.status = Some(e.to_string());
        }
        self.reload_spaces();
        if self
            .selected_space()
            .map(|s| s.repos.is_empty())
            .unwrap_or(true)
        {
            self.pane = Pane::Spaces;
        }
    }

    fn do_delete_space(&mut self) {
        self.modal = Modal::None;
        let Some(space) = self.selected_space() else {
            return;
        };
        let path = space.path.clone();
        if let Err(e) = space::delete_space(&path) {
            self.status = Some(e.to_string());
        }
        self.pane = Pane::Spaces;
        self.reload_spaces();
    }

    fn launch(&mut self, backend: Backend) -> Effect {
        if !self.backend_ok(backend) {
            self.status = Some(format!("{} is not on PATH", backend.label()));
            return Effect::None;
        }
        let Some(space) = self.selected_space() else {
            self.status = Some("no space selected".into());
            return Effect::None;
        };
        Effect::LaunchNew {
            backend,
            cwd: space.path.clone(),
        }
    }

    fn resume_index(&mut self, idx: Option<usize>, fork: bool) -> Effect {
        let Some((backend, cwd, id)) = idx
            .and_then(|i| self.sessions.get(i))
            .map(|s| (s.backend, s.cwd.clone(), s.id.clone()))
        else {
            return Effect::None;
        };
        if !self.backend_ok(backend) {
            self.status = Some(format!("{} is not on PATH, cannot resume", backend.label()));
            return Effect::None;
        }
        Effect::Resume {
            backend,
            cwd: PathBuf::from(cwd),
            id,
            fork,
        }
    }
}
