# proj - manage project worktrees under ~/projects
#
# Replaces `wt`, which modelled one worktree per branch, flat, under ~/code.
# The layout here is two levels -- ~/projects/<project>/<workstream> -- where a
# project is an initiative with a README.md and a workstream is one unit of
# review, usually one branch and one PR. See ~/projects/README.md for the full
# design; this file is the phase-1 shell half of it.
#
# A workstream directory IS the worktree. The rare cross-repo workstream (a
# directory of per-repo worktrees instead) is not handled here -- it is phase 4,
# and nothing below assumes it will not arrive.

function proj --description "Manage project worktrees under ~/projects"
    # Bare `proj` opens the dashboard. It writes what you chose to a temp file on
    # exit and this acts on it -- a child process cannot cd its parent shell, nor
    # run a build in it, so the file is how ↵ reaches back out here.
    if test (count $argv) -eq 0
        __proj_run_tui
        return $status
    end

    set -l cmd $argv[1]
    set -e argv[1]

    switch $cmd
        case new add
            __proj_new $argv

        case rm remove
            __proj_remove $argv

        case ls list
            __proj_list

        case cd
            __proj_cd $argv

        case status st
            __proj_status $argv

        case tui
            __proj_run_tui $argv
            return $status

        case dump
            proj-tui --dump $argv

        case prompt
            __proj_prompt

        case help -h --help
            __proj_help

            # Private helpers for completions
        case __projects
            __proj_complete_projects

        case __workstreams
            __proj_complete_workstreams

        case __branches
            __proj_complete_branches

        case '*'
            echo "proj: unknown command '$cmd'" >&2
            return 1
    end
end

function __proj_help
    echo "proj - manage project worktrees under ~/projects"
    echo ""
    echo "Usage: proj <command> [args]"
    echo ""
    echo "Commands:"
    echo "  new <project>/<workstream> [-b <base>]  Create a worktree for a workstream"
    echo "                                          The branch defaults to <project>/<workstream>"
    echo "                                          prefixed per the project's README frontmatter;"
    echo "                                          override with --branch-name"
    echo "                                          Runs pnpm install and pnpm build"
    echo "                                          With --rebase, rebase on main before building"
    echo "  rm|remove [<project>/<workstream>]      Remove a workstream (defaults to the current one)"
    echo "                                          Fails on uncommitted or unpushed commits"
    echo "                                          Also deletes the associated branch"
    echo "  cd [<project>[/<workstream>]]           cd to a project or workstream"
    echo "                                            cd              -> ~/projects"
    echo "                                            cd -            -> previous location"
    echo "                                            cd <project>    -> the project directory"
    echo "                                            cd <p>/<w>      -> that workstream"
    echo "                                            cd <workstream> -> if unambiguous across projects"
    echo "                                          With --rebase, rebase on main and rebuild after"
    echo "  ls|list                                 List projects and their workstreams"
    echo "  (no args)                               Open the dashboard: PRs, CI, merged-ness, virtual rows"
    echo "                                          Opens on the workstream you are standing in, if any"
    echo "  tui [--select <project>]                Same, opened on a project"
    echo "  dump                                    Print the dashboard's state as text"
    echo "  status|st [<project>]                   Show each workstream's branch, drift and dirtiness"
    echo "  prompt                                  Print the prompt segment for \$PWD (used by starship)"
    echo "  help                                    Show this help"
    echo ""
    echo "Branches are named <project>/<workstream> locally and brian/<project>/<workstream>"
    echo "on the remote, so a branch name and a directory path say the same thing."
end

# ---------------------------------------------------------------------------
# The dashboard
# ---------------------------------------------------------------------------

