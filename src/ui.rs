//! Rendering. Three panes: projects, that project's workstreams, and the
//! selected workstream's detail.

use ratatui::prelude::*;
use ratatui::widgets::*;

use crate::app::{ago, project_health, spinner, App, Pane, Sidebar};
use crate::discover::UNFILED;
use crate::model::*;

const ACCENT: Color = Color::Rgb(0x66, 0x38, 0xb6);
const DIM: Color = Color::Rgb(0x8a, 0x8a, 0x9a);

// lazygit's git icons, by its own names, so the two tools speak the same
// vocabulary. The last two are the reason to take the set wholesale rather than
// pick glyphs that merely look right: lazygit already distinguishes a worktree
// that is there from one that is missing, which is exactly the materialized /
// virtual split here.
const BRANCH_ICON: &str = "\u{f062c}";
const WORKTREE_ICON: &str = "\u{f0339}";
const MISSING_WORKTREE_ICON: &str = "\u{f033a}";
const UPSTREAM_ICON: &str = "\u{f02a2}";
const PATH_ICON: &str = "\u{f07b}";
const REVIEW_ICON: &str = "\u{f4a5}";
const DIRTY_ICON: &str = "\u{f448}";
const OP_ICON: &str = "\u{f071}";

/// The selection in a pane that does not have the cursor. A muted accent rather
/// than the accent itself: two identically-highlighted rows in two panes is two
/// claims about where the cursor is, and only one of them is true.
const ACCENT_MUTED: Color = Color::Rgb(0x3b, 0x33, 0x4f);

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
fn pr_badge(pr: &PrInfo) -> Vec<Span<'static>> {
    let (r, g, b) = pr.state.rgb();
    let c = Color::Rgb(r, g, b);

    // The caps set only a foreground: they are filled half-circles, so whatever
    // is behind the row shows through as the surround and the pill reads as one
    // shape on any background. The body sets its own background, which is what
    // survives the selection -- see `draw_workstreams` for why that works.
    vec![
        Span::styled("\u{e0b6}", Style::default().fg(c)),
        Span::styled(
            format!("{} {}", pr.state.icon(), pr.state.label()),
            Style::default().bg(c).fg(Color::White),
        ),
        Span::styled("\u{e0b4}", Style::default().fg(c)),
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

    if app.sidebar == Sidebar::Reviews {
        draw_reviews(f, app, r.projects);
        draw_review_checks(f, app, r.workstreams);
        draw_review_detail(f, app, r.detail);
    } else {
        draw_projects(f, app, r.projects);
        draw_workstreams(f, app, r.workstreams);
        draw_detail(f, app, r.detail);
    }
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
    } else if let Some(menu) = &app.copy_menu {
        // Fixed columns: number, label, value, then the note. The label is
        // truncated to its column rather than allowed to push the value right,
        // which is what made "CI (failing) test-apps-admin-frontend" run into
        // the url with nothing between them to say where one ended.
        const LABEL_W: usize = 14;
        const VALUE_W: usize = 56;

        let mut text = vec![Line::from("")];
        for (i, item) in menu.items.iter().enumerate() {
            let on = i == menu.idx;
            let mut spans = vec![
                Span::styled(
                    // Right-aligned to two columns: " 9 " and " 10 " are
                    // different widths and shifted every column after them.
                    format!(" {:>2} ", if i == 9 { 0 } else { i + 1 }),
                    if on {
                        Style::default().bg(ACCENT).fg(Color::White)
                    } else {
                        Style::default().fg(DIM)
                    },
                ),
                Span::styled(
                    format!(" {:<w$}", truncate(&item.label, LABEL_W - 1), w = LABEL_W),
                    if on {
                        Style::default().bold()
                    } else {
                        Style::default()
                    },
                ),
                // The value, not just the label: which of two branch-shaped
                // strings you meant is decided by seeing them, and the whole
                // reason this menu exists is that the label alone does not say.
                Span::styled(
                    format!("{:<w$}", truncate(&item.value, VALUE_W), w = VALUE_W),
                    Style::default().fg(DIM),
                ),
            ];
            if let Some(note) = &item.note {
                // Parenthesised and dim, in its own column: an annotation about
                // the value, not part of what gets copied.
                spans.push(Span::styled(
                    format!(" ({})", truncate(note, 26)),
                    Style::default().fg(Color::DarkGray).italic(),
                ));
            }
            text.push(Line::from(spans));
        }
        text.push(Line::from(""));
        text.push(Line::from(vec![
            Span::styled("↵/digit", Style::default().fg(ACCENT)),
            Span::styled(" copy   ", Style::default().fg(DIM)),
            Span::styled("j/k", Style::default().fg(ACCENT)),
            Span::styled(" move   ", Style::default().fg(DIM)),
            Span::styled("esc", Style::default().fg(ACCENT)),
            Span::styled(" cancel", Style::default().fg(DIM)),
        ]));
        let h = text.len() as u16 + 2;
        draw_modal_left(f, " copy ", text, 108, h);
    } else if let Some(c) = &app.confirm {
        let mut text = vec![Line::from("")];
        for line in &c.body {
            let style = if line.contains("refuse") || line.starts_with("NOT") {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default().fg(DIM)
            };
            text.push(Line::from(Span::styled(line.clone(), style)));
        }
        text.push(Line::from(""));
        text.push(Line::from(vec![
            Span::styled("y", Style::default().fg(Color::Red).bold()),
            Span::styled(" delete    ", Style::default().fg(DIM)),
            Span::styled("any other key", Style::default().bold()),
            Span::styled(" cancel", Style::default().fg(DIM)),
        ]));
        let h = text.len() as u16 + 2;
        draw_modal(f, &c.title.clone(), text, 64, h);
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
            // Background only, no fg: otherwise the selected project's health
            // glyph loses its red/green and every project looks equally fine.
            // Muted when the cursor is elsewhere -- the project stays visible as
            // context for the pane on the right without competing with it.
            .highlight_style(Style::default().bg(if app.pane == Pane::Projects {
                ACCENT
            } else {
                ACCENT_MUTED
            })),
        area,
        &mut app.list_state,
    );
}

