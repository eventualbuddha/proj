mod app;
mod discover;
mod git;
mod link;
mod github;
mod model;
mod ui;

use anyhow::Result;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use std::io::stdout;
use std::time::Duration;

use app::{App, Pane};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("proj — project and workstream dashboard");
        println!();
        println!("  proj              the TUI");
        println!("  proj --dump       print the same state as text");
        println!("  proj --cd-file F  write the chosen workstream's path to F on exit");
        println!("  proj --select P   open focused on project P");
        println!("  proj --render     draw one frame to stdout and exit");
        println!("  proj --render --loading   draw the pre-scan frame");
        println!("  proj --render --styles N  dump the resolved fg/bg of row N");
        return Ok(());
    }

    let cd_file = args
        .iter()
        .position(|a| a == "--cd-file")
        .and_then(|i| args.get(i + 1))
        .cloned();

    if args.iter().any(|a| a == "--dump") {
        return dump(args.iter().any(|a| a == "--no-github"));
    }

    // Draw one frame to an in-memory backend and print it. Exists so the layout
    // can be checked without a terminal -- in CI, over a pipe, or by anything
    // that cannot press a key.
    if args.iter().any(|a| a == "--render") {
        let mut app = App::new()?;
        // --loading draws the pre-scan state, which is otherwise only on screen
        // for the few hundred milliseconds the background scan takes.
        if !args.iter().any(|a| a == "--loading") {
            app.scan_now()?;
            select(&mut app, &args);
        }
        let (w, h) = (140u16, 34u16);
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h))?;
        term.draw(|f| ui::draw(f, &mut app))?;
        let buf = term.backend().buffer();

        // --styles dumps the fg/bg ratatui actually resolved per cell, which is
        // the only way to see what a highlight style did to a cell that set its
        // own colours.
        if args.iter().any(|a| a == "--styles") {
            let row = args
                .iter()
                .position(|a| a == "--styles")
                .and_then(|i| args.get(i + 1))
                .and_then(|v| v.parse::<u16>().ok())
                .unwrap_or(2);
            let mut last = String::new();
            for x in 0..w {
                let c = &buf[(x, row)];
                let key = format!("{:?}/{:?}", c.fg, c.bg);
                if key != last {
                    println!("col {x:>3}  fg={:<22} bg={:?}", format!("{:?}", c.fg), c.bg);
                    last = key;
                }
            }
            return Ok(());
        }

        for y in 0..h {
            let mut line = String::new();
            for x in 0..w {
                line.push_str(buf[(x, y)].symbol());
            }
            println!("{}", line.trim_end());
        }
        return Ok(());
    }

    // Nothing blocking here: the scan is already running on a thread and the
    // window goes up immediately with a loading modal over it.
    let mut app = App::new()?;

    let mut term = setup()?;
    let result = run(&mut term, &mut app);
    restore()?;
    result?;

    if let (Some(path), Some(action)) = (cd_file, app.action) {
        std::fs::write(path, action)?;
    }
    Ok(())
}

/// Open focused on a named project, so `proj --select react-19` lands where you
/// meant rather than at the top of the list.
fn select(app: &mut App, args: &[String]) {
    let Some(slug) = args
        .iter()
        .position(|a| a == "--select")
        .and_then(|i| args.get(i + 1))
    else {
        return;
    };
    if let Some(i) = app.visible().iter().position(|&i| &app.projects[i].slug == slug) {
        app.project_idx = i;
        app.pane = Pane::Workstreams;
    }
}

fn setup() -> Result<Terminal> {
    enable_raw_mode()?;
    execute!(stdout(), EnterAlternateScreen, EnableMouseCapture)?;
    Ok(ratatui::Terminal::new(ratatui::backend::CrosstermBackend::new(
        stdout(),
    ))?)
}

type Terminal = ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>;

fn restore() -> Result<()> {
    disable_raw_mode()?;
    // Release the mouse before leaving, or the terminal keeps reporting events
    // at a program that is no longer listening and text selection stays broken.
    execute!(stdout(), DisableMouseCapture, LeaveAlternateScreen)?;
    Ok(())
}