function __proj_run_tui --description "Run the dashboard, act on what it asks for, and come back"
    # A loop, not a single run. Most actions -- lazygit, an editor, a rebase, a
    # push -- are things you do *while* looking at the dashboard, so the natural
    # end of one is being back at it. Only `cd` and quitting leave.
    while true
        set -l file (mktemp)
        proj-tui --cd-file "$file" $argv
        set -l st $status

        if not test -s "$file"
            # No verb: the user quit.
            rm -f "$file"
            return $st
        end

        set -l parts (string split \t -- (cat "$file"))
        rm -f "$file"

        # Where the next run should open, so acting on a row does not send the
        # selection back to the top of the list. The second field is a worktree
        # path for some verbs and <project>/<workstream> for others, so strip the
        # root before taking the first component -- splitting an absolute path on
        # "/" yields an empty first element and reopens on nothing.
        set -l reopen
        if test (count $parts) -ge 2
            set -l ref "$parts[2]"
            set -l root (__proj_root)
            if string match -q "$root/*" -- "$ref"
                set ref (string replace -- "$root/" "" "$ref")
            end
            # The whole <project>/<workstream>, not just the project: coming back
            # from a rebase should land on the row you rebased.
            if test -n "$ref"
                set reopen --select "$ref"
            end
        end

        # Whether to pause before the full-screen redraw. A full-screen program
        # already owned the terminal and left nothing to read; a build, a rebase
        # or a push leaves output that the next frame would wipe.
        set -l pause 1

        switch $parts[1]
            case cd
                __proj_goto "$parts[2]"
                return $status

            case new
                echo "Creating worktree for '$parts[2]' on branch '$parts[3]'..."
                __proj_new "$parts[2]" --branch-name "$parts[3]"

            case edit
                $EDITOR "$parts[2]"
                set pause 0

            case lazygit
                lazygit --path "$parts[2]"
                set pause 0

            case rebase
                __proj_rebase_and_build "$parts[2]"

            case push
                __proj_push "$parts[2]" "$parts[3]"

            case delete
                __proj_remove "$parts[2]"
                # The row is gone, so reopening on it would land nowhere.
                set reopen

            case '*'
                echo "proj: the dashboard asked for something unknown: $parts[1]" >&2
                return 1
        end

        # `_` is read-only in fish -- it holds the name of the running command --
        # so it cannot be a `read` target, and naming it that made this line
        # fail every time it was reached.
        if test $pause -eq 1
            echo ""
            read -P "Press enter to return to proj (or ctrl-d to stay here) " -l __proj_ack
            or return 0
        end

        set argv $reopen
    end
end

function __proj_push --description "Push WORKTREE's branch to its remote name, setting upstream"
    set -l wt_path "$argv[1]"
    set -l remote_branch "$argv[2]"

    set -l branch (git -C "$wt_path" rev-parse --abbrev-ref HEAD 2>/dev/null)
    if test -z "$branch"; or test "$branch" = HEAD
        echo "proj push: refusing to push a detached HEAD" >&2
        return 1
    end

    # An explicit refspec, because local and remote names differ by design:
    # `<project>/<workstream>` here, `brian/<project>/<workstream>` there. A bare
    # `git push` with push.autoSetupRemote would create a same-named remote
    # branch and quietly bypass the whole convention.
    echo "Pushing '$branch' to origin/$remote_branch..."
    git -C "$wt_path" push origin "$branch:refs/heads/$remote_branch"
    or return 1

    git -C "$wt_path" branch -u "origin/$remote_branch" "$branch" >/dev/null 2>&1
    echo "Upstream set to origin/$remote_branch"
end

# ---------------------------------------------------------------------------
# Paths and configuration
# ---------------------------------------------------------------------------

# The canonical clone every worktree is added from. Single-repo for now; phase 4
# turns this into a lookup keyed by the repo directory name.
function __proj_repo --description "Print the canonical clone for a repo (default vxsuite)"
    set -l repo $argv[1]
    test -n "$repo"; or set repo vxsuite
    echo ~/code/$repo
end

function __proj_root --description "Print the projects root"
    echo ~/projects
end

function __proj_slug --description "Turn a name into a directory-safe slug"
    string replace -a / - -- "$argv[1]"
end

# ---------------------------------------------------------------------------
# Project metadata (YAML frontmatter in each project's README.md)
# ---------------------------------------------------------------------------
#
# Deliberately a line-oriented reader for a handful of scalar keys rather than a
# YAML parser: the frontmatter is written by hand to a schema documented in
# ~/projects/README.md, and dragging in a parser to read six keys would make the
# prompt path slower for no gain. Anything structured (repos, branches) is left
# to the TUI, which has a real parser.

