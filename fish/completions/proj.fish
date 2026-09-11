# Completions for proj (project worktree manager)
#
# The candidate lists come from `proj __projects` / `proj __workstreams` /
# `proj __branches` so this file stays in sync with the function itself.

function __proj_needs_command
    set -l cmd (commandline -opc)
    test (count $cmd) -eq 1
end

function __proj_using_command_any
    set -l cmd (commandline -opc)
    if test (count $cmd) -lt 2
        return 1
    end
    for subcmd in $argv
        if test "$cmd[2]" = "$subcmd"
            return 0
        end
    end
    return 1
end

# Subcommands
complete -c proj -f
complete -c proj -n __proj_needs_command -a new -d "Create a workstream worktree"
complete -c proj -n __proj_needs_command -a add -d "Create a workstream worktree"
complete -c proj -n __proj_needs_command -a rm -d "Remove a workstream"
complete -c proj -n __proj_needs_command -a remove -d "Remove a workstream"
complete -c proj -n __proj_needs_command -a ls -d "List projects and workstreams"
complete -c proj -n __proj_needs_command -a list -d "List projects and workstreams"
complete -c proj -n __proj_needs_command -a cd -d "Change to a project or workstream"
complete -c proj -n __proj_needs_command -a status -d "Show workstream branches and drift"
complete -c proj -n __proj_needs_command -a st -d "Show workstream branches and drift"
complete -c proj -n __proj_needs_command -a daemon -d "The shared fetcher: status or stop"
complete -c proj -n __proj_needs_command -a help -d "Show help"

complete -c proj -n '__proj_using_command_any daemon' -a status -d "What the running daemon is doing"
complete -c proj -n '__proj_using_command_any daemon' -a stop -d "Ask it to exit now"

# new — the argument is a project the workstream will be created under, so
# complete projects with a trailing slash rather than existing workstreams.
complete -c proj -n '__proj_using_command_any new add' -a '(proj __projects)'
complete -c proj -n '__proj_using_command_any new add' -l branch -s b -d "Base branch for a new branch (default: main)" -x -a '(proj __branches)'
complete -c proj -n '__proj_using_command_any new add' -l branch-name -s n -d "Branch name (default: from the project's branch-prefix)" -x -a '(proj __branches)'
complete -c proj -n '__proj_using_command_any new add' -l rebase -d "Rebase on main before building"

# rm/remove — existing workstreams only
complete -c proj -n '__proj_using_command_any rm remove' -a '(proj __workstreams)'
complete -c proj -n '__proj_using_command_any rm remove' -l force -s f -d "Force removal even with changes"

# cd — workstreams first, then bare projects, plus the previous-location target
complete -c proj -n '__proj_using_command_any cd' -a '(proj __workstreams)'
complete -c proj -n '__proj_using_command_any cd' -a '(proj __projects)'
complete -c proj -n '__proj_using_command_any cd' -a - -d "Previous location"
complete -c proj -n '__proj_using_command_any cd' -l rebase -d "Rebase on main and rebuild after cd'ing"

# status — optionally scoped to one project
complete -c proj -n '__proj_using_command_any status st' -a '(proj __projects)'
