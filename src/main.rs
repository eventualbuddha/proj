mod app;
mod cache;
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
use std::path::PathBuf;
use std::time::Duration;

use app::{App, Confirm, CopyMenu, Pane, Select};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("proj — project and workstream dashboard");
        println!();
        println!("  proj              the TUI");
        println!("  proj --dump       print the same state as text");
        println!("  proj --cd-file F  write the chosen workstream's path to F on exit");
        println!("  proj --select P[/W]  open focused there; defaults to the cwd");
        println!("  proj --render     draw one frame to stdout and exit");
        println!("  proj --render --loading   draw the pre-scan frame");
        println!("  proj --render --styles N  dump the resolved fg/bg of row N");
        println!("  proj --render --press=KEYS  press KEYS first (e.g. --press=y)");
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
    //
    // It goes through exactly the startup the TUI does, including the threaded
    // scan and the held selection, rather than a synchronous shortcut. It used
    // to use the shortcut, and that is how the selection silently stopped being
    // applied at all: --render kept working because it took a path the program
    // no longer took.
    if args.iter().any(|a| a == "--render") {
        let mut app = App::new()?;
        app.select_now(select_target(&args));

        // --loading draws the pre-scan state, which is otherwise only on screen
        // for the few hundred milliseconds the background scan takes.
        if !args.iter().any(|a| a == "--loading") {
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            while app.loading && std::time::Instant::now() < deadline {
                app.drain();
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        // --loading means "draw whatever is on screen now", so it must not wait
        // for anything; it is how startup latency gets measured.
        let waiting = !args.iter().any(|a| a == "--loading");

        // Wait for the first network refresh too, not just the scan. With a warm
        // cache the render is right either way; with a cold one it would show a
        // dashboard with no PR state at all and no way to tell that apart from
        // there being none.
        if waiting {
            let deadline = std::time::Instant::now() + Duration::from_secs(60);
            while app.fetched_at.is_none() && std::time::Instant::now() < deadline {
                app.drain();
                std::thread::sleep(Duration::from_millis(50));
            }
        }

        // And for the merged-ness pass behind the scan. A dashboard that has
        // scanned is not yet one that can answer `d`: pressing it while the
        // pass is still running drew "NOT merged into main" over a merged
        // branch, since a fresh scan starts every row at `No`.
        if waiting {
            let deadline = std::time::Instant::now() + Duration::from_secs(60);
            while (app.scanning || app.merging) && std::time::Instant::now() < deadline {
                app.drain();
                std::thread::sleep(Duration::from_millis(20));
            }
        }

        // The event loop asks for the selected row's check contexts every
        // frame; do the same here, or --render shows a menu missing its CI entry
        // and looks like a bug in the menu rather than in the harness.
        if waiting {
            app.request_contexts();
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        while waiting
            && std::time::Instant::now() < deadline
            && app
                .workstream()
                .and_then(|w| w.pr.as_ref())
                .is_some_and(|pr| pr.checks.contexts.is_none() && !pr.checks.is_empty())
        {
            app.drain();
            std::thread::sleep(Duration::from_millis(20));
        }

        // Drive the real key handler rather than setting state directly, so
        // what gets drawn is what pressing the key produces.
        for k in args.iter().filter_map(|a| a.strip_prefix("--press")) {
            // `\n` is a literal backslash-n: enter, which the modals that ask
            // for something need and which no shell will pass as a raw key.
            let mut chars = k.trim_start_matches('=').chars().peekable();
            while let Some(c) = chars.next() {
                let code = match c {
                    '\\' if chars.peek() == Some(&'n') => {
                        chars.next();
                        KeyCode::Enter
                    }
                    c => KeyCode::Char(c),
                };
                handle_key(
                    &mut app,
                    crossterm::event::KeyEvent::new(code, crossterm::event::KeyModifiers::NONE),
                );
            }
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
    // window goes up immediately with a loading modal over it. Which means the
    // selection cannot be applied yet -- there is nothing to select in -- so it
    // is held until the scan lands.
    let mut app = App::new()?;
    app.select_now(select_target(&args));

    let mut term = setup()?;
    let result = run(&mut term, &mut app);
    restore()?;
    result?;

    if let (Some(path), Some(action)) = (cd_file, app.action) {
        std::fs::write(path, action)?;
    }
    Ok(())
}

/// Open focused on a workstream: the one `--select` names, or -- failing that --
/// whichever one the current directory is inside.
///
/// Falling back to the cwd is the common case. You run `proj` while standing in
/// the thing you are working on, and having it open at the top of an alphabetical
/// list means scrolling back to where you already were.
fn select_target(args: &[String]) -> Option<Select> {
    let explicit = args
        .iter()
        .position(|a| a == "--select")
        .and_then(|i| args.get(i + 1))
        .map(|s| {
            let mut it = s.splitn(2, '/');
            Select {
                project: it.next().unwrap_or_default().to_string(),
                workstream: it.next().map(str::to_string),
                // Someone named this explicitly, so put the cursor on it.
                focus: true,
            }
        });

    explicit.or_else(|| {
        let (project, workstream) = std::env::current_dir()
            .ok()
            .and_then(|d| discover::locate(&d))?;
        Some(Select {
            project,
            workstream,
            // Inferred, not asked for: highlight the row but leave the cursor in
            // the projects pane.
            focus: false,
        })
    })
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

        if handle_key(app, key) {
            break;
        }
    }
    Ok(())
}

/// Handle one keypress. Returns true to leave the loop.
///
/// Split out of `run` so it can be driven without a terminal -- the event loop
/// needs a real tty, this does not, and every key-handling bug so far has been
/// in code that only a tty could reach.
fn handle_key(app: &mut App, key: crossterm::event::KeyEvent) -> bool {
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
        return false;
    }

    // Naming a new workstream: every key is either text or navigation within
    // the modal, so nothing below can see them.
    if let Some(n) = &mut app.new_ws {
        if n.picking_base {
            match key.code {
                // Back to the name rather than out entirely: esc steps back
                // here as it does everywhere else.
                KeyCode::Esc => n.picking_base = false,
                KeyCode::Char('j') | KeyCode::Down => {
                    n.base_idx = (n.base_idx + 1) % n.bases.len();
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    n.base_idx = (n.base_idx + n.bases.len() - 1) % n.bases.len();
                }
                KeyCode::Enter => return create_workstream(app),
                _ => {}
            }
        } else {
            match key.code {
                KeyCode::Esc => app.new_ws = None,
                KeyCode::Backspace => {
                    n.name.pop();
                }
                KeyCode::Enter => app.new_name_done(),
                KeyCode::Char(c) => n.name.push(c),
                _ => {}
            }
        }
        return false;
    }

    // The copy menu owns the keyboard while it is open.
    if let Some(menu) = &mut app.copy_menu {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => app.copy_menu = None,
            KeyCode::Char('j') | KeyCode::Down => {
                if !menu.items.is_empty() {
                    menu.idx = (menu.idx + 1) % menu.items.len();
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if !menu.items.is_empty() {
                    menu.idx = (menu.idx + menu.items.len() - 1) % menu.items.len();
                }
            }
            // Digits jump straight to an entry and copy it, so the common case
            // is two keystrokes rather than a scroll. `0` is the tenth, which is
            // otherwise unreachable by digit and is exactly where the project
            // directory lands on a row with a PR.
            KeyCode::Char(c) if c.is_ascii_digit() => {
                let d = c.to_digit(10).unwrap() as usize;
                let n = if d == 0 { 9 } else { d - 1 };
                if n < menu.items.len() {
                    menu.idx = n;
                    copy_selected(app);
                }
            }
            KeyCode::Enter | KeyCode::Char('y') => copy_selected(app),
            _ => {}
        }
        return false;
    }

    // A confirmation swallows every key but y/n: a destructive action must
    // never be one keystroke away from a mistyped navigation key.
    if let Some(c) = &app.confirm {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                let verb = c.verb.clone();
                app.confirm = None;
                app.action = Some(verb);
                return true;
            }
            _ => app.confirm = None,
        }
        return false;
    }

    if app.help {
        app.help = false;
        return false;
    }

    match key.code {
        KeyCode::Char('q') => return true,
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return true,

        // Esc steps back rather than quitting. Quitting is `q`, and only `q`:
        // esc is what you press to get out of the thing you are in, and having
        // it also mean "close the program" makes leaving a filter or a pane feel
        // like standing next to a trapdoor.
        KeyCode::Esc => app.pane = Pane::Projects,
        KeyCode::Char('j') | KeyCode::Down => app.move_down(),
        KeyCode::Char('k') | KeyCode::Up => app.move_up(),
        KeyCode::Tab | KeyCode::Char('l') | KeyCode::Right => app.pane = Pane::Workstreams,
        KeyCode::BackTab | KeyCode::Char('h') | KeyCode::Left => app.pane = Pane::Projects,
        KeyCode::Char('/') => {
            app.filtering = true;
            app.filter = Some(String::new());
        }
        KeyCode::Char('?') => app.help = true,
        KeyCode::Char('[') => app.cycle_sidebar(false),
        KeyCode::Char(']') => app.cycle_sidebar(true),
        // The project's README is where the state of a project actually lives --
        // what is blocked, what the traps are, what to do next. `n` is taken by
        // "new workstream", so `o` it is.
        KeyCode::Char('o') if app.sidebar == app::Sidebar::Reviews => open_url(app),
        KeyCode::Char('o') => {
            let dir = app
                .project()
                .and_then(|p| p.readme.parent().map(|d| d.display().to_string()));
            match dir {
                Some(d) => {
                    if emit(app, format!("notes\t{d}")) {
                        return true;
                    }
                }
                None => app.flash("no project selected"),
            }
        }

        // A workstream that does not exist yet, and so is on no row: the one
        // thing the dashboard could not reach until now.
        KeyCode::Char('n') => app.begin_new_workstream(),

        // Navigate and launch.
        KeyCode::Char('g') => {
            if let Some(p) = require_path(app) {
                if emit(app, format!("lazygit\t{p}")) {
                    return true;
                }
            }
        }
        KeyCode::Char('G') => open_github_menu(app),

        // The author's latest, whether or not it is on disk yet.
        KeyCode::Char('c') if app.sidebar == app::Sidebar::Reviews => {
            return checkout_review(app);
        }
        KeyCode::Char('e') => {
            if let Some(p) = require_path(app) {
                if emit(app, format!("edit\t{p}")) {
                    return true;
                }
            }
        }
        KeyCode::Char('y') => open_copy_menu(app),

        // Git.
        KeyCode::Char('b') => {
            if let Some(p) = require_path(app) {
                if emit(app, format!("rebase\t{p}")) {
                    return true;
                }
            }
        }
        KeyCode::Char('p') | KeyCode::Char('P') if app.sidebar == app::Sidebar::Reviews => {
            // A review checkout tracks `refs/pull/<n>/head`, which is not a
            // branch you can push to, and the branch it would push to belongs to
            // someone else.
            app.flash("a review checkout has nowhere to push");
        }
        KeyCode::Char('p') | KeyCode::Char('P') => {
            let force = matches!(key.code, KeyCode::Char('P'));
            let remote = app.workstream().map(|w| w.git.remote_branch.clone());
            if let (Some(p), Some(remote)) = (require_path(app), remote) {
                let verb = if force { "push-force" } else { "push" };
                if emit(app, format!("{verb}\t{p}\t{remote}")) {
                    return true;
                }
            }
        }

        KeyCode::Char('d') => confirm_delete(app),
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
                return true;
            }
        }
        _ => {}
    }

    false
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
            // In the review queue the sidebar is the only list, and its rows are
            // three lines tall. The checks pane beside it is a read-out with
            // nothing to select, so a click there is not a focus change.
            if app.sidebar == app::Sidebar::Reviews {
                if inside(r.detail) {
                    if Some(y) == app.url_row {
                        open_url(app);
                    }
                    return false;
                }
                if !inside(r.projects) {
                    return false;
                }
                let Some(row) = y.checked_sub(r.projects.y + 1) else {
                    return false;
                };
                let idx = app.list_state.offset() + (row / ui::REVIEW_ROW_LINES) as usize;
                if idx >= app.reviews.len() {
                    return false;
                }
                // Click to select, click again to open -- the same rule as a
                // workstream row, and here the second click can start a build.
                let reselect = app.review_idx == idx;
                app.review_idx = idx;
                return reselect && activate(app);
            }

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
///
/// No longer bound to a key -- `o` is the project's notes now -- but still what
/// clicking the underlined url in the detail pane does.
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

