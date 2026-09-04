//! Rendering. Three panes: projects, that project's workstreams, and the
//! selected workstream's detail.

use ratatui::prelude::*;
use ratatui::widgets::*;

use crate::app::{ago, project_health, spinner, App, Pane};
use crate::discover::UNFILED;
use crate::model::*;

const ACCENT: Color = Color::Rgb(0x66, 0x38, 0xb6);
const DIM: Color = Color::Rgb(0x8a, 0x8a, 0x9a);

/// lazygit's colours for the rollup states, so a check reads the same in both
/// tools: green passing, yellow pending, red failing *and* error, plain for a
/// required check that has not reported yet.
fn check_style(s: CheckState) -> Style {
    match s {
        CheckState::Success => Style::default().fg(Color::Green),
        CheckState::Pending => Style::default().fg(Color::Yellow),
        CheckState::Failure | CheckState::Error => Style::default().fg(Color::Red),
        CheckState::Expected => Style::default().fg(Color::Gray),
        CheckState::None => Style::default().fg(DIM),
    }
}

/// A PR badge drawn the way lazygit draws it: a Nerd Font glyph and the state
/// in white on GitHub's own colour for that state, capped either side by the
/// powerline half-circles U+E0B6 and U+E0B4 in the same colour as *foreground*,
/// which is what rounds the ends.
///
/// Reading as a badge rather than as text is the point -- state is the first
/// thing you want off one of these rows, and a shape carries it faster than a
/// word does.
fn pr_badge(pr: &PrInfo, selected: bool) -> Vec<Span<'static>> {
    let (r, g, b) = pr.state.rgb();
    let c = Color::Rgb(r, g, b);

    // On the selected row the table paints its own background across the whole
    // width, which lands on top of the pill's fill and flattens the one thing
    // the pill is for. So the selected row gets an outlined pill instead: the
    // caps and text in the state colour as foreground, no fill to be overridden.
    // The colour still reads; only the shape changes.
    let (body, caps) = if selected {
        (Style::default().fg(c).bold(), Style::default().fg(c))
    } else {
        (Style::default().bg(c).fg(Color::White), Style::default().fg(c))
    };

    vec![
        Span::styled("\u{e0b6}", caps),
        Span::styled(format!("{} {}", pr.state.icon(), pr.state.label()), body),
        Span::styled("\u{e0b4}", caps),
        Span::styled(format!(" #{}", pr.number), Style::default().fg(Color::Cyan)),
    ]
}

/// Where each pane sits. A pure function of the frame, shared by the renderer
/// and by mouse hit-testing so the two cannot drift apart.
pub struct Regions {
    pub projects: Rect,
    pub workstreams: Rect,
    pub detail: Rect,
    pub footer: Rect,
}

pub fn regions(area: Rect) -> Regions {
    let rows = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(area);
    let cols = Layout::horizontal([Constraint::Length(30), Constraint::Min(40)]).split(rows[0]);
    let right = Layout::vertical([Constraint::Min(6), Constraint::Length(11)]).split(cols[1]);
    Regions {
        projects: cols[0],
        workstreams: right[0],
        detail: right[1],
        footer: rows[1],
    }
}

pub fn draw(f: &mut Frame, app: &mut App) {
    let r = regions(f.area());

    draw_projects(f, app, r.projects);
    draw_workstreams(f, app, r.workstreams);
    draw_detail(f, app, r.detail);
    draw_footer(f, app, r.footer);

    // The scan is off the main thread so the window appears at once; until it
    // lands there is genuinely nothing to draw, and an empty three-pane skeleton
    // looks like a tool that found nothing rather than one still looking.
    if app.loading {
        draw_modal(
            f,
            " loading ",
            vec![
                Line::from(vec![
                    Span::styled(spinner(), Style::default().fg(ACCENT)),
                    Span::styled("  Scanning ~/projects", Style::default().bold()),
                ]),
                Line::from(""),
                Line::from(Span::styled(
                    "worktrees, branches, and what is merged",
                    Style::default().fg(DIM),
                )),
            ],
            44,
            7,
        );
    } else if app.help {
        draw_help(f);
    }
}

fn border(title: &str, focused: bool) -> Block<'_> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(if focused {
            Style::default().fg(ACCENT)
        } else {
            Style::default().fg(DIM)
        })
        .title(Span::styled(
            format!(" {title} "),
            if focused {
                Style::default().fg(ACCENT).bold()
            } else {
                Style::default().fg(DIM)
            },
        ))
}