function __proj_meta --description "Print the value of KEY from PROJECT's README frontmatter"
    set -l project $argv[1]
    set -l key $argv[2]
    set -l readme (__proj_root)/$project/README.md

    test -f "$readme"; or return 1

    # Only look inside the leading `---` fenced block; a `name:` in the prose
    # below it is not metadata.
    awk -v key="$key" '
        NR == 1 && $0 != "---" { exit }
        NR == 1 { next }
        $0 == "---" { exit }
        {
            eq = index($0, ":")
            if (eq == 0) next
            k = substr($0, 1, eq - 1)
            gsub(/^[ \t]+|[ \t]+$/, "", k)
            if (k != key) next
            v = substr($0, eq + 1)
            gsub(/^[ \t]+|[ \t]+$/, "", v)
            gsub(/^"|"$/, "", v)
            print v
            exit
        }
    ' "$readme"
end

function __proj_emoji --description "Print PROJECT's emoji, defaulting to a folder"
    set -l emoji (__proj_meta "$argv[1]" emoji)
    if test -z "$emoji"
        set emoji 📁
    end
    echo $emoji
end

# ---------------------------------------------------------------------------
# Discovery
# ---------------------------------------------------------------------------

function __proj_projects --description "Print every project name"
    set -l root (__proj_root)
    test -d "$root"; or return 0

    for dir in $root/*/
        set -l name (string trim -r -c / -- (string replace -- "$root/" "" "$dir"))
        # A project is a directory with a README.md. Anything else under the
        # root is not ours to list.
        if test -f "$root/$name/README.md"
            echo $name
        end
    end
end

function __proj_workstreams --description "Print 'project<TAB>workstream<TAB>path' for PROJECT, or all"
    set -l root (__proj_root)
    set -l only "$argv[1]"

    for project in (__proj_projects)
        if test -n "$only"; and test "$project" != "$only"
            continue
        end

        for dir in $root/$project/*/
            set -l path (string trim -r -c / -- "$dir")
            # Workstreams are worktrees; a plain directory under a project is
            # not one (and in phase 4 would be a cross-repo container).
            test -e "$path/.git"; or continue
            printf '%s\t%s\t%s\n' "$project" (string replace -r '.*/' '' -- "$path") "$path"
        end
    end
end

function __proj_containing --description "Print 'project<TAB>workstream<TAB>path' for the workstream containing DIR"
    set -l dir (realpath "$argv[1]" 2>/dev/null; or echo "$argv[1]")

    for entry in (__proj_workstreams)
        set -l parts (string split \t -- "$entry")
        set -l path (realpath "$parts[3]" 2>/dev/null; or echo "$parts[3]")

        if test "$dir" = "$path"; or string match -q "$path/*" -- "$dir"
            printf '%s\t%s\t%s\n' "$parts[1]" "$parts[2]" "$path"
            return 0
        end
    end

    return 1
end

function __proj_find --description "Resolve <project>/<workstream> or a bare workstream to 'project<TAB>workstream<TAB>path'"
    set -l query "$argv[1]"
    test -n "$query"; or return 1

    set -l matches

    for entry in (__proj_workstreams)
        set -l parts (string split \t -- "$entry")
        set -l qualified "$parts[1]/$parts[2]"

        if test "$query" = "$qualified"
            printf '%s\n' "$entry"
            return 0
        end

        if test "$query" = "$parts[2]"
            set -a matches "$entry"
        end
    end

    if test (count $matches) -eq 1
        printf '%s\n' "$matches[1]"
        return 0
    end

    if test (count $matches) -gt 1
        echo "proj: '$query' is ambiguous:" >&2
        for m in $matches
            set -l parts (string split \t -- "$m")
            echo "  $parts[1]/$parts[2]" >&2
        end
        return 2
    end

    return 1
end

# ---------------------------------------------------------------------------
# Git helpers
# ---------------------------------------------------------------------------