/// GitHub actions for the selected row.
fn open_github_menu(app: &mut App) {
    let items = if app.sidebar == app::Sidebar::Reviews {
        let Some(r) = app.review().cloned() else {
            app.flash("nothing waiting on you");
            return;
        };
        CopyMenu::github_for_review(&r)
    } else {
        let Some(w) = app.workstream().cloned() else {
            app.flash("nothing selected");
            return;
        };
        CopyMenu::github_for(&w)
    };
    if items.is_empty() {
        app.flash("no GitHub actions for this row");
        return;
    }
    app.copy_menu = Some(CopyMenu::github(items));
}

/// Run whichever action the github menu entry names.
fn run_github(app: &mut App, label: String, value: String) {
    // `rerun:` carries a url, which has colons of its own, so the split takes
    // only the first.
    let Some((verb, rest)) = value.split_once(':').map(|(v, r)| (v, r.to_string())) else {
        app.flash(format!("unknown action: {label}"));
        return;
    };

    match verb {
        "ready" => {
            app.action = Some(format!("gh-ready\t{rest}"));
        }
        "rerun" => {
            app.action = Some(format!("ci-rerun\t{rest}"));
        }
        "review" | "ready-review" => {
            let number: u32 = rest.parse().unwrap_or(0);
            app.copy_menu = Some(CopyMenu::reviewer(format!("{verb}:{rest}")));
            let tx = app.tx.clone();
            std::thread::spawn(move || {
                let list = github::suggested_reviewers(app::OWNER, app::REPO, number)
                    .unwrap_or_default();
                let _ = tx.send(app::Msg::Reviewers(list));
            });
        }
        _ => app.flash("unknown action"),
    }
}

