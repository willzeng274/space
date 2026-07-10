use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, List, ListItem, Padding, Paragraph, Wrap},
};

use crate::app::{App, InputKind, Modal, Pane, RepoConfirm};
use crate::data::{Backend, Session, humanize_since};
use crate::space::RepoState;

const CLAUDE_COLOR: Color = Color::Magenta;
const CODEX_COLOR: Color = Color::Cyan;
const ACCENT: Color = Color::Yellow;

/// Selected-row style for every list: forces the foreground so dim secondary
/// text (ages, "(added)", "-> repos/") stays readable on the highlight bar.
fn highlight() -> Style {
    Style::default()
        .bg(Color::DarkGray)
        .fg(Color::White)
        .add_modifier(Modifier::BOLD)
}

pub fn draw(f: &mut Frame, app: &mut App) {
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(1)])
        .split(f.area());

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(28), Constraint::Percentage(72)])
        .split(root[0]);

    draw_space_list(f, app, cols[0]);

    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(cols[1]);

    draw_repo_list(f, app, right[0]);
    draw_convo_list(f, app, right[1]);
    draw_hint(f, app, root[1]);
    draw_modal(f, app);
}

fn pane_block(title: String, focused: bool) -> Block<'static> {
    let mut b = Block::default().borders(Borders::ALL).title(title);
    if focused {
        b = b.border_style(Style::default().fg(ACCENT));
    }
    b
}

fn draw_space_list(f: &mut Frame, app: &mut App, area: Rect) {
    let block = pane_block(
        format!(" spaces ({}) ", app.spaces.len()),
        app.pane == Pane::Spaces,
    );

    if app.spaces.is_empty() {
        let p = Paragraph::new(Text::from(vec![
            Line::from(""),
            Line::from(Span::styled(
                "  no spaces yet",
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(""),
            Line::from(vec![
                Span::raw("  press "),
                Span::styled(
                    "n",
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ),
                Span::raw(" to create one"),
            ]),
        ]))
        .block(block);
        f.render_widget(p, area);
        return;
    }

    let items: Vec<ListItem> = app
        .spaces
        .iter()
        .map(|s| {
            ListItem::new(Line::from(vec![
                Span::styled(
                    s.name.clone(),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(
                        "  {} repo{}",
                        s.repos.len(),
                        if s.repos.len() == 1 { "" } else { "s" }
                    ),
                    Style::default().fg(Color::DarkGray),
                ),
            ]))
        })
        .collect();

    let list = List::new(items).block(block).highlight_style(highlight());
    f.render_stateful_widget(list, area, &mut app.space_state);
}

fn draw_repo_list(f: &mut Frame, app: &mut App, area: Rect) {
    let title = match app.selected_space() {
        Some(s) => format!(" repos in {} ", s.name),
        None => " repos ".to_string(),
    };
    let block = pane_block(title, app.pane == Pane::Repos);

    let Some(space) = app.selected_space() else {
        f.render_widget(Paragraph::new("").block(block), area);
        return;
    };

    if space.repos.is_empty() {
        let p = Paragraph::new(Text::from(vec![
            Line::from(""),
            Line::from(Span::styled(
                "  empty space",
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(""),
            Line::from(vec![
                Span::raw("  press "),
                Span::styled(
                    "a",
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ),
                Span::raw(" to add a repo from ~/Desktop/repos"),
            ]),
        ]))
        .block(block);
        f.render_widget(p, area);
        return;
    }

    let items: Vec<ListItem> = space
        .repos
        .iter()
        .map(|r| {
            let (state_txt, color) = match &r.state {
                RepoState::Symlink => ("-> repos/".to_string(), Color::DarkGray),
                RepoState::Worktree { branch } => (format!(" {branch}"), Color::Green),
                RepoState::Foreign => ("(unmanaged)".to_string(), Color::Red),
            };
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{:<28}", r.name),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::styled(state_txt, Style::default().fg(color)),
            ]))
        })
        .collect();

    let list = List::new(items).block(block).highlight_style(highlight());
    f.render_stateful_widget(list, area, &mut app.repo_state);
}

fn draw_convo_list(f: &mut Frame, app: &mut App, area: Rect) {
    let title = match app.selected_space() {
        Some(s) => format!(" conversations in {} ", s.name),
        None => " conversations ".to_string(),
    };
    let block = pane_block(title, app.pane == Pane::Convos);
    let inner_w = block.inner(area).width as usize;

    let Some(space) = app.selected_space() else {
        f.render_widget(Paragraph::new("").block(block), area);
        return;
    };
    let indices = app.space_session_indices(space);

    if indices.is_empty() {
        let msg = if app.loading {
            "loading"
        } else {
            "none yet (c starts claude here, o codex, / searches everywhere)"
        };
        let p = Paragraph::new(Span::styled(msg, Style::default().fg(Color::DarkGray)))
            .block(block.padding(Padding::uniform(1)));
        f.render_widget(p, area);
        return;
    }

    let items: Vec<ListItem> = indices
        .iter()
        .map(|&i| session_row(&app.sessions[i], app.now, inner_w))
        .collect();

    let list = List::new(items).block(block).highlight_style(highlight());
    f.render_stateful_widget(list, area, &mut app.convo_state);
}

fn session_row(s: &Session, now: i64, width: usize) -> ListItem<'static> {
    let be_w = 7;
    let when_w = 9;
    let title_w = width.saturating_sub(be_w + when_w + 2).max(10);

    let spans = vec![
        Span::styled(
            pad(s.backend.label(), be_w),
            Style::default().fg(backend_color(s.backend)),
        ),
        Span::raw(pad(&s.title, title_w)),
        Span::styled(
            rpad(&humanize_since(s.last_activity, now), when_w),
            Style::default().fg(Color::DarkGray),
        ),
    ];
    ListItem::new(Line::from(spans))
}