function __proj_local_branch --description "Succeed if BRANCH exists locally"
    git -C (__proj_repo) show-ref --verify --quiet "refs/heads/$argv[1]"
end

function __proj_remote_ref --description "Print <remote>/<branch> if an already-fetched remote branch exists"
    set -l branch "$argv[1]"

    for remote in (git -C (__proj_repo) remote)
        if git -C (__proj_repo) show-ref --verify --quiet "refs/remotes/$remote/$branch"
            echo "$remote/$branch"
            return 0
        end
    end

    return 1
end

function __proj_fetch_branch --description "Look for BRANCH on the remotes, fetch it, print <remote>/<branch>"
    set -l branch "$argv[1]"

    for remote in (git -C (__proj_repo) remote)
        if git -C (__proj_repo) ls-remote --exit-code --heads "$remote" "refs/heads/$branch" >/dev/null 2>&1
            echo "Fetching '$branch' from $remote..." >&2
            if git -C (__proj_repo) fetch --quiet "$remote" "+refs/heads/$branch:refs/remotes/$remote/$branch"
                echo "$remote/$branch"
                return 0
            end
        end
    end

    return 1
end

function __proj_resolve_branch --description "Print <branch> or <remote>/<branch> if it exists anywhere, fetching if needed"
    set -l branch "$argv[1]"

    if __proj_local_branch "$branch"
        echo "$branch"
        return 0
    end

    set -l remote_ref (__proj_remote_ref "$branch")
    if test -z "$remote_ref"
        set remote_ref (__proj_fetch_branch "$branch")
    end

    if test -n "$remote_ref"
        echo "$remote_ref"
        return 0
    end

    return 1
end

function __proj_rebase_main --description "Rebase the worktree at PATH onto the latest main"
    set -l wt_path "$argv[1]"

    set -l branch (git -C "$wt_path" rev-parse --abbrev-ref HEAD 2>/dev/null)
    if test -z "$branch"; or test "$branch" = HEAD
        echo "proj: refusing to rebase the detached HEAD in $wt_path" >&2
        return 1
    end

    # Prefer the remote's main so the rebase picks up commits the local main
    # has not been updated to yet.
    set -l onto (__proj_fetch_branch main)
    if test -z "$onto"
        set onto (__proj_remote_ref main)
    end
    if test -z "$onto"; and __proj_local_branch main
        set onto main
    end
    if test -z "$onto"
        echo "proj: no main branch to rebase onto" >&2
        return 1
    end

    echo "Rebasing '$branch' onto $onto..."
    git -C "$wt_path" rebase "$onto"
end

function __proj_build --description "Install dependencies and build the worktree in the current directory"
    pnpm install
    and pnpm build
end

function __proj_rebase_and_build --description "Rebase the worktree at PATH onto main and build it; PATH must be the current directory"
    __proj_rebase_main "$argv[1]"
    or begin
        echo "proj: rebase failed, skipping the build" >&2
        return 1
    end

    __proj_build
end

function __proj_goto --description "cd to a path, remembering where we came from"
    set -g __proj_last_dir (pwd)
    cd "$argv[1]"
end

# ---------------------------------------------------------------------------
# Commands
# ---------------------------------------------------------------------------