/// Build the copy menu for the selected row.
fn open_copy_menu(app: &mut App) {
    if app.sidebar == app::Sidebar::Reviews {
        let Some(r) = app.review().cloned() else {
            app.flash("nothing to copy");
            return;
        };
        app.copy_menu = Some(CopyMenu::for_review(&r));
        return;
    }
    let Some(w) = app.workstream().cloned() else {
        app.flash("nothing selected");
        return;
    };
    app.copy_menu = Some(CopyMenu::build(&w));
}

/// Copy the highlighted entry and close the menu.
fn copy_selected(app: &mut App) {
    let kind = app.copy_menu.as_ref().map(|m| m.kind);
    if kind == Some(app::MenuKind::Github) {
        let Some((label, value, action)) = app
            .copy_menu
            .as_ref()
            .and_then(|m| m.selected())
            .map(|i| (i.label.clone(), i.value.clone(), i.action.clone()))
        else {
            return;
        };
        app.copy_menu = None;
        match action {
            Some(a) => run_github(app, label, a),
            // No action: it is a url or other text, and copying is the whole job.
            None => match link::copy(&value) {
                Ok(()) => app.flash(format!("{label} copied")),
                Err(e) => app.flash(format!("clipboard: {e}")),
            },
        }
        return;
    }
    if kind == Some(app::MenuKind::Reviewer) {
        let login = app
            .copy_menu
            .as_ref()
            .and_then(|m| m.selected())
            .map(|i| i.value.clone());
        let pending = app.copy_menu.as_ref().and_then(|m| m.pending.clone());
        app.copy_menu = None;
        if let (Some(login), Some(pending)) = (login, pending) {
            let (verb, number) = pending.split_once(':').unwrap_or(("review", ""));
            let v = if verb == "ready-review" { "gh-ready-review" } else { "gh-review" };
            app.action = Some(format!("{v}\t{number}\t{login}"));
        }
        return;
    }

    let Some((label, value)) = app
        .copy_menu
        .as_ref()
        .and_then(|m| m.selected())
        .map(|i| (i.label.clone(), i.value.clone()))
    else {
        app.copy_menu = None;
        return;
    };
    app.copy_menu = None;
    match link::copy(&value) {
        Ok(()) => app.flash(format!("{label} copied")),
        Err(e) => app.flash(format!("clipboard: {e}")),
    }
}

/// Emit a verb for the shell wrapper and quit the loop.
///
/// Everything that runs longer than an instant, or that can fail in a way you
/// need to read, goes out to the shell rather than being run in here: a rebase
/// that conflicts, a push that is rejected, a build that fails. The wrapper
/// reopens the dashboard afterwards, so this is a round trip rather than an
/// exit.
fn emit(app: &mut App, verb: String) -> bool {
    app.action = Some(verb);
    true
}

/// The selected row's worktree path, or a note in the footer saying why there
/// isn't one. Every action below needs a worktree; a virtual row has none.
///
/// Reviews go through here too. Once a PR is checked out its worktree is an
/// ordinary one -- lazygit, `$EDITOR` and a rebase all mean the same thing in it
/// -- and reading the path off the visible row is what keeps those keys from
/// acting on whatever the *other* sidebar happens to have selected.
fn require_path(app: &mut App) -> Option<String> {
    if app.sidebar == app::Sidebar::Reviews {
        return match app.review().and_then(|r| r.worktree.clone()) {
            Some(p) => Some(p.display().to_string()),
            None => {
                app.flash("not checked out — ↵ checks it out");
                None
            }
        };
    }
    match app.workstream().and_then(|w| w.path.clone()) {
        Some(p) => Some(p.display().to_string()),
        None => {
            app.flash("no worktree yet — ↵ creates one");
            None
        }
    }
}

/// Turn the selection into the instruction the shell wrapper acts on.
fn activate(app: &mut App) -> bool {
    if app.sidebar == app::Sidebar::Reviews {
        return activate_review(app);
    }
    let Some(w) = app.workstream() else {
        return false;
    };
    app.action = Some(match &w.path {
        Some(p) => format!("cd\t{}", p.display()),
        None => format!("new\t{}\t{}", w.qualified(), w.git.branch),
    });
    true
}

/// A review row is the same gesture as a workstream row: go to the worktree if
/// it exists, and otherwise make it.
///
/// Making it means fetching `refs/pull/<n>/head`, adding a worktree under the
/// `review` project and building it -- minutes of pnpm output, which is why it
/// goes out to the shell rather than running here. Afterwards the checkout is a
/// row in the Projects sidebar like any other, and this key is a plain cd.
fn activate_review(app: &mut App) -> bool {
    let Some(r) = app.review() else {
        app.flash("nothing waiting on you");
        return false;
    };
    let verb = match &r.worktree {
        Some(p) => format!("cd\t{}", p.display()),
        None => format!("review-checkout\t{}\t{}", r.number, r.dir_name()),
    };
    emit(app, verb)
}

/// `c`: the author's latest, checking the PR out if it is not on disk and
/// fast-forwarding the checkout if it is.
///
/// ↵ cannot be this. A cd has to stay instant and offline, and `b` -- rebase on
/// main -- is the wrong move on someone else's branch: what has moved is their
/// head, not the base.
fn checkout_review(app: &mut App) -> bool {
    let Some(r) = app.review() else {
        app.flash("nothing waiting on you");
        return false;
    };
    let verb = match &r.worktree {
        Some(p) => format!("review-update\t{}\t{}", p.display(), r.number),
        None => format!("review-checkout\t{}\t{}", r.number, r.dir_name()),
    };
    emit(app, verb)
}