fn draw_projects(f: &mut Frame, app: &mut App, area: Rect) {
    let visible = app.visible();
    let rows: Vec<ListItem> = visible
        .iter()
        .map(|&i| {
            let p = &app.projects[i];
            let health = project_health(p);
            let n = p.workstreams.len();
            ListItem::new(Line::from(vec![
                Span::raw(format!("{} ", p.emoji)),
                Span::styled(
                    format!("{:<18}", truncate(&p.slug, 18)),
                    if p.slug == UNFILED {
                        Style::default().fg(DIM).italic()
                    } else {
                        Style::default()
                    },
                ),
                Span::styled(format!("{n:>2} "), Style::default().fg(DIM)),
                Span::styled(health.glyph().to_string(), check_style(health)),
            ]))
        })
        .collect();

    app.list_state.select(Some(app.project_idx));
    f.render_stateful_widget(
        List::new(rows)
            .block(border("Projects", app.pane == Pane::Projects))
            .highlight_style(Style::default().bg(ACCENT).fg(Color::White)),
        area,
        &mut app.list_state,
    );
}

fn draw_workstreams(f: &mut Frame, app: &mut App, area: Rect) {
    let title = app
        .project()
        .map(|p| format!("Workstreams: {}", p.slug))
        .unwrap_or_else(|| "Workstreams".into());

    let Some(project) = app.project().cloned() else {
        f.render_widget(border(&title, app.pane == Pane::Workstreams), area);
        return;
    };

    let selected = app.workstream_idx;
    let rows: Vec<Row> = project
        .workstreams
        .iter()
        .enumerate()
        .map(|(i, w)| {
            let is_selected = i == selected;
            // A virtual row is dimmed and marked: it names a branch you cannot
            // cd to, which is a different kind of thing from the rows above it.
            let base = if w.is_virtual() {
                Style::default().fg(DIM)
            } else {
                Style::default()
            };

            let pr_cell = match &w.pr {
                Some(pr) => Cell::from(Line::from(pr_badge(pr, is_selected))),
                None => Cell::from("—").style(Style::default().fg(DIM)),
            };

            let ci_cell = match &w.pr {
                Some(pr) if pr.checks.state != CheckState::None => Cell::from(Line::from(vec![
                    Span::styled(pr.checks.state.glyph(), check_style(pr.checks.state)),
                    Span::styled(
                        format!(" {}", pr.checks.state.label()),
                        check_style(pr.checks.state),
                    ),
                ])),
                _ => Cell::from("—").style(Style::default().fg(DIM)),
            };

            let flags = {
                let mut s = String::new();
                if w.is_virtual() {
                    s.push_str("virtual ");
                }
                if w.merged.is_merged() {
                    s.push_str(w.merged.label());
                    s.push(' ');
                }
                if w.git.dirty > 0 {
                    s.push_str(&format!("✱{} ", w.git.dirty));
                }
                if w.git.unpushed.is_some_and(|n| n > 0) {
                    s.push_str(&format!("⇡{} ", w.git.unpushed.unwrap()));
                }
                if w.git.upstream.is_none() {
                    s.push_str("local ");
                }
                s
            };

            Row::new(vec![
                Cell::from(truncate(&w.name, 26)).style(base),
                Cell::from(truncate(&w.git.branch, 40)).style(base),
                pr_cell,
                ci_cell,
                Cell::from(format!("+{} −{}", w.git.ahead, w.git.behind)).style(base),
                Cell::from(flags).style(base),
            ])
        })
        .collect();

    app.table_state.select(Some(app.workstream_idx));
    f.render_stateful_widget(
        Table::new(
            rows,
            [
                Constraint::Length(26),
                Constraint::Min(20),
                Constraint::Length(18),
                Constraint::Length(10),
                Constraint::Length(9),
                Constraint::Length(22),
            ],
        )
        .header(
            Row::new(vec!["workstream", "branch", "PR", "CI", "vs main", ""])
                .style(Style::default().fg(DIM).bold()),
        )
        .block(border(&title, app.pane == Pane::Workstreams))
        .row_highlight_style(Style::default().bg(ACCENT).fg(Color::White)),
        area,
        &mut app.table_state,
    );
}