function __proj_new
    set -l root (__proj_root)
    set -l target ""
    set -l base_branch ""
    set -l branch_name ""
    set -l rebase 0

    set -l i 1
    while test $i -le (count $argv)
        switch $argv[$i]
            case --rebase
                set rebase 1
            case --branch -b
                set i (math $i + 1)
                if test $i -le (count $argv)
                    set base_branch $argv[$i]
                else
                    echo "proj new: --branch requires an argument" >&2
                    return 1
                end
            case --branch-name -n
                set i (math $i + 1)
                if test $i -le (count $argv)
                    set branch_name $argv[$i]
                else
                    echo "proj new: --branch-name requires an argument" >&2
                    return 1
                end
            case '-*'
                echo "proj new: unknown option '$argv[$i]'" >&2
                return 1
            case '*'
                if test -z "$target"
                    set target $argv[$i]
                else
                    echo "proj new: unexpected argument '$argv[$i]'" >&2
                    return 1
                end
        end
        set i (math $i + 1)
    end

    if test -z "$target"
        echo "proj new: <project>/<workstream> required" >&2
        return 1
    end

    set -l parts (string split -m1 / -- "$target")
    if test (count $parts) -ne 2; or test -z "$parts[1]"; or test -z "$parts[2]"
        echo "proj new: expected <project>/<workstream>, got '$target'" >&2
        return 1
    end
    set -l project $parts[1]
    set -l workstream $parts[2]

    if not test -f "$root/$project/README.md"
        echo "proj new: no project '$project' (expected $root/$project/README.md)" >&2
        return 1
    end

    set -l wt_path "$root/$project/$workstream"
    if test -e "$wt_path"
        echo "proj new: $wt_path already exists" >&2
        return 1
    end

    # The branch name IS the workstream's identity: `<project>/<workstream>`
    # locally, `brian/<project>/<workstream>` on the remote. Nothing to configure
    # and nothing to guess -- a path and a branch name carry the same
    # information, in both directions.
    if test -z "$branch_name"
        set branch_name "$project/$workstream"
    end

    set -l git_args
    if __proj_local_branch "$branch_name"
        if test -n "$base_branch"
            echo "proj new: branch '$branch_name' already exists, ignoring --branch $base_branch" >&2
        end
        echo "Checking out existing branch '$branch_name' at $wt_path..."
        set git_args "$wt_path" "$branch_name"
    else
        set -l remote_ref (__proj_remote_ref "$branch_name")
        if test -z "$remote_ref"
            set remote_ref (__proj_fetch_branch "$branch_name")
        end

        if test -n "$remote_ref"
            if test -n "$base_branch"
                echo "proj new: branch '$branch_name' already exists on the remote, ignoring --branch $base_branch" >&2
            end
            echo "Checking out remote branch '$remote_ref' at $wt_path..."
            set git_args --track -b "$branch_name" "$wt_path" "$remote_ref"
        else
            set -l base "$base_branch"
            test -n "$base"; or set base main

            set -l start (__proj_resolve_branch "$base")
            if test -z "$start"
                echo "proj new: base branch '$base' not found locally or on any remote" >&2
                return 1
            end

            echo "Creating branch '$branch_name' from $start at $wt_path..."
            set git_args -b "$branch_name" "$wt_path" "$start"
        end
    end

    git -C (__proj_repo) worktree add $git_args
    or begin
        echo "proj new: failed to create worktree" >&2
        return 1
    end

    echo "Setting up workstream..."
    __proj_goto "$wt_path"
    or return 1

    if test $rebase -eq 1
        __proj_rebase_main "$wt_path"
        or begin
            echo "proj new: rebase failed, skipping the build (worktree still created)" >&2
            return 1
        end
    end

    __proj_build
    or begin
        echo "proj new: setup commands had errors (worktree still created)" >&2
        return 1
    end

    echo ""
    echo "Workstream '$project/$workstream' ready at $wt_path"
end

