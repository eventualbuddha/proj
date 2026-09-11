# proj

Manage the project worktrees under `~/projects`, and show what is going on in
them: workstreams, PR and CI state, reviews waiting on you, and the branches
that have no worktree at all.

The layout is two levels — `~/projects/<project>/<workstream>` — where a project
is an initiative with a `README.md` and a workstream is one unit of review,
usually one branch and one PR. A workstream directory *is* the worktree.

## Parts

| Path | What it is |
| --- | --- |
| `src/` | `proj-tui`, the dashboard (Rust, ratatui). Shells out to `git` and `gh`. |
| `fish/proj.fish` | The `proj` command. Bare `proj` opens the dashboard. |
| `fish/completions/proj.fish` | Completions, fed by `proj __projects` / `__workstreams` / `__branches`. |
| `fish/conf.d/30-proj-cargo.fish` | Per-project `CARGO_TARGET_DIR`, driven off `PWD`. |
| `bin/proj-prompt.sh` | The starship segment, as a plain `sh` script. |

The TUI is installed as `proj-tui`, not `proj`: `proj` is the fish function that
wraps it, because a process cannot change its parent shell's directory and the
whole point of the ↵ key is that it does.

## Install

With home-manager, as a flake input:

```nix
inputs.proj.url = "github:…/proj";

# in your home configuration
imports = [ inputs.proj.homeModules.default ];
```

Or by path, without adding an input — which needs `--impure`, since a pure
flake evaluation cannot see outside the flake:

```nix
imports = [ /home/you/code/proj/nix/home-manager.nix ];
```

The module installs both binaries, the fish function, the completions and the
`conf.d` snippet, and defines a `custom.proj` starship segment. Putting that
segment in the prompt is still the host's job: splice `${custom.proj}` into
starship's `format`.

## Develop

```sh
nix develop      # cargo, rustc, clippy, rust-analyzer, git, gh
cargo test
cargo run --bin proj-tui
```

`PROJECTS_ROOT` overrides `~/projects` — the tests use it, and so can you.