/// Chat-style preview: role-colored headers, long messages clipped so one
/// giant prompt doesn't drown the pane.
const PREVIEW_MSG_MAX_LINES: usize = 14;

fn preview_lines(s: &Session) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = Vec::new();

    lines.push(Line::from(Span::styled(
        s.title.clone(),
        Style::default().add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(vec![
        Span::styled(
            format!("{}  ", s.backend.label()),
            Style::default().fg(backend_color(s.backend)),
        ),
        Span::styled(shorten_home(&s.cwd), Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("  {} messages", s.msg_count),
            Style::default().fg(Color::DarkGray),
        ),
    ]));
    lines.push(Line::from(Span::styled(
        "─".repeat(60),
        Style::default().fg(Color::DarkGray),
    )));

    for m in &s.messages {
        let (label, color) = match m.role.as_str() {
            "user" => ("you", Color::Green),
            "assistant" => ("agent", backend_color(s.backend)),
            other => (other, Color::Gray),
        };
        lines.push(Line::from(Span::styled(
            format!("● {label}"),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )));

        let body: Vec<&str> = m.text.lines().collect();
        for l in body.iter().take(PREVIEW_MSG_MAX_LINES) {
            lines.push(Line::from(format!("  {l}")));
        }
        if body.len() > PREVIEW_MSG_MAX_LINES {
            lines.push(Line::from(Span::styled(
                format!("  + {} more lines", body.len() - PREVIEW_MSG_MAX_LINES),
                Style::default().fg(Color::DarkGray),
            )));
        }
        lines.push(Line::from(""));
    }
    lines
}

// ---- modals ----------------------------------------------------------------

fn draw_modal(f: &mut Frame, app: &mut App) {
    match &mut app.modal {
        Modal::None => {}
        Modal::Input { kind, buffer } => {
            let (title, hint) = match kind {
                InputKind::NewSpace => (" new space ", "folder name under ~/Desktop"),
                InputKind::Branch => (" new branch ", "branch for the worktree"),
            };
            let area = centered(50, 5, f.area());
            f.render_widget(Clear, area);
            let text = Text::from(vec![
                Line::from(vec![
                    Span::raw(buffer.clone()),
                    Span::styled("█", Style::default().fg(ACCENT)),
                ]),
                Line::from(Span::styled(
                    format!("{hint}  Enter confirm  Esc cancel"),
                    Style::default().fg(Color::DarkGray),
                )),
            ]);
            f.render_widget(
                Paragraph::new(text).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(ACCENT))
                        .title(title)
                        .padding(Padding::horizontal(1)),
                ),
                area,
            );
        }
        Modal::RepoPicker {
            candidates,
            filter,
            state,
            ..
        } => {
            let h = (candidates.len().max(1) as u16 + 4).min(18);
            let area = centered(56, h, f.area());
            f.render_widget(Clear, area);

            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(ACCENT))
                .title(" add repo (from ~/Desktop/repos) ")
                .padding(Padding::horizontal(1));
            let inner = block.inner(area);
            f.render_widget(block, area);

            let rows = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(2), Constraint::Min(1)])
                .split(inner);

            let filter_line = Text::from(vec![
                Line::from(vec![
                    Span::styled("> ", Style::default().fg(ACCENT)),
                    Span::raw(filter.clone()),
                    Span::styled("█", Style::default().fg(ACCENT)),
                ]),
                Line::from(Span::styled(
                    "type to filter  ↑↓ move  Enter add  Esc close",
                    Style::default().fg(Color::DarkGray),
                )),
            ]);
            f.render_widget(Paragraph::new(filter_line), rows[0]);

            if candidates.is_empty() {
                f.render_widget(
                    Paragraph::new(Span::styled(
                        "no match",
                        Style::default().fg(Color::DarkGray),
                    )),
                    rows[1],
                );
            } else {
                let items: Vec<ListItem> = candidates
                    .iter()
                    .map(|c| {
                        if c.added {
                            ListItem::new(Line::from(vec![
                                Span::styled(c.name.clone(), Style::default().fg(Color::DarkGray)),
                                Span::styled(
                                    "  (added)",
                                    Style::default()
                                        .fg(Color::DarkGray)
                                        .add_modifier(Modifier::ITALIC),
                                ),
                            ]))
                        } else {
                            ListItem::new(c.name.clone())
                        }
                    })
                    .collect();
                let list = List::new(items).highlight_style(highlight());
                f.render_stateful_widget(list, rows[1], state);
            }
        }
        Modal::ConvoSearch {
            filter,
            matches,
            state,
            preview_scroll,
        } => {
            // fzf-style overlay: results left, live preview right.
            let area = centered_pct(92, 84, f.area());
            f.render_widget(Clear, area);

            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(ACCENT))
                .title(format!(" search conversations ({}) ", matches.len()));
            let inner = block.inner(area);
            f.render_widget(block, area);

            let cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
                .split(inner);

            let left = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(2),
                    Constraint::Min(1),
                    Constraint::Length(1),
                ])
                .split(cols[0]);

            let prompt = Text::from(vec![
                Line::from(vec![
                    Span::styled("> ", Style::default().fg(ACCENT)),
                    Span::raw(filter.clone()),
                    Span::styled("█", Style::default().fg(ACCENT)),
                ]),
                Line::from(""),
            ]);
            f.render_widget(
                Paragraph::new(prompt).block(Block::default().padding(Padding::horizontal(1))),
                left[0],
            );

            let inner_w = left[1].width.saturating_sub(2) as usize;
            let items: Vec<ListItem> = matches
                .iter()
                .map(|&i| {
                    let s = &app.sessions[i];
                    let space_w = 10;
                    let be_w = 7;
                    let title_w = inner_w.saturating_sub(be_w + space_w + 1).max(8);
                    ListItem::new(Line::from(vec![
                        Span::styled(
                            pad(s.backend.label(), be_w),
                            Style::default().fg(backend_color(s.backend)),
                        ),
                        Span::styled(pad(&s.space, space_w), Style::default().fg(ACCENT)),
                        Span::raw(pad(&s.title, title_w)),
                    ]))
                })
                .collect();
            let list = List::new(items)
                .block(Block::default().padding(Padding::horizontal(1)))
                .highlight_style(highlight());
            f.render_stateful_widget(list, left[1], state);

            f.render_widget(
                Paragraph::new(Span::styled(
                    " type to filter  ↑↓ move  Enter resume  ^f fork  ^u/^d scroll  Esc close",
                    Style::default().fg(Color::DarkGray),
                )),
                left[2],
            );

            let selected = state
                .selected()
                .and_then(|p| matches.get(p))
                .map(|&i| &app.sessions[i]);
            let preview_block = Block::default()
                .borders(Borders::LEFT)
                .border_style(Style::default().fg(Color::DarkGray))
                .padding(Padding::horizontal(1));
            match selected {
                Some(s) => {
                    let p = Paragraph::new(Text::from(preview_lines(s)))
                        .block(preview_block)
                        .wrap(Wrap { trim: false })
                        .scroll((*preview_scroll, 0));
                    f.render_widget(p, cols[1]);
                }
                None => {
                    let msg = if app.loading { "loading" } else { "no match" };
                    f.render_widget(
                        Paragraph::new(Span::styled(msg, Style::default().fg(Color::DarkGray)))
                            .block(preview_block),
                        cols[1],
                    );
                }
            }
        }
        Modal::Stack {
            repo,
            stack,
            prs,
            prs_loading,
            state,
            ..
        } => {
            let area = centered_pct(80, 70, f.area());
            f.render_widget(Clear, area);
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(ACCENT))
                .title(format!(
                    " stack: {repo}{} ",
                    if *prs_loading { " (loading PRs)" } else { "" }
                ));
            let inner = block.inner(area);
            f.render_widget(block, area);

            let rows = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(1),
                    Constraint::Min(1),
                    Constraint::Length(1),
                ])
                .split(inner);

            f.render_widget(
                Paragraph::new(Span::styled(
                    format!("  {}", stack.default_branch),
                    Style::default().fg(Color::DarkGray),
                )),
                rows[0],
            );

            if stack.entries.is_empty() {
                f.render_widget(
                    Paragraph::new(Span::styled(
                        format!("  no branches beyond {}", stack.default_branch),
                        Style::default().fg(Color::DarkGray),
                    )),
                    rows[1],
                );
            } else {
                let items: Vec<ListItem> = stack
                    .entries
                    .iter()
                    .map(|e| {
                        let indent = "  ".repeat(e.depth);
                        let mut spans = vec![
                            Span::raw(format!("{indent}└─ ")),
                            Span::styled(
                                e.branch.clone(),
                                Style::default().add_modifier(Modifier::BOLD),
                            ),
                            Span::styled(
                                format!(
                                    "  {} commit{}",
                                    e.commits,
                                    if e.commits == 1 { "" } else { "s" }
                                ),
                                Style::default().fg(Color::DarkGray),
                            ),
                        ];
                        match prs.get(&e.branch) {
                            Some(pr) => {
                                let color = match pr.state.as_str() {
                                    "OPEN" => Color::Green,
                                    "DRAFT" => Color::Yellow,
                                    "MERGED" => Color::Magenta,
                                    _ => Color::Red,
                                };
                                spans.push(Span::styled(
                                    format!("  PR #{} {}", pr.number, pr.state.to_lowercase()),
                                    Style::default().fg(color),
                                ));
                            }
                            None if !*prs_loading => {
                                spans.push(Span::styled(
                                    "  no PR",
                                    Style::default().fg(Color::DarkGray),
                                ));
                            }
                            None => {}
                        }
                        ListItem::new(Line::from(spans))
                    })
                    .collect();
                let list = List::new(items)
                    .block(Block::default().padding(Padding::horizontal(1)))
                    .highlight_style(highlight());
                f.render_stateful_widget(list, rows[1], state);
            }

            f.render_widget(
                Paragraph::new(Span::styled(
                    " j/k move  Enter open PR (or compare page)  d diff vs parent  Esc close",
                    Style::default().fg(Color::DarkGray),
                )),
                rows[2],
            );
        }
        Modal::StackDiff {
            branch,
            parent,
            lines,
            truncated,
            scroll,
            ..
        } => {
            let area = centered_pct(94, 88, f.area());
            f.render_widget(Clear, area);
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(ACCENT))
                .title(format!(" diff {parent}..{branch} "));
            let inner = block.inner(area);
            f.render_widget(block, area);

            let rows = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(1), Constraint::Length(1)])
                .split(inner);

            let styled: Vec<Line> = lines
                .iter()
                .map(|l| {
                    let style = if l.starts_with("+++") || l.starts_with("---") {
                        Style::default().add_modifier(Modifier::BOLD)
                    } else if l.starts_with('+') {
                        Style::default().fg(Color::Green)
                    } else if l.starts_with('-') {
                        Style::default().fg(Color::Red)
                    } else if l.starts_with("@@") {
                        Style::default().fg(Color::Cyan)
                    } else if l.starts_with("diff --git") {
                        Style::default().add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    };
                    Line::from(Span::styled(l.clone(), style))
                })
                .collect();
            let max_scroll = styled.len().saturating_sub(rows[0].height as usize) as u16;
            let p = Paragraph::new(Text::from(styled))
                .block(Block::default().padding(Padding::horizontal(1)))
                .scroll(((*scroll).min(max_scroll), 0));
            f.render_widget(p, rows[0]);

            let more = if *truncated > 0 {
                format!("  ({truncated} lines truncated)")
            } else {
                String::new()
            };
            f.render_widget(
                Paragraph::new(Span::styled(
                    format!(" j/k scroll  ^u/^d page  Esc back to stack{more}"),
                    Style::default().fg(Color::DarkGray),
                )),
                rows[1],
            );
        }
        Modal::ConfirmRepo { kind, repo, branch } => {
            let (title, question, detail) = match kind {
                RepoConfirm::Remove => (
                    " remove worktree ",
                    format!("remove `{repo}` (worktree on {branch}) from this space?"),
                    "commits stay on the branch in the canonical repo. refuses if there are uncommitted changes; untracked files are carried back.",
                ),
                RepoConfirm::ToSymlink => (
                    " back to symlink ",
                    format!("convert `{repo}` (worktree on {branch}) back to a symlink?"),
                    "commits stay on the branch. refuses if there are uncommitted changes; untracked files are carried back.",
                ),
            };
            draw_confirm(f, title, &question, detail, ACCENT);
        }
        Modal::ConfirmDeleteSpace { name } => {
            draw_confirm(
                f,
                " delete space ",
                &format!("delete space \"{name}\"?"),
                "unlinks symlinks, removes worktrees. canonical checkouts in the pool stay.",
                Color::Red,
            );
        }
        Modal::PullResults { space, results } => {
            let h = (results.len() as u16 + 4).min(20);
            let area = centered(70, h, f.area());
            f.render_widget(Clear, area);
            let mut lines: Vec<Line> = results
                .iter()
                .map(|(repo, outcome)| match outcome {
                    Ok(msg) => Line::from(vec![
                        Span::styled("✓ ", Style::default().fg(Color::Green)),
                        Span::styled(
                            format!("{repo:<24}"),
                            Style::default().add_modifier(Modifier::BOLD),
                        ),
                        Span::raw(msg.clone()),
                    ]),
                    Err(msg) => Line::from(vec![
                        Span::styled("✗ ", Style::default().fg(Color::Red)),
                        Span::styled(
                            format!("{repo:<24}"),
                            Style::default().add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(msg.clone(), Style::default().fg(Color::Red)),
                    ]),
                })
                .collect();
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "any key to close",
                Style::default().fg(Color::DarkGray),
            )));
            f.render_widget(
                Paragraph::new(Text::from(lines))
                    .wrap(Wrap { trim: false })
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(ACCENT))
                            .title(format!(" pull {space} "))
                            .padding(Padding::horizontal(1)),
                    ),
                area,
            );
        }
    }
}

