# Per-project CARGO_TARGET_DIR.
#
# A vxsuite worktree is 8.6 GB, and 7.2 GB of that is Rust build output --
# libs/ballot-interpreter/target (3.5 G), libs/pdi-scanner/target (2.8 G) and the
# workspace target/ (950 M). With one worktree per branch that ceiling arrives
# fast, so workstreams within a project share one target directory. Branches in a
# project are usually close, so most artifacts stay valid across a switch, and
# cargo's own lock serialises the rare concurrent build.
#
# Per project rather than one global directory: two unrelated projects sit on
# distant commits and would keep invalidating each other. sccache is the answer
# for cross-project reuse -- see ~/projects/README.md.
#
# Driven off PWD rather than set by `proj cd`, so a plain `cd`, a zoxide jump, or
# an editor's own shell all get it right. Load order is after the vendor tools
# that set the rest of the Rust environment.

function __proj_cargo_target --on-variable PWD --description "Point CARGO_TARGET_DIR at the current project's shared target dir"
    set -l root ~/projects
    set -l here (pwd -P 2>/dev/null; or pwd)

    if not string match -q "$root/*" -- "$here"
        # Only unset what we set. A CARGO_TARGET_DIR from elsewhere is not ours
        # to clear.
        if set -q __proj_cargo_target_set
            set -e CARGO_TARGET_DIR
            set -e __proj_cargo_target_set
        end
        return
    end

    set -l project (string split / -- (string replace -- "$root/" "" "$here"))[1]

    if not test -f "$root/$project/README.md"
        return
    end

    set -gx CARGO_TARGET_DIR ~/.cache/proj/cargo-target/$project
    set -g __proj_cargo_target_set 1
end

# conf.d runs before the first prompt, so seed it for the shell's starting
# directory rather than waiting for the first cd.
__proj_cargo_target
