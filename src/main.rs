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
        app.pending_select = select_target(&args);

        // --loading draws the pre-scan state, which is otherwise only on screen
        // for the few hundred milliseconds the background scan takes.
        if !args.iter().any(|a| a == "--loading") {
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            while app.loading && std::time::Instant::now() < deadline {
                app.drain();
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        // Wait for the first network refresh too, not just the scan. With a warm
        // cache the render is right either way; with a cold one it would show a
        // dashboard with no PR state at all and no way to tell that apart from
        // there being none.
        {
            let deadline = std::time::Instant::now() + Duration::from_secs(60);
            while app.fetched_at.is_none() && std::time::Instant::now() < deadline {
                app.drain();
                std::thread::sleep(Duration::from_millis(50));
            }
        }

        // The event loop asks for the selected row's check contexts every
        // frame; do the same here, or --render shows a menu missing its CI entry
        // and looks like a bug in the menu rather than in the harness.
        app.request_contexts();
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        while std::time::Instant::now() < deadline
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
            for c in k.trim_start_matches('=').chars() {
                handle_key(
                    &mut app,
                    crossterm::event::KeyEvent::new(
                        KeyCode::Char(c),
                        crossterm::event::KeyModifiers::NONE,
                    ),
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
    app.pending_select = select_target(&args);

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

        // Navigate and launch.
        KeyCode::Char('g') => {
            if let Some(p) = require_path(app) {
                if emit(app, format!("lazygit\t{p}")) {
                    return true;
                }
            }
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
        KeyCode::Char('p') => {
            let remote = app.workstream().map(|w| w.git.remote_branch.clone());
            if let (Some(p), Some(remote)) = (require_path(app), remote) {
                if emit(app, format!("push\t{p}\t{remote}")) {
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

/// The selected workstream's path, or a note in the footer saying why there
/// isn't one. Every action below needs a worktree; a virtual row has none.
fn require_path(app: &mut App) -> Option<String> {
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
    let Some(w) = app.workstream() else {
        return false;
    };
    app.action = Some(match &w.path {
        Some(p) => format!("cd\t{}", p.display()),
        None => format!("new\t{}\t{}", w.qualified(), w.git.branch),
    });
    true
}

/// Ask before removing a workstream, and say what is at stake.
///
/// `proj rm` refuses on uncommitted or unpushed work anyway, so this is not the
/// safety net -- it is the part that tells you *which* workstream you are about
/// to remove, before the shell scrolls past with an answer.
fn confirm_delete(app: &mut App) {
    let Some(w) = app.workstream() else { return };
    if w.is_virtual() {
        app.flash("nothing to delete — this branch has no worktree");
        return;
    }

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
    if w.git.unpushed.is_some_and(|n| n > 0) {
        body.push(format!(
            "{} unpushed commit(s) — proj rm will refuse",
            w.git.unpushed.unwrap()
        ));
    }
    if w.git.upstream.is_none() {
        body.push("never pushed — proj rm will refuse".to_string());
    }

    app.confirm = Some(Confirm {
        title: format!(" delete {qualified} "),
        body,
        verb: format!("delete\t{qualified}"),
    });
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
        a.copy_menu = Some(CopyMenu {
            items: vec![
                app::CopyItem::new("branch", "a/b"),
                app::CopyItem::new("HEAD", "deadbeef"),
            ],
            idx: 0,
        });
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