/// Confirmation dialog sized to its wrapped content, so the question, the
/// consequences, and the key instruction are always all visible.
fn draw_confirm(f: &mut Frame, title: &str, question: &str, detail: &str, border: Color) {
    const W: u16 = 64;
    // Inner text width: 2 border cols + 2 padding cols.
    let inner = (W - 4) as usize;
    let wrapped = |s: &str| s.chars().count().div_ceil(inner).max(1) as u16;
    // question + detail + blank + keys line, inside the borders.
    let h = wrapped(question) + wrapped(detail) + 2 + 2;
    let area = centered(W, h, f.area());
    f.render_widget(Clear, area);
    let text = Text::from(vec![
        Line::from(question.to_string()),
        Line::from(Span::styled(
            detail.to_string(),
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled(
                "y",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" confirm  "),
            Span::styled(
                "any other key",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" cancels"),
        ]),
    ]);
    f.render_widget(
        Paragraph::new(text).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border))
                .title(title.to_string())
                .padding(Padding::horizontal(1)),
        ),
        area,
    );
}

fn centered(w: u16, h: u16, r: Rect) -> Rect {
    let w = w.min(r.width.saturating_sub(2));
    let h = h.min(r.height.saturating_sub(2));
    Rect {
        x: r.x + (r.width.saturating_sub(w)) / 2,
        y: r.y + (r.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    }
}

fn centered_pct(wp: u16, hp: u16, r: Rect) -> Rect {
    centered(r.width * wp / 100, r.height * hp / 100, r)
}

// ---- hint bar ----------------------------------------------------------------

fn draw_hint(f: &mut Frame, app: &App, area: Rect) {
    if let Some(msg) = &app.status {
        f.render_widget(
            Paragraph::new(Span::styled(
                msg.clone(),
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            )),
            area,
        );
        return;
    }

    if let Some(progress) = app.pull_progress() {
        f.render_widget(
            Paragraph::new(Span::styled(
                progress,
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            )),
            area,
        );
        return;
    }

    let mut spans = match app.pane {
        Pane::Spaces => hint_spans(&[
            ("j/k", "move"),
            ("Tab", "pane"),
            ("n", "new space"),
            ("a", "add repo"),
            ("p", "pull"),
            ("/", "search"),
            ("Enter", "shell"),
            ("c", "claude"),
            ("o", "codex"),
            ("D", "delete"),
            ("q", "quit"),
        ]),
        Pane::Repos => hint_spans(&[
            ("j/k", "move"),
            ("Tab", "pane"),
            ("a", "add"),
            ("p", "pull"),
            ("w", "worktree"),
            ("s", "stack"),
            ("u", "symlink"),
            ("x", "remove"),
            ("Enter", "shell"),
            ("c", "claude"),
            ("o", "codex"),
            ("q", "quit"),
        ]),
        Pane::Convos => hint_spans(&[
            ("j/k", "move"),
            ("Tab", "pane"),
            ("Enter", "resume"),
            ("f", "fork"),
            ("/", "search all"),
            ("q", "quit"),
        ]),
    };

    let mut missing = Vec::new();
    if !app.claude_ok {
        missing.push("claude");
    }
    if !app.codex_ok {
        missing.push("codex");
    }
    if !missing.is_empty() {
        spans.push(Span::styled(
            format!("   ({} not on PATH)", missing.join(", ")),
            Style::default().fg(Color::Red),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn hint_spans(pairs: &[(&str, &str)]) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for (k, label) in pairs {
        spans.push(Span::styled(
            k.to_string(),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw(format!(" {label}  ")));
    }
    spans
}

// ---- text helpers ------------------------------------------------------------

fn backend_color(b: Backend) -> Color {
    match b {
        Backend::Claude => CLAUDE_COLOR,
        Backend::Codex => CODEX_COLOR,
    }
}

fn shorten_home(path: &str) -> String {
    if let Some(home) = dirs::home_dir()
        && let Some(rest) = path.strip_prefix(&home.to_string_lossy().to_string())
    {
        return format!("~{rest}");
    }
    path.to_string()
}

fn pad(s: &str, width: usize) -> String {
    let count = s.chars().count();
    if count > width {
        let clipped: String = s.chars().take(width.saturating_sub(1)).collect();
        format!("{clipped}…")
    } else {
        format!("{s}{}", " ".repeat(width - count))
    }
}

fn rpad(s: &str, width: usize) -> String {
    let count = s.chars().count();
    if count >= width {
        s.chars().take(width).collect()
    } else {
        format!("{}{s}", " ".repeat(width - count))
    }
}