fn run(term: &mut Terminal, app: &mut App) -> Result<()> {
    loop {
        app.drain();
        app.tick();
        app.clamp();
        if app.flash.as_ref().is_some_and(|(_, until)| github::now() > *until) {
            app.flash = None;
        }
        // Asking here rather than on selection keeps it to one place, and the
        // request is a no-op unless the row is failing and unfetched.
        app.request_contexts();

        term.draw(|f| ui::draw(f, app))?;

        // A poll rather than a blocking read: the background refresh arrives on
        // a channel, not as a terminal event, so the loop has to come round on
        // its own for the screen to ever show it.
        if !event::poll(Duration::from_millis(100))? {
            continue;
        }
        let ev = event::read()?;

        if let Event::Mouse(m) = ev {
            // The frame's own area, not the terminal's: they differ if the
            // window resized between the draw and the click.
            let area = ratatui::layout::Rect::new(0, 0, term.size()?.width, term.size()?.height);
            if handle_mouse(app, m, area) {
                break;
            }
            continue;
        }

        let Event::Key(key) = ev else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }

        if app.filtering {
            match key.code {
                KeyCode::Esc => {
                    app.filtering = false;
                    app.filter = None;
                }
                KeyCode::Enter => app.filtering = false,
                KeyCode::Backspace => {
                    if let Some(f) = &mut app.filter {
                        f.pop();
                    }
                }
                KeyCode::Char(c) => app.filter.get_or_insert_with(String::new).push(c),
                _ => {}
            }
            app.project_idx = 0;
            app.workstream_idx = 0;
            continue;
        }

        if app.help {
            app.help = false;
            continue;
        }

        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => break,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
            KeyCode::Char('j') | KeyCode::Down => app.move_down(),
            KeyCode::Char('k') | KeyCode::Up => app.move_up(),
            KeyCode::Tab | KeyCode::Char('l') | KeyCode::Right => app.pane = Pane::Workstreams,
            KeyCode::BackTab | KeyCode::Char('h') | KeyCode::Left => app.pane = Pane::Projects,
            KeyCode::Char('/') => {
                app.filtering = true;
                app.filter = Some(String::new());
            }
            KeyCode::Char('?') => app.help = true,
            KeyCode::Char('o') => open_url(app),
            KeyCode::Char('r') => app.start_refresh(),
            KeyCode::Char('R') => app.start_scan(false),
            KeyCode::Char('a') => {
                app.auto = !app.auto;
                let msg = if app.auto {
                    "auto-refresh on"
                } else {
                    "auto-refresh off"
                };
                app.flash(msg);
            }
            // A materialized row is a cd. A virtual one has nowhere to go yet,
            // so it becomes a request to create it -- handed to the shell, which
            // can show the build and be interrupted.
            KeyCode::Enter => {
                if activate(app) {
                    break;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Route a mouse event to whichever pane it landed in. Returns true to quit,
/// which happens when a click activates a row.
///
/// Hit-testing goes through the same `ui::regions` the renderer uses, so the two
/// cannot drift apart, and through the widgets' own `offset()`, so it stays
/// right once a list is long enough to scroll.
fn handle_mouse(app: &mut App, m: MouseEvent, area: ratatui::layout::Rect) -> bool {
    let r = ui::regions(area);
    let (x, y) = (m.column, m.row);
    let inside = |rect: ratatui::layout::Rect| {
        x >= rect.x && x < rect.x + rect.width && y >= rect.y && y < rect.y + rect.height
    };

    match m.kind {
        // Scroll acts on the pane under the pointer without focusing it: looking
        // is not the same as choosing.
        MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
            let target = if inside(r.projects) {
                Pane::Projects
            } else if inside(r.workstreams) {
                Pane::Workstreams
            } else {
                return false;
            };
            let saved = app.pane;
            app.pane = target;
            if m.kind == MouseEventKind::ScrollDown {
                app.move_down();
            } else {
                app.move_up();
            }
            app.pane = saved;
            false
        }

        MouseEventKind::Down(MouseButton::Left) => {
            if inside(r.projects) {
                // +1 for the block's top border; the list's own offset covers
                // whatever has scrolled off the top.
                let Some(row) = y.checked_sub(r.projects.y + 1) else {
                    return false;
                };
                let idx = app.list_state.offset() + row as usize;
                if idx < app.visible().len() {
                    app.pane = Pane::Projects;
                    if app.project_idx != idx {
                        app.project_idx = idx;
                        app.workstream_idx = 0;
                    }
                }
                false
            } else if inside(r.workstreams) {
                // +2 here: the border and then the header row.
                let Some(row) = y.checked_sub(r.workstreams.y + 2) else {
                    app.pane = Pane::Workstreams;
                    return false;
                };
                let idx = app.table_state.offset() + row as usize;
                let n = app.project().map_or(0, |p| p.workstreams.len());
                if idx >= n {
                    app.pane = Pane::Workstreams;
                    return false;
                }
                // Click to select, click again to open. There is no
                // double-click event to rely on, and this way the first click on
                // a row you have not selected can never trigger a build.
                let reselect = app.pane == Pane::Workstreams && app.workstream_idx == idx;
                app.pane = Pane::Workstreams;
                app.workstream_idx = idx;
                if reselect {
                    return activate(app);
                }
                false
            } else if inside(r.detail) {
                // The url is the one clickable thing in this pane, and it is
                // drawn underlined and blue, so the affordance is already there.
                if Some(y) == app.url_row {
                    open_url(app);
                }
                false
            } else {
                false
            }
        }
        _ => false,
    }
}

/// Open the selected PR's url, or copy it. Reports which in the footer, because
/// "copied" and "opened" look identical from the outside and only one of them
/// means a browser is about to appear.
fn open_url(app: &mut App) {
    let Some(url) = app.selected_url() else {
        app.flash("no PR on this row");
        return;
    };
    match link::open_or_copy(&url) {
        link::Outcome::Opened => app.flash("opened in a browser"),
        link::Outcome::Copied => app.flash("PR url copied to the clipboard"),
        link::Outcome::Failed(e) => app.flash(format!("could not open: {e}")),
    }
}

/// Turn the selection into the instruction the shell wrapper acts on.
fn activate(app: &mut App) -> bool {
    let Some(w) = app.workstream() else {
        return false;
    };
    app.action = Some(match &w.path {
        Some(p) => format!("cd\t{}", p.display()),
        None => format!("new\t{}\t{}", w.qualified(), w.git.branch),
    });
    true
}

fn dump(no_gh: bool) -> Result<()> {
    let mut app = App::new()?;
    app.scan_now()?;
    if !no_gh {
        let branches: Vec<String> = app
            .projects
            .iter()
            .flat_map(|p| p.workstreams.iter())
            .map(|w| w.git.remote_branch.clone())
            .collect();
        match github::refresh(app::OWNER, app::REPO, &branches) {
            Ok(cache) => {
                github::apply(&mut app.projects, &cache);
                app.fetched_at = Some(cache.fetched_at);
                app::compute_merged(&mut app.projects);
            }
            Err(e) => eprintln!("github: {e:#} (using cache if present)"),
        }
    }
    for p in &app.projects {
        println!("{} {} — {}", p.emoji, p.slug, p.name);
        for w in &p.workstreams {
            let pr = match &w.pr {
                Some(pr) => format!(
                    "#{} {} {} {}",
                    pr.number,
                    pr.state.label(),
                    pr.checks.state.glyph(),
                    pr.checks.total
                ),
                None => "—".into(),
            };
            println!(
                "    {:<24} {:<8} {:<46} +{:<3} −{:<3} {:<9} {}",
                w.name,
                if w.is_virtual() { "virtual" } else { "" },
                w.git.branch,
                w.git.ahead,
                w.git.behind,
                w.merged.label(),
                pr
            );
        }
    }
    Ok(())
}