fn draw_reviews(f: &mut Frame, app: &mut App, area: Rect) {
    let title = format!("Reviews ({})", app.reviews.len());

    if app.reviews.is_empty() {
        // An empty queue is the normal state here -- reviews turn over the same
        // day -- so it needs to read as "nothing waiting", not as "broken".
        f.render_widget(
            Paragraph::new(vec![
                Line::from(""),
                Line::from(Span::styled(
                    "  nothing waiting on you",
                    Style::default().fg(DIM),
                )),
            ])
            .block(border(&title, app.pane == Pane::Projects)),
            area,
        );
        return;
    }

    let rows: Vec<ListItem> = app
        .reviews
        .iter()
        .map(|r| {
            ListItem::new(vec![
                Line::from(vec![
                    Span::styled(
                        format!("#{} ", r.number),
                        Style::default().fg(Color::Cyan),
                    ),
                    Span::styled(r.checks.state.glyph(), check_style(r.checks.state)),
                    Span::styled(
                        format!(" {}", r.reason.label()),
                        Style::default().fg(match r.reason {
                            ReviewReason::Rereview => Color::Yellow,
                            _ => DIM,
                        }),
                    ),
                ]),
                Line::from(Span::styled(
                    format!("  {}", truncate(&r.title, 24)),
                    Style::default(),
                )),
                Line::from(Span::styled(
                    format!("  {}", r.author),
                    Style::default().fg(DIM),
                )),
            ])
        })
        .collect();

    app.list_state.select(Some(app.review_idx));
    f.render_stateful_widget(
        List::new(rows)
            .block(border(&title, app.pane == Pane::Projects))
            .highlight_style(Style::default().bg(ACCENT)),
        area,
        &mut app.list_state,
    );
}

/// The selected review's checks. Which jobs are red is the first thing you want
/// to know about someone else's PR, and it is the pane the workstreams table
/// would otherwise be using.
fn draw_review_checks(f: &mut Frame, app: &mut App, area: Rect) {
    let Some(r) = app.review() else {
        f.render_widget(border("Checks", false), area);
        return;
    };
    let title = format!("Checks: #{}", r.number);

    let lines: Vec<Line> = match &r.checks.contexts {
        Some(cs) if !cs.is_empty() => cs
            .iter()
            .map(|c| {
                Line::from(vec![
                    Span::styled(
                        if c.failed { "✗ " } else { "✓ " },
                        if c.failed {
                            Style::default().fg(Color::Red)
                        } else {
                            Style::default().fg(Color::Green)
                        },
                    ),
                    Span::raw(c.name.rsplit(": ").next().unwrap_or(&c.name).to_string()),
                ])
            })
            .collect(),
        Some(_) => vec![Line::from(Span::styled(
            "no checks reported",
            Style::default().fg(DIM),
        ))],
        None => vec![Line::from(vec![
            Span::styled(spinner(), Style::default().fg(Color::Yellow)),
            Span::styled(
                format!("  loading {} checks", r.checks.total),
                Style::default().fg(DIM),
            ),
        ])],
    };

    f.render_widget(Paragraph::new(lines).block(border(&title, false)), area);
}