/// Hand the shell a workstream to create. Same verb as materializing a virtual
/// row -- the only difference is that this one names a base, because there is no
/// existing branch to take the answer from.
fn create_workstream(app: &mut App) -> bool {
    let Some(n) = &app.new_ws else { return false };
    let (qualified, branch, base) = (
        format!("{}/{}", n.project, n.name),
        n.branch(),
        n.base().to_string(),
    );
    app.new_ws = None;
    emit(app, format!("new\t{qualified}\t{branch}\t{base}"))
}

/// Ask before removing a workstream, and say what is at stake.
///
/// `proj rm` refuses on uncommitted work or on commits that exist nowhere else,
/// so this is not the safety net -- it is the part that tells you *which*
/// workstream you are about to remove, before the shell scrolls past with an
/// answer.
fn confirm_delete(app: &mut App) {
    if app.sidebar == app::Sidebar::Reviews {
        return confirm_delete_review(app);
    }
    if app.workstream().is_some_and(|w| w.is_virtual()) {
        return confirm_delete_branch(app);
    }
    let Some(w) = app.workstream() else { return };

    let qualified = w.qualified();
    let mut body = vec![format!("branch  {}", w.git.branch)];
    if w.merged.is_merged() {
        body.push(format!("this is {} into main", w.merged.label()));
    } else {
        body.push("NOT merged into main".to_string());
    }
    if w.git.dirty > 0 {
        body.push(format!("{} uncommitted change(s) — proj rm will refuse", w.git.dirty));
    }

    // Commits that exist nowhere but here. Not the same question as "has an
    // upstream": a merged branch is deleted on the remote and its tracking ref
    // pruned, which used to be reported as "never pushed — proj rm will refuse"
    // on the one row where nothing at all was at risk.
    if !w.merged.is_merged() {
        // Both unique to this branch and unpushed. `unpushed` alone counts
        // main's own commits when the upstream ref is stale -- 45, where three
        // of them are yours.
        let stranded = w.git.ahead.min(w.git.unpushed.unwrap_or(u32::MAX));
        if stranded > 0 {
            body.push(format!(
                "{stranded} commit(s) are only here — proj rm will refuse"
            ));
        }
    }
    if !w.git.pushed {
        body.push("never pushed to a remote".to_string());
    } else if w.git.upstream.is_none() {
        body.push(format!("origin/{} is gone", w.git.remote_branch));
    }

    app.confirm = Some(Confirm {
        title: format!(" delete {qualified} "),
        body,
        verb: format!("delete\t{qualified}"),
    });
}

/// Ask before deleting a branch that has no worktree.
///
/// There is no `proj rm` behind this one and so nothing that will refuse it:
/// what the confirmation says about merged-ness and about commits that live
/// nowhere else is the entire safety net.
fn confirm_delete_branch(app: &mut App) {
    let Some(w) = app.workstream() else { return };
    let (qualified, branch, remote) = (
        w.qualified(),
        w.git.branch.clone(),
        w.git.remote_branch.clone(),
    );

    let mut body = vec![format!("branch  {branch}")];
    if w.merged.is_merged() {
        body.push(format!("this is {} into main", w.merged.label()));
    } else {
        body.push("NOT merged into main".to_string());
        let stranded = w.git.ahead.min(w.git.unpushed.unwrap_or(u32::MAX));
        if stranded > 0 {
            body.push(format!(
                "{stranded} commit(s) are only here — they will be lost"
            ));
        }
    }
    if !w.git.pushed {
        body.push("never pushed to a remote".to_string());
    } else if w.git.upstream.is_none() {
        body.push(format!("origin/{remote} is gone"));
    } else {
        body.push(format!("also deletes origin/{remote}"));
    }
    if w.pr
        .as_ref()
        .is_some_and(|pr| pr.state == model::PrState::Open)
    {
        body.push(format!(
            "PR #{} is still open",
            w.pr.as_ref().unwrap().number
        ));
    }

    app.confirm = Some(Confirm {
        title: format!(" delete the branch {branch} "),
        body,
        // The remote name goes along whatever the tracking ref says: a prune
        // that has not run yet leaves `origin/<name>` there to delete, and the
        // remote side is checked before anything is pushed.
        verb: format!("delete-branch\t{qualified}\t{branch}\t{remote}"),
    });
}

/// Ask before throwing away a review checkout.
///
/// A review worktree is disposable by design -- it is a copy of someone else's
/// branch and there is nothing in it to lose -- but "disposable" is not
/// "nothing", so the confirmation still says which PR is going and `proj rm`
/// still refuses on uncommitted changes.
fn confirm_delete_review(app: &mut App) {
    let Some(r) = app.review() else { return };
    let Some(path) = &r.worktree else {
        app.flash("not checked out — nothing to delete");
        return;
    };
    // `proj rm` takes `<project>/<workstream>`, and both halves are right there
    // in the path -- worth reading off it rather than assuming the review
    // project is called `review`.
    let name = path.file_name().map(|n| n.to_string_lossy().to_string());
    let project = path
        .parent()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().to_string());
    let (Some(name), Some(project)) = (name, project) else {
        return;
    };
    let (number, title) = (r.number, truncate_title(&r.title));
    app.confirm = Some(Confirm {
        title: format!(" delete the review checkout of #{number} "),
        body: vec![title, path.display().to_string()],
        verb: format!("delete\t{project}/{name}"),
    });
}

