use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::Receiver;
use std::time::Duration;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use crossterm::{
    event::{self, Event, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};

use space::app::{App, Effect};
use space::data::Backend;
use space::parse::{self, Load};
use space::space as spaces;
use space::ui;

type Term = Terminal<CrosstermBackend<io::Stdout>>;

#[derive(Parser)]
#[command(
    name = "space",
    version,
    about = "spaces of repos + agent conversations, in one TUI",
    after_help = "layout:\n  ~/Desktop/repos/<group>/<repo>  canonical pool (you populate)\n  ~/Desktop/<space>/              symlinks + <repo>-<branch> worktrees\n\nshell integration: add  eval \"$(space --init zsh)\"  to ~/.zshrc"
)]
struct Cli {
    /// Print the shell wrapper for SHELL (supported: zsh)
    #[arg(long, value_name = "SHELL")]
    init: Option<String>,

    /// Internal: handoff path written for the shell wrapper
    #[arg(long, global = true, hide = true, value_name = "PATH")]
    handoff_file: Option<PathBuf>,

    #[command(subcommand)]
    cmd: Option<Cmd>,
}

/// Agent-facing subcommands, documented in each space's CLAUDE.md/AGENTS.md.
/// All work from anywhere inside a space.
#[derive(Subcommand)]
enum Cmd {
    /// Add a <repo>-<branch> worktree (run before branching)
    Wt { repo: String, branch: String },
    /// Update repos from origin (ff-only on main, fetch-only on branches)
    Pull { repo: Option<String> },
    /// Link a pool repo into this space (bare name or <group>/<repo>)
    Add { repo: String },
    /// List members and the pool
    Ls,
    /// Branch/PR stack of a repo
    Stack { repo: Option<String> },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    if let Some(shell) = cli.init.as_deref() {
        return match shell {
            "zsh" => {
                print!("{}", include_str!("../shell/space.zsh"));
                Ok(())
            }
            other => bail!("unsupported shell {other:?}; supported: zsh"),
        };
    }

    match cli.cmd {
        Some(Cmd::Wt { repo, branch }) => cmd_wt(&repo, &branch),
        Some(Cmd::Pull { repo }) => cmd_pull(repo.as_deref()),
        Some(Cmd::Add { repo }) => cmd_add(&repo),
        Some(Cmd::Ls) => cmd_ls(),
        Some(Cmd::Stack { repo }) => cmd_stack(repo.as_deref()),
        None => run_tui(cli.handoff_file),
    }
}

/// The space enclosing the caller's working directory. Prefers the shell's
/// logical $PWD: getcwd() resolves symlinks, so from inside a symlinked repo
/// it would point into the pool, past the space.
fn current_space() -> Result<PathBuf> {
    let cwd = std::env::var_os("PWD")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .map_or_else(std::env::current_dir, Ok)?;
    spaces::enclosing_space(&cwd).ok_or_else(|| {
        anyhow::anyhow!(
            "not inside a space (no {} found in any parent of {})",
            spaces::MARKER,
            cwd.display()
        )
    })
}

fn cmd_wt(repo: &str, branch: &str) -> Result<()> {
    let space_dir = current_space()?;
    let name = spaces::promote_to_worktree(&space_dir, repo, branch)?;
    let _ = spaces::refresh_policy_files(&space_dir);
    println!(
        "added worktree {}/{name} on `{branch}`",
        space_dir.display()
    );
    Ok(())
}

fn cmd_pull(only: Option<&str>) -> Result<()> {
    let space_dir = current_space()?;
    let members = spaces::members(&space_dir);
    if let Some(name) = only
        && !members.iter().any(|r| r.name == name)
    {
        bail!("`{name}` is not in this space");
    }
    let mut failed = false;
    for r in &members {
        if let Some(name) = only
            && r.name != name
        {
            continue;
        }
        if r.state == spaces::RepoState::Foreign {
            println!(
                "skip {:<24} not space-managed (stray file/dir at the space root)",
                r.name
            );
            continue;
        }
        match spaces::pull_main(&space_dir.join(&r.name)) {
            Ok(msg) => println!("ok   {:<24} {msg}", r.name),
            Err(e) => {
                failed = true;
                println!("fail {:<24} {e}", r.name);
            }
        }
    }
    let _ = spaces::refresh_policy_files(&space_dir);
    if failed {
        std::process::exit(1);
    }
    Ok(())
}