fn draw_detail(f: &mut Frame, app: &mut App, area: Rect) {
    app.url_row = None;
    let Some(w) = app.workstream().cloned() else {
        f.render_widget(border("Detail", false), area);
        return;
    };

    let mut lines: Vec<Line> = Vec::new();

    lines.push(Line::from(vec![
        Span::styled("branch  ", Style::default().fg(DIM)),
        Span::raw(w.git.branch.clone()),
        Span::styled(format!("  {}", w.git.head), Style::default().fg(DIM)),
    ]));

    lines.push(Line::from(vec![
        Span::styled("path    ", Style::default().fg(DIM)),
        match &w.path {
            Some(p) => Span::raw(p.display().to_string()),
            // Say what a virtual row is, rather than leaving a blank: this is
            // the state that used to be invisible entirely.
            None => Span::styled(
                "no worktree — ↵ creates one and builds it",
                Style::default().fg(Color::Yellow),
            ),
        },
    ]));

    let upstream = match &w.git.upstream {
        Some(u) => format!("{u}{}", match w.git.unpushed {
            Some(0) | None => String::new(),
            Some(n) => format!(" ({n} unpushed)"),
        }),
        None => "none — never pushed".to_string(),
    };
    lines.push(Line::from(vec![
        Span::styled("upstream", Style::default().fg(DIM)),
        Span::raw(format!(" {upstream}")),
    ]));

    lines.push(Line::from(vec![
        Span::styled("vs main ", Style::default().fg(DIM)),
        Span::raw(format!("+{} ahead, −{} behind", w.git.ahead, w.git.behind)),
        Span::styled(
            match w.merged {
                Merged::No => String::new(),
                Merged::Pr => "   merged (PR)".into(),
                Merged::Ancestor => "   merged (ancestor of main)".into(),
                // Spell this one out. "Squashed" is the difference between "the
                // code is in main" and "GitHub says so", and a reader who does
                // not know that will read it as a warning.
                Merged::Equivalent => "   squashed into main (same patches, rewritten history)".into(),
            },
            Style::default().fg(Color::Magenta),
        ),
    ]));

    if w.git.dirty > 0 || w.git.staged > 0 {
        lines.push(Line::from(vec![
            Span::styled("working ", Style::default().fg(DIM)),
            Span::raw(format!("{} modified, {} staged", w.git.dirty, w.git.staged)),
        ]));
    }

    match &w.pr {
        Some(pr) => {
            let mut head = vec![Span::styled("pr      ", Style::default().fg(DIM))];
            head.extend(pr_badge(pr, false));
            head.push(Span::raw(format!("  {}", truncate(&pr.title, 48))));
            lines.push(Line::from(head));
            // Remember which row this is so a click can hit it. +1 for the
            // block's top border.
            app.url_row = Some(area.y + 1 + lines.len() as u16);
            lines.push(Line::from(vec![
                Span::styled("        ", Style::default().fg(DIM)),
                Span::styled(pr.url.clone(), Style::default().fg(Color::Blue).underlined()),
            ]));
            if let Some(rd) = &pr.review_decision {
                lines.push(Line::from(vec![
                    Span::styled("review  ", Style::default().fg(DIM)),
                    Span::raw(rd.replace('_', " ").to_lowercase()),
                ]));
            }
            lines.push(Line::from(vec![
                Span::styled("ci      ", Style::default().fg(DIM)),
                Span::styled(
                    format!("{} {}", pr.checks.state.glyph(), pr.checks.state.label()),
                    check_style(pr.checks.state),
                ),
                Span::styled(
                    format!("  {} checks", pr.checks.total),
                    Style::default().fg(DIM),
                ),
            ]));
            if let Some(failing) = &pr.checks.failing {
                for name in failing.iter().take(3) {
                    lines.push(Line::from(vec![
                        Span::styled("        ", Style::default().fg(DIM)),
                        Span::styled(format!("✗ {name}"), Style::default().fg(Color::Red)),
                    ]));
                }
            } else if pr.checks.state == CheckState::Failure {
                lines.push(Line::from(vec![
                    Span::styled("        ", Style::default().fg(DIM)),
                    Span::styled(spinner(), Style::default().fg(Color::Yellow)),
                    Span::styled(" loading failing checks", Style::default().fg(DIM)),
                ]));
            }
        }
        None => lines.push(Line::from(vec![
            Span::styled("pr      ", Style::default().fg(DIM)),
            Span::styled("none", Style::default().fg(DIM)),
        ])),
    }

    f.render_widget(
        Paragraph::new(lines).block(border(&w.name, false)),
        area,
    );
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    if app.filtering {
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("/", Style::default().fg(ACCENT).bold()),
                Span::raw(app.filter.clone().unwrap_or_default()),
                Span::styled("▏", Style::default().fg(ACCENT)),
            ])),
            area,
        );
        return;
    }

    let mut spans = vec![
        Span::styled("j/k", Style::default().fg(ACCENT)),
        Span::styled(" move  ", Style::default().fg(DIM)),
        Span::styled("tab", Style::default().fg(ACCENT)),
        Span::styled(" pane  ", Style::default().fg(DIM)),
        Span::styled("↵", Style::default().fg(ACCENT)),
        Span::styled(" cd/create  ", Style::default().fg(DIM)),
        Span::styled("/", Style::default().fg(ACCENT)),
        Span::styled(" filter  ", Style::default().fg(DIM)),
        Span::styled("o", Style::default().fg(ACCENT)),
        Span::styled(" url  ", Style::default().fg(DIM)),
        Span::styled("r", Style::default().fg(ACCENT)),
        Span::styled(" refresh  ", Style::default().fg(DIM)),
        Span::styled("?", Style::default().fg(ACCENT)),
        Span::styled(" help  ", Style::default().fg(DIM)),
        Span::styled("q", Style::default().fg(ACCENT)),
        Span::styled(" quit", Style::default().fg(DIM)),
    ];

    // Cache age is always on screen. Every number to its left came from GitHub
    // at that moment and not since, and a viewer who cannot see the age has no
    // way to know whether to trust them.
    let status = if let Some((msg, _)) = &app.flash {
        Span::styled(format!("  {msg}"), Style::default().fg(Color::Green))
    } else if app.refreshing {
        Span::styled(
            format!("  {} github", spinner()),
            Style::default().fg(Color::Yellow),
        )
    } else if let Some(e) = &app.error {
        Span::styled(format!("  {}", truncate(e, 40)), Style::default().fg(Color::Red))
    } else {
        match app.fetched_at {
            Some(t) => Span::styled(format!("  github {}", ago(t)), Style::default().fg(DIM)),
            None => Span::styled("  no github data".to_string(), Style::default().fg(Color::Yellow)),
        }
    };
    spans.push(status);

    if let Some(fl) = &app.filter {
        spans.push(Span::styled(
            format!("  /{fl}"),
            Style::default().fg(ACCENT),
        ));
    }

    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_modal(f: &mut Frame, title: &str, text: Vec<Line>, w: u16, h: u16) {
    let area = centered(w, h, f.area());
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(text)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(ACCENT))
                    .title(title.to_string()),
            )
            .alignment(Alignment::Center),
        area,
    );
}