/// Enough of a PR title to recognise, for a modal that is 64 columns wide.
fn truncate_title(title: &str) -> String {
    if title.chars().count() <= 58 {
        return title.to_string();
    }
    format!("{}…", title.chars().take(57).collect::<String>())
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
            // The op, where there is one, matters more than the merged label --
            // both here and on screen -- because everything else on the row is
            // measured against a HEAD part-way through it.
            let state = match &w.git.op {
                Some(op) => op.label(),
                None => w.merged.label().to_string(),
            };
            println!(
                "    {:<24} {:<8} {:<46} +{:<3} −{:<3} {:<14} {}",
                w.name,
                if w.is_virtual() { "virtual" } else { "" },
                w.git.branch,
                w.git.ahead,
                w.git.behind,
                state,
                pr
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEvent, KeyModifiers};

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }
    fn esc() -> KeyEvent {
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)
    }

    /// An app with no projects is enough: every assertion here is about pane
    /// focus and quitting, neither of which needs anything on screen.
    fn app() -> App {
        let mut a = App::new().expect("app");
        a.loading = false;
        a
    }

    #[test]
    fn esc_never_quits() {
        let mut a = app();
        a.pane = Pane::Workstreams;
        assert!(!handle_key(&mut a, esc()));
        assert!(!handle_key(&mut a, esc()), "esc on the left pane still stays");
    }

    #[test]
    fn esc_drops_focus_back_to_projects() {
        let mut a = app();
        a.pane = Pane::Workstreams;
        handle_key(&mut a, esc());
        assert!(a.pane == Pane::Projects);
    }

    #[test]
    fn esc_clears_a_filter_without_quitting() {
        let mut a = app();
        handle_key(&mut a, key('/'));
        handle_key(&mut a, key('x'));
        assert_eq!(a.filter.as_deref(), Some("x"));
        assert!(!handle_key(&mut a, esc()));
        assert!(a.filter.is_none() && !a.filtering);
    }

    #[test]
    fn esc_cancels_a_confirmation_without_acting() {
        let mut a = app();
        a.confirm = Some(app::Confirm {
            title: "t".into(),
            body: vec![],
            verb: "delete\tp/w".into(),
        });
        assert!(!handle_key(&mut a, esc()));
        assert!(a.confirm.is_none());
        assert!(a.action.is_none(), "cancelling must not emit the verb");
    }

    #[test]
    fn q_quits_and_ctrl_c_quits() {
        assert!(handle_key(&mut app(), key('q')));
        assert!(handle_key(
            &mut app(),
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
        ));
    }

    #[test]
    fn q_inside_a_filter_is_typed_not_obeyed() {
        let mut a = app();
        handle_key(&mut a, key('/'));
        assert!(!handle_key(&mut a, key('q')));
        assert_eq!(a.filter.as_deref(), Some("q"));
    }
}

/// The review queue's ↵, which is the same gesture as a workstream's and has to
/// mean the same two things: go there, or make it.
#[cfg(test)]
mod review_tests {
    use super::*;
    use crossterm::event::{KeyEvent, KeyModifiers};
    use model::*;
    use std::path::PathBuf;

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }
    fn code(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    fn review(number: u32, title: &str) -> Review {
        Review {
            number,
            title: title.into(),
            url: format!("https://github.com/votingworks/vxsuite/pull/{number}"),
            author: "someone".into(),
            branch: "someone/fix".into(),
            state: PrState::Open,
            checks: Checks::default(),
            reason: ReviewReason::Requested,
            updated: 0,
            worktree: None,
        }
    }

    /// A `review` project holding `dirs`, and one review in the queue.
    fn app(dirs: &[&str], reviews: Vec<Review>) -> App {
        let mut a = App::new().expect("app");
        a.loading = false;
        a.sidebar = app::Sidebar::Reviews;
        a.projects = vec![Project {
            slug: "review".into(),
            emoji: "👀".into(),
            name: "Code review".into(),
            kind: ProjectKind::Review,
            status: "active".into(),
            readme: PathBuf::from("/tmp/projects/review/README.md"),
            branch_prefix: None,
            branch_globs: Vec::new(),
            workstreams: dirs
                .iter()
                .map(|d| Workstream {
                    project: "review".into(),
                    name: (*d).into(),
                    origin: Origin::Worktree,
                    path: Some(PathBuf::from(format!("/tmp/projects/review/{d}"))),
                    git: GitState::default(),
                    merged: Merged::No,
                    pr: None,
                })
                .collect(),
        }];
        a.reviews = reviews;
        a.link_reviews();
        a
    }

    #[test]
    fn enter_on_a_review_with_no_worktree_checks_it_out() {
        let mut a = app(&[], vec![review(9083, "Fix the scanner status poll")]);
        assert!(handle_key(&mut a, code(KeyCode::Enter)), "checking out quits");
        assert_eq!(
            a.action.as_deref(),
            Some("review-checkout\t9083\t9083-fix-the-scanner-status")
        );
    }

    #[test]
    fn enter_on_a_checked_out_review_is_a_cd() {
        let mut a = app(&["9083-fix-the-scanner"], vec![review(9083, "Fix the scanner status poll")]);
        assert!(handle_key(&mut a, code(KeyCode::Enter)));
        assert_eq!(
            a.action.as_deref(),
            Some("cd\t/tmp/projects/review/9083-fix-the-scanner")
        );
    }

    /// The number leads the directory name so that this holds: a PR retitled
    /// after checkout is still the same checkout.
    #[test]
    fn a_retitled_pr_is_not_checked_out_twice() {
        let a = app(&["9083-old-title-here"], vec![review(9083, "A completely different title")]);
        assert_eq!(
            a.review().unwrap().worktree.as_ref().map(|p| p.display().to_string()),
            Some("/tmp/projects/review/9083-old-title-here".to_string())
        );
    }

    #[test]
    fn a_number_that_only_prefixes_another_is_not_a_match() {
        let a = app(&["90830-something"], vec![review(9083, "Nine oh eight three")]);
        assert!(a.review().unwrap().worktree.is_none());
    }

    #[test]
    fn c_updates_a_checkout_and_creates_one_that_is_missing() {
        let mut a = app(&["9083-fix-the-scanner"], vec![review(9083, "Fix it")]);
        assert!(handle_key(&mut a, key('c')));
        assert_eq!(
            a.action.as_deref(),
            Some("review-update\t/tmp/projects/review/9083-fix-the-scanner\t9083")
        );

        let mut a = app(&[], vec![review(9083, "Fix it")]);
        assert!(handle_key(&mut a, key('c')));
        assert_eq!(a.action.as_deref(), Some("review-checkout\t9083\t9083-fix-it"));
    }

    /// Every one of these read the *other* sidebar's selection before the queue
    /// had rows of its own, which is an action on a row you cannot see.
    #[test]
    fn worktree_actions_wait_for_the_checkout() {
        for k in ['g', 'e', 'b', 'd'] {
            let mut a = app(&[], vec![review(9083, "Fix it")]);
            assert!(!handle_key(&mut a, key(k)), "{k} should not quit");
            assert!(a.action.is_none(), "{k} acted on nothing");
            assert!(a.confirm.is_none(), "{k} offered to delete nothing");
            assert!(a.flash.is_some(), "{k} should say why");
        }
    }

    #[test]
    fn worktree_actions_land_in_the_checkout() {
        for (k, verb) in [
            ('g', "lazygit\t/tmp/projects/review/9083-fix-it"),
            ('e', "edit\t/tmp/projects/review/9083-fix-it"),
        ] {
            let mut a = app(&["9083-fix-it"], vec![review(9083, "Fix it")]);
            assert!(handle_key(&mut a, key(k)));
            assert_eq!(a.action.as_deref(), Some(verb));
        }
    }

    #[test]
    fn d_offers_the_checkout_and_not_a_workstream() {
        let mut a = app(&["9083-fix-it"], vec![review(9083, "Fix it")]);
        handle_key(&mut a, key('d'));
        let c = a.confirm.as_ref().expect("a confirmation");
        assert_eq!(c.verb, "delete\treview/9083-fix-it");
    }

    #[test]
    fn there_is_nowhere_to_push_a_review() {
        let mut a = app(&["9083-fix-it"], vec![review(9083, "Fix it")]);
        assert!(!handle_key(&mut a, key('p')));
        assert!(a.action.is_none());
        assert!(a.flash.is_some());
    }

    #[test]
    fn an_empty_queue_swallows_the_key() {
        let mut a = app(&[], Vec::new());
        assert!(!handle_key(&mut a, code(KeyCode::Enter)));
        assert!(a.action.is_none());
    }
}