function __proj_remove
    set -l force 0
    set -l target ""

    for arg in $argv
        switch $arg
            case --force -f
                set force 1
            case '-*'
                echo "proj rm: unknown option '$arg'" >&2
                return 1
            case '*'
                if test -z "$target"
                    set target $arg
                else
                    echo "proj rm: unexpected argument '$arg'" >&2
                    return 1
                end
        end
    end

    set -l entry ""
    if test -n "$target"
        set entry (__proj_find "$target")
        or return 1
        if test -z "$entry"
            echo "proj rm: no workstream matching '$target'" >&2
            return 1
        end
    else
        set entry (__proj_containing (pwd))
        if test -z "$entry"
            echo "proj rm: '"(pwd)"' is not inside a workstream" >&2
            return 1
        end
    end

    set -l parts (string split \t -- "$entry")
    set -l project $parts[1]
    set -l workstream $parts[2]
    set -l wt_path (realpath "$parts[3]" 2>/dev/null; or echo "$parts[3]")

    set -l branch (git -C "$wt_path" rev-parse --abbrev-ref HEAD 2>/dev/null)

    if test $force -eq 0
        set -l dirty (git -C "$wt_path" status --porcelain 2>/dev/null)
        if test -n "$dirty"
            echo "proj rm: workstream has uncommitted changes:" >&2
            git -C "$wt_path" status --short >&2
            echo "" >&2
            echo "Use --force to remove anyway" >&2
            return 1
        end

        # Unpushed commits. Fall back to the default branch when there is no
        # upstream, so an unpushed branch is not silently dropped.
        if test -n "$branch"; and test "$branch" != HEAD
            set -l base '@{upstream}'
            if not git -C "$wt_path" rev-parse --verify --quiet '@{upstream}' >/dev/null 2>&1
                set base (__proj_remote_ref main)
                test -n "$base"; or set base main
            end

            set -l unpushed (git -C "$wt_path" log --oneline "$base..HEAD" 2>/dev/null)
            if test -n "$unpushed"
                echo "proj rm: '$branch' has commits not in $base:" >&2
                git -C "$wt_path" log --oneline "$base..HEAD" >&2
                echo "" >&2
                echo "Use --force to remove anyway" >&2
                return 1
            end
        end
    end

    echo "Removing workstream '$project/$workstream'..."

    set -l here (pwd)
    if test "$here" = "$wt_path"; or string match -q "$wt_path/*" "$here"
        __proj_goto (__proj_root)/$project
        echo "Changed directory to "(__proj_root)/$project
    end

    git -C (__proj_repo) worktree remove "$wt_path" --force
    or begin
        echo "proj rm: failed to remove worktree" >&2
        return 1
    end

    if test -n "$branch"; and test "$branch" != HEAD
        set -l delete_flag -d
        test $force -eq 0; or set delete_flag -D

        if git -C (__proj_repo) branch $delete_flag "$branch" >/dev/null 2>&1
            echo "Deleted branch '$branch'"
        else
            echo "Kept branch '$branch' (not fully merged; delete with: git -C "(__proj_repo)" branch -D $branch)"
        end
    end

    echo "Workstream '$project/$workstream' removed"
end

function __proj_list
    set -l root (__proj_root)

    for project in (__proj_projects)
        set -l emoji (__proj_emoji "$project")
        set -l name (__proj_meta "$project" name)
        test -n "$name"; or set name "$project"

        echo "$emoji $project — $name"

        set -l any 0
        for entry in (__proj_workstreams "$project")
            set -l parts (string split \t -- "$entry")
            set -l branch (git -C "$parts[3]" rev-parse --abbrev-ref HEAD 2>/dev/null)
            printf '    %-28s %s\n' "$parts[2]" "$branch"
            set any 1
        end

        if test $any -eq 0
            echo "    (no workstreams)"
        end
    end
end

function __proj_status
    set -l only "$argv[1]"

    for entry in (__proj_workstreams "$only")
        set -l parts (string split \t -- "$entry")
        set -l project $parts[1]
        set -l workstream $parts[2]
        set -l path $parts[3]

        set -l branch (git -C "$path" rev-parse --abbrev-ref HEAD 2>/dev/null)
        if test -z "$branch"; or test "$branch" = HEAD
            set branch (git -C "$path" rev-parse --short HEAD 2>/dev/null)" (detached)"
        end

        # Drift against the remote's main, which is what "behind" actually
        # means day to day -- a stale local main would understate it.
        set -l base (__proj_remote_ref main)
        test -n "$base"; or set base main

        set -l drift ""
        set -l counts (git -C "$path" rev-list --left-right --count "$base...HEAD" 2>/dev/null)
        if test -n "$counts"
            set -l lr (string split \t -- "$counts")
            set drift "+$lr[2] -$lr[1]"
        end

        set -l dirty ""
        set -l changes (git -C "$path" status --porcelain 2>/dev/null | wc -l | string trim)
        if test "$changes" != 0
            set dirty "$changes dirty"
        end

        printf '%s %-34s %-46s %-10s %s\n' (__proj_emoji "$project") "$project/$workstream" "$branch" "$drift" "$dirty"
    end