fn cmd_add(repo: &str) -> Result<()> {
    let space_dir = current_space()?;
    let resolved = spaces::resolve_pool_repo(&space_dir, repo)?;
    spaces::add_repo(&space_dir, &resolved)?;
    let _ = spaces::refresh_policy_files(&space_dir);
    println!("added {resolved} to {}", space_dir.display());
    Ok(())
}

fn cmd_ls() -> Result<()> {
    let space_dir = current_space()?;
    let _ = spaces::refresh_policy_files(&space_dir);
    println!("space: {}", space_dir.display());
    for r in spaces::members(&space_dir) {
        let state = match &r.state {
            spaces::RepoState::Symlink => "-> repos/".to_string(),
            spaces::RepoState::Worktree { branch } => format!("worktree on {branch}"),
            spaces::RepoState::Foreign => "(unmanaged)".to_string(),
        };
        println!("  {:<28} {state}", r.name);
    }
    println!("pool:");
    for p in spaces::list_pool() {
        println!("  {}", p.name);
    }
    Ok(())
}

fn cmd_stack(repo: Option<&str>) -> Result<()> {
    let space_dir = current_space()?;
    let dir = match repo {
        Some(repo) => {
            let d = space_dir.join(repo);
            if !d.is_dir() {
                bail!("`{repo}` is not in this space");
            }
            d
        }
        None => PathBuf::from(std::env::var_os("PWD").unwrap_or_default()),
    };
    let stack = space::stack::branch_stack(&dir)?;
    let prs = stack
        .slug
        .as_deref()
        .and_then(space::stack::fetch_prs)
        .unwrap_or_default();
    println!("{}", stack.default_branch);
    for e in &stack.entries {
        let pr = prs
            .iter()
            .find(|(head, _)| *head == e.branch)
            .map(|(_, p)| format!("  PR #{} {}  {}", p.number, p.state.to_lowercase(), p.url))
            .unwrap_or_else(|| match &stack.slug {
                Some(slug) => format!(
                    "  no PR  {}",
                    space::stack::compare_url(slug, &e.parent, &e.branch)
                ),
                None => String::new(),
            });
        println!(
            "{}└─ {}  {} commit{}{}",
            "  ".repeat(e.depth),
            e.branch,
            e.commits,
            if e.commits == 1 { "" } else { "s" },
            pr
        );
    }
    Ok(())
}

fn run_tui(handoff: Option<PathBuf>) -> Result<()> {
    let (tx, rx) = std::sync::mpsc::channel::<Load>();
    parse::spawn_loader(tx);

    let mut terminal = setup_terminal()?;
    let mut app = App::new(which("claude"), which("codex"), which("delta"));

    let res = event_loop(&mut terminal, &mut app, &rx, handoff.as_deref());

    restore_terminal(&mut terminal)?;
    res
}

/// Report "cd here, then run this" to the wrapper and exit the TUI loop.
fn write_handoff(path: &Path, cwd: &Path, argv: &[String]) -> Result<()> {
    std::fs::write(path, space::handoff::render(cwd, argv)?)?;
    Ok(())
}

/// Argv for launching a fresh agent session (cwd handled separately).
fn launch_argv(backend: Backend) -> Vec<String> {
    vec![
        binary_name(backend).to_string(),
        skip_permissions_flag(backend).to_string(),
    ]
}

/// Argv for resuming/forking an existing conversation.
fn resume_argv(backend: Backend, id: &str, fork: bool) -> Vec<String> {
    let mut v = vec![binary_name(backend).to_string()];
    match backend {
        Backend::Claude => {
            v.push("--resume".to_string());
            v.push(id.to_string());
            if fork {
                v.push("--fork-session".to_string());
            }
        }
        Backend::Codex => {
            v.push(if fork { "fork" } else { "resume" }.to_string());
            v.push(id.to_string());
        }
    }
    v.push(skip_permissions_flag(backend).to_string());
    v
}

/// Build a runnable Command from an argv produced above (legacy, no-wrapper path).
fn command_from(argv: &[String], cwd: &Path) -> Command {
    let mut c = Command::new(&argv[0]);
    c.args(&argv[1..]);
    if !cwd.as_os_str().is_empty() {
        c.current_dir(cwd);
    }
    c
}