#[cfg(test)]
mod new_workstream_tests {
    use super::*;
    use crossterm::event::{KeyEvent, KeyModifiers};
    use model::*;

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }
    fn code(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }
    fn type_in(a: &mut App, s: &str) {
        for c in s.chars() {
            handle_key(a, key(c));
        }
    }

    /// A two-workstream project and nothing else, so the base list is known.
    fn app() -> App {
        let mut a = App::new().expect("app");
        a.loading = false;
        a.projects = vec![Project {
            slug: "react-19".into(),
            emoji: "⚛️".into(),
            name: "React 19".into(),
            kind: ProjectKind::Project,
            status: "active".into(),
            readme: std::path::PathBuf::from("/tmp/react-19/README.md"),
            branch_prefix: None,
            branch_globs: Vec::new(),
            workstreams: vec![ws("react-query"), ws("esm-lib")],
        }];
        a
    }

    fn ws(name: &str) -> Workstream {
        Workstream {
            project: "react-19".into(),
            name: name.into(),
            origin: Origin::Worktree,
            path: Some(std::path::PathBuf::from(format!("/tmp/react-19/{name}"))),
            git: GitState {
                branch: format!("react-19/{name}"),
                ..Default::default()
            },
            merged: Merged::No,
            pr: None,
        }
    }

    #[test]
    fn n_offers_main_and_the_projects_own_workstreams_as_bases() {
        let mut a = app();
        handle_key(&mut a, key('n'));
        type_in(&mut a, "hooks");
        handle_key(&mut a, code(KeyCode::Enter));
        let n = a.new_ws.as_ref().expect("naming");
        assert!(n.picking_base);
        let bases: Vec<&str> = n.bases.iter().map(|b| b.branch.as_str()).collect();
        assert_eq!(bases, ["main", "react-19/react-query", "react-19/esm-lib"]);
        assert_eq!(n.base(), "main", "main is where the cursor starts");
    }

    #[test]
    fn the_verb_carries_the_branch_and_the_chosen_base() {
        let mut a = app();
        handle_key(&mut a, key('n'));
        type_in(&mut a, "hooks");
        handle_key(&mut a, code(KeyCode::Enter));
        handle_key(&mut a, key('j'));
        assert!(handle_key(&mut a, code(KeyCode::Enter)), "creating quits");
        assert_eq!(
            a.action.as_deref(),
            Some("new\treact-19/hooks\treact-19/hooks\treact-19/react-query")
        );
    }

    #[test]
    fn typing_is_typing_and_not_a_keybinding() {
        let mut a = app();
        handle_key(&mut a, key('n'));
        // Every one of these is a command outside the modal.
        assert!(!handle_key(&mut a, key('q')));
        type_in(&mut a, "dpj");
        assert_eq!(a.new_ws.as_ref().unwrap().name, "qdpj");
        assert!(a.action.is_none() && a.confirm.is_none());
    }

    #[test]
    fn esc_steps_back_from_the_base_and_then_out() {
        let mut a = app();
        handle_key(&mut a, key('n'));
        type_in(&mut a, "hooks");
        handle_key(&mut a, code(KeyCode::Enter));
        handle_key(&mut a, code(KeyCode::Esc));
        let n = a.new_ws.as_ref().expect("still naming");
        assert!(!n.picking_base);
        assert_eq!(n.name, "hooks", "the name survives the step back");
        handle_key(&mut a, code(KeyCode::Esc));
        assert!(a.new_ws.is_none());
        assert!(a.action.is_none(), "cancelling must not create anything");
    }

    #[test]
    fn a_name_that_cannot_be_a_directory_is_refused() {
        for bad in ["", "feat/hooks", "two words"] {
            let mut a = app();
            handle_key(&mut a, key('n'));
            type_in(&mut a, bad);
            handle_key(&mut a, code(KeyCode::Enter));
            assert!(
                !a.new_ws.as_ref().unwrap().picking_base,
                "{bad:?} should not have been accepted"
            );
            assert!(a.flash.is_some(), "{bad:?} should say why");
        }
    }

    #[test]
    fn a_name_already_in_the_project_is_refused() {
        let mut a = app();
        handle_key(&mut a, key('n'));
        type_in(&mut a, "esm-lib");
        handle_key(&mut a, code(KeyCode::Enter));
        assert!(!a.new_ws.as_ref().unwrap().picking_base);
    }

    #[test]
    fn backspace_edits_the_name() {
        let mut a = app();
        handle_key(&mut a, key('n'));
        type_in(&mut a, "hooks");
        handle_key(&mut a, code(KeyCode::Backspace));
        assert_eq!(a.new_ws.as_ref().unwrap().name, "hook");
    }
}

#[cfg(test)]
mod delete_tests {
    use super::*;
    use crossterm::event::{KeyEvent, KeyModifiers};
    use model::*;