end

function __proj_cd
    set -l root (__proj_root)
    set -l rebase 0
    set -l target ""

    for arg in $argv
        switch $arg
            case --rebase
                set rebase 1
            case -
                if test -z "$target"
                    set target $arg
                else
                    echo "proj cd: unexpected argument '$arg'" >&2
                    return 1
                end
            case '-*'
                echo "proj cd: unknown option '$arg'" >&2
                return 1
            case '*'
                if test -z "$target"
                    set target $arg
                else
                    echo "proj cd: unexpected argument '$arg'" >&2
                    return 1
                end
        end
    end

    if test -z "$target"
        __proj_goto "$root"
        return $status
    end

    if test "$target" = -
        if not set -q __proj_last_dir
            echo "proj cd: no previous location" >&2
            return 1
        end
        set -l prev $__proj_last_dir
        set -g __proj_last_dir (pwd)
        cd "$prev"
        or return 1

        if test $rebase -eq 1
            set -l entry (__proj_containing (pwd))
            if test -z "$entry"
                echo "proj cd: '"(pwd)"' is not inside a workstream, skipping the rebase" >&2
                return 1
            end
            set -l parts (string split \t -- "$entry")
            __proj_rebase_and_build "$parts[3]"
            return $status
        end

        return 0
    end

    # A workstream, qualified or bare.
    set -l entry (__proj_find "$target")
    set -l find_status $status
    if test $find_status -eq 2
        return 1
    end

    if test -n "$entry"
        set -l parts (string split \t -- "$entry")
        __proj_goto "$parts[3]"
        or return 1

        if test $rebase -eq 1
            __proj_rebase_and_build "$parts[3]"
            return $status
        end

        return 0
    end

    # Failing that, a project directory.
    if test -f "$root/$target/README.md"
        __proj_goto "$root/$target"
        return $status
    end

    echo "proj cd: no workstream or project matching '$target'" >&2
    echo "" >&2
    __proj_list >&2
    return 1
end

# ---------------------------------------------------------------------------
# Prompt
# ---------------------------------------------------------------------------
#
# Delegates to the `proj-prompt` binary rather than reimplementing it here.
# starship calls that script directly on every prompt -- a fish function would
# mean spawning fish each time -- so this subcommand exists only so `proj prompt`
# does what you would expect from the shell.

function __proj_prompt --description "Print the prompt segment for the current directory"
    proj-prompt
end

# ---------------------------------------------------------------------------
# Completion helpers (invoked as `proj __projects` etc.)
# ---------------------------------------------------------------------------

function __proj_complete_projects --description "Print 'project<TAB>description' for every project"
    for project in (__proj_projects)
        set -l name (__proj_meta "$project" name)
        test -n "$name"; or set name project
        printf '%s\t%s\n' "$project" "$name"
    end
end

function __proj_complete_workstreams --description "Print 'project/workstream<TAB>branch' for every workstream"
    for entry in (__proj_workstreams)
        set -l parts (string split \t -- "$entry")
        set -l branch (git -C "$parts[3]" rev-parse --abbrev-ref HEAD 2>/dev/null)
        printf '%s/%s\t%s\n' "$parts[1]" "$parts[2]" "$branch"
    end
end

function __proj_complete_branches --description "Print 'branch<TAB>description' for local and remote branches"
    set -l seen

    for branch in (git -C (__proj_repo) for-each-ref --format='%(refname:short)' refs/heads 2>/dev/null)
        set -a seen "$branch"
        printf '%s\t%s\n' "$branch" "local branch"
    end

    for remote in (git -C (__proj_repo) remote)
        for ref in (git -C (__proj_repo) for-each-ref --format='%(refname:short)' "refs/remotes/$remote" 2>/dev/null)
            set -l branch (string replace -- "$remote/" "" "$ref")
            test "$branch" != HEAD; or continue
            contains -- "$branch" $seen; and continue
            set -a seen "$branch"
            printf '%s\t%s\n' "$branch" "$remote branch"
        end
    end
end