fn event_loop(
    terminal: &mut Term,
    app: &mut App,
    rx: &Receiver<Load>,
    handoff: Option<&Path>,
) -> Result<()> {
    loop {
        while let Ok(msg) = rx.try_recv() {
            match msg {
                Load::Session(s) => app.add_session(*s),
                Load::Done => app.loading = false,
            }
        }
        app.poll_pull();
        app.poll_prs();
        app.now = space::data::now_epoch();
        terminal.draw(|f| ui::draw(f, app))?;

        if !event::poll(Duration::from_millis(100))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }

        match app.on_key(key.code, key.modifiers) {
            Effect::None => {}
            Effect::Quit => return Ok(()),
            Effect::Shell { cwd } => {
                restore_terminal(terminal)?;
                match handoff {
                    Some(h) => return write_handoff(h, &cwd, &[]),
                    None => exec_shell(&cwd),
                }
            }
            Effect::LaunchNew { backend, cwd } => {
                let argv = launch_argv(backend);
                restore_terminal(terminal)?;
                match handoff {
                    Some(h) => return write_handoff(h, &cwd, &argv),
                    None => run_agent_then_shell(command_from(&argv, &cwd), backend, &cwd),
                }
            }
            Effect::Resume {
                backend,
                cwd,
                id,
                fork,
            } => {
                let argv = resume_argv(backend, &id, fork);
                restore_terminal(terminal)?;
                match handoff {
                    Some(h) => return write_handoff(h, &cwd, &argv),
                    None => run_agent_then_shell(command_from(&argv, &cwd), backend, &cwd),
                }
            }
            Effect::Diff {
                repo,
                dir,
                parent,
                branch,
            } => {
                restore_terminal(terminal)?;
                // Shell for the pipe; positional args dodge quoting pitfalls.
                let status = Command::new("sh")
                    .args([
                        "-c",
                        r#"git -C "$1" diff "$2..$3" | delta --paging=always"#,
                        "_",
                    ])
                    .arg(&dir)
                    .arg(&parent)
                    .arg(&branch)
                    .status();
                if let Err(e) = status {
                    eprintln!("failed to run delta: {e}");
                }
                reenter_terminal(terminal)?;
                app.open_stack_at(repo, dir);
            }
        }
    }
}

/// Re-enter the TUI after a temporary suspend (the delta pager). Rebuilds the
/// Terminal rather than calling `Terminal::clear`, which issues a cursor
/// position query that can hang or error right after another program owned
/// the terminal; fresh buffers force a full repaint with no query.
fn reenter_terminal(terminal: &mut Term) -> Result<()> {
    enable_raw_mode()?;
    execute!(
        io::stdout(),
        EnterAlternateScreen,
        crossterm::terminal::Clear(crossterm::terminal::ClearType::All)
    )?;
    *terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    Ok(())
}

/// Run the agent with the real terminal, then replace this process with the
/// user's shell cd'd into `cwd`. The TUI is gone either way (fzf-style:
/// quitting the agent lands in a shell where the work is, not back in space).
fn run_agent_then_shell(mut cmd: Command, backend: Backend, cwd: &Path) -> ! {
    match cmd.status() {
        Ok(st) if st.success() => {}
        Ok(st) => println!(
            "{} exited ({})",
            binary_name(backend),
            st.code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signal".into())
        ),
        Err(e) => println!("failed to launch {}: {e}", binary_name(backend)),
    }
    exec_shell(cwd)
}

/// Replace this process with `$SHELL` (fallback zsh) in `dir`. Never returns.
/// $PWD is set to the logical path so the shell stays on the space-side of
/// repo symlinks instead of resolving into the pool (`space wt` depends on it).
fn exec_shell(dir: &Path) -> ! {
    use std::os::unix::process::CommandExt;
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
    let target = if dir.as_os_str().is_empty() || !dir.exists() {
        PathBuf::from("/")
    } else {
        dir.to_path_buf()
    };
    let err = Command::new(&shell)
        .current_dir(&target)
        .env("PWD", &target)
        .exec();
    eprintln!("space: failed to exec {shell}: {err}");
    std::process::exit(1);
}

/// Agents launched from spaces run without permission prompts.
/// If a CLI update renames these, the handoff surfaces the CLI's own
/// unknown-flag error and waits for a keypress, so it fails loudly.
fn skip_permissions_flag(backend: Backend) -> &'static str {
    match backend {
        Backend::Claude => "--dangerously-skip-permissions",
        Backend::Codex => "--dangerously-bypass-approvals-and-sandbox",
    }
}

fn binary_name(backend: Backend) -> &'static str {
    match backend {
        Backend::Claude => "claude",
        Backend::Codex => "codex",
    }
}

fn which(bin: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&paths).any(|dir| dir.join(bin).is_file())
}

fn setup_terminal() -> Result<Term> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    Ok(Terminal::new(CrosstermBackend::new(stdout))?)
}

fn restore_terminal(terminal: &mut Term) -> Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}