fn draw_review_detail(f: &mut Frame, app: &App, area: Rect) {
    let Some(r) = app.review() else {
        f.render_widget(border("Detail", false), area);
        return;
    };

    let mut lines = vec![Line::from(vec![
        Span::styled("   ", Style::default().fg(DIM)),
        Span::raw(truncate(&r.title, 84)),
    ])];
    lines.push(Line::from(vec![
        Span::styled("   ", Style::default().fg(DIM)),
        Span::styled(r.url.clone(), Style::default().fg(Color::Blue).underlined()),
    ]));
    lines.push(Line::from(vec![
        Span::styled(format!("{BRANCH_ICON}  "), Style::default().fg(DIM)),
        Span::raw(r.branch.clone()),
    ]));
    lines.push(Line::from(vec![
        Span::styled("   ", Style::default().fg(DIM)),
        Span::raw(r.author.clone()),
        Span::styled(
            format!("   {} ", r.reason.label()),
            Style::default().fg(match r.reason {
                ReviewReason::Rereview => Color::Yellow,
                _ => DIM,
            }),
        ),
    ]));
    if r.updated > 0 {
        lines.push(Line::from(vec![
            Span::styled("󰇗  ", Style::default().fg(DIM)),
            Span::styled(
                format!("head commit {}", ago(r.updated as u64)),
                Style::default().fg(DIM),
            ),
        ]));
    }
    lines.push(Line::from(vec![
        Span::styled("󰙨  ", Style::default().fg(DIM)),
        Span::styled(
            format!("{} {}", r.checks.state.glyph(), r.checks.state.label()),
            check_style(r.checks.state),
        ),
        Span::styled(
            format!("  {} checks", r.checks.total),
            Style::default().fg(DIM),
        ),
    ]));

    f.render_widget(
        Paragraph::new(lines).block(border(&format!("#{}", r.number), false)),
        area,
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
    let focused = app.pane == Pane::Workstreams;
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
                Some(pr) => Cell::from(Line::from(pr_badge(pr))),
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
                if w.merged.is_merged() {
                    s.push_str(w.merged.label());
                    s.push(' ');
                }
                if w.git.dirty > 0 {
                    s.push_str(&format!("{DIRTY_ICON}{} ", w.git.dirty));
                }
                if w.git.unpushed.is_some_and(|n| n > 0) {
                    s.push_str(&format!("⇡{} ", w.git.unpushed.unwrap()));
                }
                if w.git.upstream.is_none() {
                    s.push_str("local ");
                }
                s
            };

            let name_cell = Cell::from(Line::from(vec![
                Span::styled(
                    format!("{} ", if w.is_virtual() { MISSING_WORKTREE_ICON } else { WORKTREE_ICON }),
                    if w.is_virtual() {
                        Style::default().fg(Color::Yellow)
                    } else {
                        Style::default().fg(DIM)
                    },
                ),
                Span::styled(truncate(&w.name, 22), base),
            ]));

            let branch_cell = Cell::from(Line::from(vec![
                Span::styled(format!("{BRANCH_ICON} "), Style::default().fg(DIM)),
                Span::styled(truncate(&w.git.branch, 34), base),
            ]));

            if let Some(op) = &w.git.op {
                return Row::new(vec![
                    name_cell,
                    branch_cell,
                    pr_cell,
                    ci_cell,
                    Cell::from(Line::from(drift(w.git.ahead, w.git.behind))),
                    Cell::from(Line::from(vec![Span::styled(
                        format!("{OP_ICON} {}", op.label()),
                        Style::default().fg(Color::Yellow).bold(),
                    )])),
                ])
                .style(if is_selected && focused {
                    Style::default().bg(ACCENT)
                } else {
                    Style::default()
                });
            }

            Row::new(vec![
                name_cell,
                branch_cell,
                pr_cell,
                ci_cell,
                Cell::from(Line::from(drift(w.git.ahead, w.git.behind))),
                Cell::from(flags).style(base),
            ])
            // Selection is painted as the *row's* background rather than through
            // `row_highlight_style`. ratatui patches a highlight style over each
            // cell, so a highlight that sets fg and bg overwrites every colour a
            // cell chose for itself -- which flattened the PR pill to white on
            // purple and threw away the one signal it carries. A row background
            // is underneath instead: spans that set their own colours keep them,
            // and spans that do not inherit it.
            // No highlight at all while the cursor is on the left: a row
            // "selected" in a pane you are not in is only going to be read as
            // where you are.
            .style(if is_selected && focused {
                Style::default().bg(ACCENT)
            } else {
                Style::default()
            })
        })
        .collect();

    app.table_state.select(Some(app.workstream_idx));
    f.render_stateful_widget(
        Table::new(
            rows,
            [
                Constraint::Length(25),
                Constraint::Min(20),
                Constraint::Length(18),
                Constraint::Length(10),
                Constraint::Length(9),
                Constraint::Length(18),
            ],
        )
        .header(
            Row::new(vec!["workstream", "branch", "PR", "CI", "vs main", ""])
                .style(Style::default().fg(DIM).bold()),
        )
        .block(border(&title, app.pane == Pane::Workstreams)),
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
        Span::styled(format!("{BRANCH_ICON}  "), Style::default().fg(DIM)),
        Span::raw(w.git.branch.clone()),
        Span::styled(format!("  {}", w.git.head), Style::default().fg(DIM)),
    ]));

    lines.push(Line::from(vec![
        Span::styled(
            format!("{}  ", if w.is_virtual() { MISSING_WORKTREE_ICON } else { PATH_ICON }),
            Style::default().fg(DIM),
        ),
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
        Span::styled(format!("{UPSTREAM_ICON}  "), Style::default().fg(DIM)),
        Span::raw(upstream),
    ]));

    lines.push(Line::from(vec![
        Span::styled("󰇷  ", Style::default().fg(DIM)),
        Span::raw(format!("↑{} ahead  ↓{} behind main", w.git.ahead, w.git.behind)),
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

    if let Some(op) = &w.git.op {
        lines.push(Line::from(vec![
            Span::styled(format!("{OP_ICON}  "), Style::default().fg(Color::Yellow)),
            Span::styled(
                op.label(),
                Style::default().fg(Color::Yellow).bold(),
            ),
            Span::styled(
                "  — HEAD is detached until this finishes".to_string(),
                Style::default().fg(DIM),
            ),
        ]));
        if op.kind == OpKind::Rebase {
            lines.push(Line::from(vec![
                Span::styled("   ", Style::default().fg(DIM)),
                Span::styled(
                    "resolve and `git rebase --continue`, or `git rebase --abort`",
                    Style::default().fg(DIM),
                ),
            ]));
        }
    }

    if w.git.dirty > 0 || w.git.staged > 0 {
        lines.push(Line::from(vec![
            Span::styled(format!("{DIRTY_ICON}  "), Style::default().fg(DIM)),
            Span::raw(format!("{} modified, {} staged", w.git.dirty, w.git.staged)),
        ]));
    }

    match &w.pr {
        Some(pr) => {
            let mut head = vec![Span::styled("   ", Style::default().fg(DIM))];
            head.extend(pr_badge(pr));
            head.push(Span::raw(format!("  {}", truncate(&pr.title, 48))));
            lines.push(Line::from(head));
            // Remember which row this is so a click can hit it. +1 for the
            // block's top border.
            app.url_row = Some(area.y + 1 + lines.len() as u16);
            lines.push(Line::from(vec![
                Span::styled("   ", Style::default().fg(DIM)),
                Span::styled(pr.url.clone(), Style::default().fg(Color::Blue).underlined()),
            ]));
            if let Some(rd) = &pr.review_decision {
                lines.push(Line::from(vec![
                    Span::styled(format!("{REVIEW_ICON}  "), Style::default().fg(DIM)),
                    Span::raw(rd.replace('_', " ").to_lowercase()),
                ]));
            }
            lines.push(Line::from(vec![
                Span::styled("󰙨  ", Style::default().fg(DIM)),
                Span::styled(
                    format!("{} {}", pr.checks.state.glyph(), pr.checks.state.label()),
                    check_style(pr.checks.state),
                ),
                Span::styled(
                    format!("  {} checks", pr.checks.total),
                    Style::default().fg(DIM),
                ),
            ]));
            let failing = pr.checks.failing();
            if !failing.is_empty() {
                for c in failing.iter().take(3) {
                    lines.push(Line::from(vec![
                        Span::styled("   ", Style::default().fg(DIM)),
                        Span::styled(format!("✗ {}", c.name), Style::default().fg(Color::Red)),
                    ]));
                }
            } else if pr.checks.contexts.is_none() && pr.checks.state == CheckState::Failure {
                lines.push(Line::from(vec![
                    Span::styled("   ", Style::default().fg(DIM)),
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

    // No j/k or tab here. Movement keys are the ones you learn in the first ten
    // seconds and then never read again, and the footer is narrow enough that
    // every one of them costs a key you might actually have forgotten. They stay
    // in `?`.
    let mut spans = vec![
        Span::styled("↵", Style::default().fg(ACCENT)),
        Span::styled(" cd/create  ", Style::default().fg(DIM)),
        Span::styled("/", Style::default().fg(ACCENT)),
        Span::styled(" filter  ", Style::default().fg(DIM)),
        Span::styled("g", Style::default().fg(ACCENT)),
        Span::styled(" lazygit  ", Style::default().fg(DIM)),
        Span::styled("b", Style::default().fg(ACCENT)),
        Span::styled(" rebase  ", Style::default().fg(DIM)),
        Span::styled("p", Style::default().fg(ACCENT)),
        Span::styled(" push  ", Style::default().fg(DIM)),
        Span::styled("y", Style::default().fg(ACCENT)),
        Span::styled(" copy  ", Style::default().fg(DIM)),
        Span::styled("o", Style::default().fg(ACCENT)),
        Span::styled(" notes  ", Style::default().fg(DIM)),
        Span::styled("?", Style::default().fg(ACCENT)),
        Span::styled(" help  ", Style::default().fg(DIM)),
        Span::styled("[ ]", Style::default().fg(ACCENT)),
        Span::styled(" reviews  ", Style::default().fg(DIM)),
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

    if !app.auto {
        spans.push(Span::styled("  paused", Style::default().fg(Color::Yellow)));
    }

    if let Some(fl) = &app.filter {
        spans.push(Span::styled(
            format!("  /{fl}"),
            Style::default().fg(ACCENT),
        ));
    }

    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_modal(f: &mut Frame, title: &str, text: Vec<Line>, w: u16, h: u16) {
    draw_modal_aligned(f, title, text, w, h, Alignment::Center)
}

fn draw_modal_left(f: &mut Frame, title: &str, text: Vec<Line>, w: u16, h: u16) {
    draw_modal_aligned(f, title, text, w, h, Alignment::Left)
}

fn draw_modal_aligned(
    f: &mut Frame,
    title: &str,
    text: Vec<Line>,
    w: u16,
    h: u16,
    align: Alignment,
) {
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
            .alignment(align),
        area,
    );
}

fn draw_help(f: &mut Frame) {
    let text = vec![
        Line::from(Span::styled("proj — read-only view", Style::default().bold())),
        Line::from(""),
        Line::from("  j / k, ↓ / ↑    move within the focused pane"),
        Line::from("  tab / h / l     switch pane"),
        Line::from("  [ / ]           switch the sidebar between projects and reviews"),
        Line::from("  ↵               quit and cd to the selected workstream"),
        Line::from("  /               filter projects by name, workstream or branch"),
        Line::from("  esc             step back: workstreams → projects, or clear a filter"),
        Line::from(""),
        Line::from(Span::styled("  acting on the selected workstream", Style::default().bold())),
        Line::from("  g               lazygit, scoped to its worktree"),
        Line::from("  e               $EDITOR there"),
        Line::from("  y               copy menu: path, branch, sha, PR url, checks url…"),
        Line::from("  o               open its PR url, or copy it"),
        Line::from("  b               rebase on main, then rebuild"),
        Line::from("  p               push, to brian/<project>/<workstream>"),
        Line::from("  d               delete it, after confirming"),
        Line::from(""),
        Line::from("  r               refresh from GitHub"),
        Line::from("  R               re-scan the filesystem and git now"),
        Line::from("  a               pause or resume automatic refreshes"),
        Line::from("  q               quit (esc never quits)"),
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

    // Sized from the content rather than a guess: the list grew from 12 lines to
    // 29 while the box stayed at 18, which silently clipped a third of it.
    let h = (text.len() as u16 + 2).min(f.area().height);
    let area = centered(76, h, f.area());
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

/// Ahead/behind as lazygit writes it: ↑ahead ↓behind in yellow, and nothing at
/// all when a side is zero. "+0 −0" is three characters of noise saying a branch
/// is exactly where main is, which is the least interesting thing a row can say.
fn drift(ahead: u32, behind: u32) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    if ahead > 0 {
        out.push(Span::styled(
            format!("↑{ahead}"),
            Style::default().fg(Color::Yellow),
        ));
    }
    if behind > 0 {
        if !out.is_empty() {
            out.push(Span::raw(" "));
        }
        out.push(Span::styled(
            format!("↓{behind}"),
            Style::default().fg(Color::Yellow),
        ));
    }
    if out.is_empty() {
        out.push(Span::styled("=", Style::default().fg(DIM)));
    }
    out
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