fn draw_help(f: &mut Frame) {
    let text = vec![
        Line::from(Span::styled("proj — read-only view", Style::default().bold())),
        Line::from(""),
        Line::from("  j / k, ↓ / ↑    move within the focused pane"),
        Line::from("  tab / h / l     switch pane"),
        Line::from("  ↵               quit and cd to the selected workstream"),
        Line::from("  /               filter projects by name, workstream or branch"),
        Line::from("  esc             clear the filter"),
        Line::from("  o               open the PR url, or copy it (OSC 52) if headless"),
        Line::from("  r               refresh from GitHub"),
        Line::from("  R               re-scan the filesystem and git"),
        Line::from("  q               quit"),
        Line::from(""),
        Line::from("  click           select a row; click it again to open it"),
        Line::from("  click a url     same as o — opens it, or copies it"),
        Line::from("  scroll          move within the pane under the pointer"),
        Line::from(""),
        Line::from(Span::styled(
            "  A dimmed row is virtual: a branch with no worktree.",
            Style::default().fg(DIM),
        )),
        Line::from(Span::styled(
            "  \"squashed\" means the patches are in main, though history was rewritten.",
            Style::default().fg(DIM),
        )),
        Line::from(""),
        Line::from(Span::styled("  Actions land in phase 3.", Style::default().fg(DIM))),
    ];

    let area = centered(74, 18, f.area());
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(text).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(ACCENT))
                .title(" help "),
        ),
        area,
    );
}

fn centered(w: u16, h: u16, area: Rect) -> Rect {
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    Rect {
        x,
        y,
        width: w.min(area.width),
        height: h.min(area.height),
    }
}

/// Truncate on character boundaries -- the branch names here are ASCII but the
/// project names are emoji, and slicing bytes would panic.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let keep = max.saturating_sub(1);
    format!("{}…", s.chars().take(keep).collect::<String>())
}