    fn app(git: GitState, merged: Merged) -> App {
        let mut a = App::new().expect("app");
        a.loading = false;
        a.projects = vec![Project {
            slug: "backup-restore".into(),
            emoji: "🔐".into(),
            name: "Backup & restore".into(),
            kind: ProjectKind::Project,
            status: "active".into(),
            readme: std::path::PathBuf::from("/tmp/backup-restore/README.md"),
            branch_prefix: None,
            branch_globs: Vec::new(),
            workstreams: vec![Workstream {
                project: "backup-restore".into(),
                name: "bump-test-timeout".into(),
                origin: Origin::Worktree,
                path: Some(std::path::PathBuf::from("/tmp/w")),
                git,
                merged,
                pr: None,
            }],
        }];
        a.pane = Pane::Workstreams;
        handle_key(&mut a, KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));
        a
    }

    fn body(a: &App) -> String {
        a.confirm.as_ref().expect("confirmation").body.join("\n")
    }

    /// The branch was pushed and merged; the remote deleted it and the prune
    /// took the tracking ref with it. Nothing here is at risk, and saying
    /// "never pushed — proj rm will refuse" was both wrong and a refusal.
    #[test]
    fn a_merged_branch_whose_remote_is_gone_is_not_called_unpushed() {
        let a = app(
            GitState {
                branch: "backup-restore/bump-test-timeout".into(),
                remote_branch: "backup-restore/bump-test-timeout".into(),
                ahead: 1,
                behind: 1,
                upstream: None,
                unpushed: None,
                pushed: true,
                ..Default::default()
            },
            Merged::Pr,
        );
        let body = body(&a);
        assert!(!body.contains("refuse"), "nothing here is refused: {body}");
        assert!(!body.contains("never pushed"), "it was pushed: {body}");
        assert!(body.contains("origin/backup-restore/bump-test-timeout is gone"));
        assert!(body.contains("merged into main"));
    }

    /// A virtual row has no worktree to remove, so `d` goes after the branch
    /// itself -- and says that the remote is going too.
    #[test]
    fn a_branch_with_no_worktree_is_deleted_on_both_sides() {
        let mut a = app(
            GitState {
                branch: "backup-restore/esm-batch-4".into(),
                remote_branch: "brian/esm-batch-4".into(),
                ahead: 2,
                upstream: Some("origin/brian/esm-batch-4".into()),
                unpushed: Some(0),
                pushed: true,
                ..Default::default()
            },
            Merged::No,
        );
        a.projects[0].workstreams[0].path = None;
        a.projects[0].workstreams[0].origin = Origin::OrphanBranch;
        a.confirm = None;
        handle_key(
            &mut a,
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE),
        );
        let c = a.confirm.as_ref().expect("a confirmation");
        assert_eq!(
            c.verb,
            "delete-branch\tbackup-restore/bump-test-timeout\tbackup-restore/esm-batch-4\tbrian/esm-batch-4"
        );
        let body = c.body.join("\n");
        assert!(
            body.contains("also deletes origin/brian/esm-batch-4"),
            "{body}"
        );
        assert!(body.contains("NOT merged"), "{body}");
    }

    /// Nothing is pushed, so the branch is the only copy of those commits.
    #[test]
    fn an_unpushed_branch_with_no_worktree_says_what_is_lost() {
        let mut a = app(
            GitState {
                branch: "backup-restore/scratch".into(),
                remote_branch: "brian/scratch".into(),
                ahead: 3,
                upstream: None,
                unpushed: None,
                pushed: false,
                ..Default::default()
            },
            Merged::No,
        );
        a.projects[0].workstreams[0].path = None;
        a.projects[0].workstreams[0].origin = Origin::OrphanBranch;
        a.confirm = None;
        handle_key(
            &mut a,
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE),
        );
        let body = body(&a);
        assert!(body.contains("3 commit(s) are only here"), "{body}");
        assert!(body.contains("never pushed"), "{body}");
    }

    #[test]
    fn commits_that_exist_only_here_are_still_called_out() {
        let a = app(
            GitState {
                branch: "backup-restore/claim-workspace".into(),
                ahead: 3,
                upstream: None,
                unpushed: None,
                pushed: false,
                ..Default::default()
            },
            Merged::No,
        );
        let body = body(&a);
        assert!(body.contains("3 commit(s) are only here — proj rm will refuse"));
        assert!(body.contains("never pushed to a remote"));
        assert!(body.contains("NOT merged into main"));
    }

    /// `unpushed` counts against the upstream ref, which goes stale: a branch
    /// three commits ahead of main can be 45 ahead of an upstream that has not
    /// been updated since main moved.
    #[test]
    fn the_stranded_count_is_the_branchs_own_commits_not_the_upstream_drift() {
        let a = app(
            GitState {
                branch: "test-noise/act-checks".into(),
                ahead: 3,
                upstream: Some("origin/test-noise-act-checks".into()),
                unpushed: Some(45),
                pushed: true,
                ..Default::default()
            },
            Merged::No,
        );
        assert!(body(&a).contains("3 commit(s) are only here"));
    }

    #[test]
    fn a_fully_pushed_branch_has_nothing_stranded() {
        let a = app(
            GitState {
                branch: "esm-migration/lib-batch-4".into(),
                ahead: 12,
                upstream: Some("origin/brian/esm-lib-batch-4".into()),
                unpushed: Some(0),
                pushed: true,
                ..Default::default()
            },
            Merged::No,
        );
        let body = body(&a);
        assert!(!body.contains("refuse"), "{body}");
    }

    #[test]
    fn uncommitted_changes_are_the_one_thing_that_still_refuses_regardless() {
        let a = app(
            GitState {
                branch: "backup-restore/bump-test-timeout".into(),
                pushed: true,
                dirty: 2,
                ..Default::default()
            },
            Merged::Pr,
        );
        assert!(body(&a).contains("2 uncommitted change(s) — proj rm will refuse"));
    }
}

#[cfg(test)]
mod select_tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn explicit_select_takes_focus() {
        let t = select_target(&args(&["--select", "react-19/react-query"])).expect("target");
        assert_eq!(t.project, "react-19");
        assert_eq!(t.workstream.as_deref(), Some("react-query"));
        assert!(t.focus, "an explicit request moves the cursor");
    }

    #[test]
    fn explicit_select_of_a_bare_project_takes_focus() {
        let t = select_target(&args(&["--select", "react-19"])).expect("target");
        assert_eq!(t.workstream, None);
        assert!(t.focus);
    }

    #[test]
    fn cwd_derived_selection_does_not_take_focus() {
        // Ask from inside a real workstream; the fallback is the cwd.
        let dir = discover::projects_root().join("react-19").join("react-query");
        if !dir.is_dir() {
            return; // nothing to assert against on a machine without it
        }
        std::env::set_current_dir(&dir).unwrap();
        let t = select_target(&args(&[])).expect("target");
        assert_eq!(t.project, "react-19");
        assert!(!t.focus, "standing somewhere is not choosing it");
    }
}

#[cfg(test)]
mod copy_tests {
    use super::*;
    use crossterm::event::{KeyEvent, KeyModifiers};

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn loaded() -> App {
        // Drive the real startup so there is something selected to copy from.
        let mut a = App::new().expect("app");
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while a.loading && std::time::Instant::now() < deadline {
            a.drain();
            std::thread::sleep(Duration::from_millis(10));
        }
        a.pane = Pane::Workstreams;
        a
    }

    #[test]
    fn y_opens_a_menu_of_things_that_apply() {
        let mut a = loaded();
        if a.workstream().is_none() {
            return;
        }
        assert!(!handle_key(&mut a, key('y')));
        let menu = a.copy_menu.as_ref().expect("menu opened");
        let labels: Vec<&str> = menu.items.iter().map(|i| i.label.as_str()).collect();

        // Branch and remote branch exist for every row, materialized or not.
        assert!(labels.contains(&"branch"));
        assert!(labels.contains(&"remote branch"));

        // Path only where there is a worktree; urls only where there is a PR.
        let w = a.workstream().unwrap();
        assert_eq!(labels.contains(&"worktree path"), w.path.is_some());
        assert_eq!(labels.contains(&"PR url"), w.pr.is_some());
        assert!(
            !labels.iter().any(|l| l.contains("checks url")),
            "the /checks tab is GitHub's list of links, not a CI page"
        );

        // Every entry has something to copy.
        assert!(menu.items.iter().all(|i| !i.value.is_empty()));
    }

    #[test]
    fn the_menu_owns_the_keyboard_and_esc_closes_it() {
        let mut a = loaded();
        if a.workstream().is_none() {
            return;
        }
        handle_key(&mut a, key('y'));
        // `q` must not quit the program while the menu is up.
        assert!(!handle_key(&mut a, key('q')));
        assert!(a.copy_menu.is_none(), "q closes the menu");

        handle_key(&mut a, key('y'));
        assert!(!handle_key(
            &mut a,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)
        ));
        assert!(a.copy_menu.is_none());
    }

    #[test]
    fn j_and_k_wrap_around_the_menu() {
        let mut a = loaded();
        if a.workstream().is_none() {
            return;
        }
        handle_key(&mut a, key('y'));
        let n = a.copy_menu.as_ref().unwrap().items.len();
        assert!(n > 1);
        handle_key(&mut a, key('k'));
        assert_eq!(a.copy_menu.as_ref().unwrap().idx, n - 1, "k wraps to the end");
        handle_key(&mut a, key('j'));
        assert_eq!(a.copy_menu.as_ref().unwrap().idx, 0);
    }

    #[test]
    fn an_out_of_range_digit_does_nothing() {
        // A synthetic two-entry menu rather than a real row: a workstream with a
        // PR offers nine or more entries, so there is no out-of-range digit to
        // press against one. The first version of this test asserted against a
        // real row and failed for exactly that reason.
        let mut a = loaded();
        a.copy_menu = Some(CopyMenu::copy(vec![
            app::CopyItem::new("branch", "a/b"),
            app::CopyItem::new("HEAD", "deadbeef"),
        ]));
        assert!(!handle_key(&mut a, key('5')));
        assert!(
            a.copy_menu.is_some(),
            "a digit past the end must not copy the selected entry instead"
        );
        assert_eq!(a.copy_menu.as_ref().unwrap().idx, 0, "and must not move");
    }
}

#[cfg(test)]
mod ci_url_tests {
    use super::*;
    use model::{Check, CheckState, Checks};

    fn checks(c: Vec<Check>) -> Checks {
        Checks {
            state: CheckState::Failure,
            total: c.len() as u32,
            contexts: Some(c),
        }
    }
    fn check(name: &str, failed: bool) -> Check {
        Check {
            name: format!("ci/circleci: {name}"),
            url: format!("https://circleci.com/gh/votingworks/vxsuite/{}", name.len()),
            failed,
        }
    }

    #[test]
    fn ci_url_prefers_the_failing_job() {
        let c = checks(vec![
            check("build", false),
            check("test-admin", true),
            check("test-scan", true),
        ]);
        assert_eq!(c.ci_url().unwrap().name, "ci/circleci: test-admin");
    }

    #[test]
    fn ci_url_falls_back_to_the_first_job_when_all_pass() {
        let c = checks(vec![check("build", false), check("test-admin", false)]);
        assert_eq!(c.ci_url().unwrap().name, "ci/circleci: build");
    }

    #[test]
    fn ci_url_is_absent_until_contexts_are_fetched() {
        let c = Checks {
            state: CheckState::Failure,
            total: 64,
            contexts: None,
        };
        assert!(c.ci_url().is_none());
        assert!(c.failing().is_empty());
    }
}

#[cfg(test)]
mod github_menu_tests {
    use super::*;
    use model::*;

    fn ws(pr: Option<PrInfo>) -> Workstream {
        Workstream {
            project: "p".into(),
            name: "w".into(),
            origin: Origin::Worktree,
            path: Some(std::path::PathBuf::from("/tmp/w")),
            git: GitState { branch: "p/w".into(), remote_branch: "brian/p/w".into(), ..Default::default() },
            merged: Merged::No,
            pr,
        }
    }

    fn pr(state: PrState) -> PrInfo {
        PrInfo {
            number: 9083,
            state,
            url: "https://github.com/votingworks/vxsuite/pull/9083".into(),
            ..Default::default()
        }
    }

    /// Entries that copy must carry no action; entries that act must carry one
    /// whose verb is recognised. A url in `value` was being split on its scheme
    /// colon and reported as an unknown action.
    #[test]
    fn every_entry_is_either_a_copy_or_a_known_action() {
        for state in [PrState::Draft, PrState::Open] {
            for item in CopyMenu::github_for(&ws(Some(pr(state)))) {
                match &item.action {
                    None => assert!(
                        !item.value.is_empty(),
                        "copy entry {:?} has nothing to copy",
                        item.label
                    ),
                    Some(a) => {
                        let verb = a.split_once(':').map(|(v, _)| v).unwrap_or("");
                        assert!(
                            matches!(verb, "ready" | "ready-review" | "review" | "rerun"),
                            "entry {:?} has unknown verb {verb:?}",
                            item.label
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn a_workstream_with_no_pr_offers_the_compare_url() {
        let items = CopyMenu::github_for(&ws(None));
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "open a PR");
        assert!(items[0].value.contains("/compare/main..."));
    }

    #[test]
    fn a_workstream_with_a_pr_offers_its_url_and_not_a_compare_link() {
        let items = CopyMenu::github_for(&ws(Some(pr(PrState::Open))));
        assert_eq!(items[0].label, "PR url");
        assert!(items.iter().all(|i| !i.value.contains("/compare/")));
        assert_eq!(items.iter().filter(|i| i.label == "PR url").count(), 1);
    }
}
