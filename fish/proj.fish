# proj - manage project worktrees under ~/projects
#
# Replaces `wt`, which modelled one worktree per branch, flat, under ~/code.
# The layout here is two levels -- ~/projects/<project>/<workstream> -- where a
# project is an initiative with a README.md and a workstream is one unit of
# review, usually one branch and one PR. See ~/projects/README.md for the full
# design; this file is the phase-1 shell half of it.
#
# A workstream directory IS the worktree, added from the canonical clone of the
# first repo in its project's `repos:` -- ~/code/<repo>, or $PROJ_CODE_ROOT/<repo>.
# The rare cross-repo workstream (a directory of per-repo worktrees instead) is
# not handled here.

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

        case archive
            __proj_archive $argv

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

        case daemon
            __proj_daemon $argv

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
            __proj_complete_branches $argv

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
    echo "                                          Added from ~/code/<repo>, the first of the"
    echo "                                          project's repos; runs pnpm install and pnpm"
    echo "                                          build where the repo uses pnpm"
    echo "                                          With --rebase, rebase on main before building"
    echo "                                          A project that does not exist yet is created"
    echo "                                          first, prompting for its README frontmatter"
    echo "  new <project>                           Create just the project"
    echo "  archive [<project>]                     Move a project to ~/projects/ARCHIVE (defaults"
    echo "                                          to the one you are standing in), worktrees and all"
    echo "  rm|remove [<project>/<workstream>]      Remove a workstream (defaults to the current one)"
    echo "                                          Fails on uncommitted changes, or, unless its PR merged,"
    echo "                                          on commits that are neither in main nor on a remote"
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
    echo "  daemon [status|stop]                    The shared fetcher behind every open dashboard"
    echo "  help                                    Show this help"
    echo ""
    echo "Branches are named <project>/<workstream>, locally and on the remote, so a"
    echo "branch name and a directory path say the same thing."
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
                # Like `cd`: creating a worktree ends with you standing in it,
                # so the shell is the destination, not a detour.
                #
                # A fourth field is a base to branch from, which only a
                # workstream invented in the dashboard has: materializing a
                # virtual row takes the branch that already exists.
                set -l base
                if test (count $parts) -ge 4; and test -n "$parts[4]"
                    set base --branch "$parts[4]"
                    echo "Creating worktree for '$parts[2]' on branch '$parts[3]' from '$parts[4]'..."
                else
                    echo "Creating worktree for '$parts[2]' on branch '$parts[3]'..."
                end
                set -l repo
                if test (count $parts) -ge 5; and test -n "$parts[5]"
                    set repo --repo "$parts[5]"
                end
                __proj_new "$parts[2]" --branch-name "$parts[3]" $base $repo
                return $status

            case new-project
                # One line of output on success, and the dashboard reopens on
                # the new project: the next thing to do there is `n`.
                if __proj_create_project "$parts[2]" "$parts[3]" "$parts[4]" "$parts[5]"
                    set pause 0
                end

            case archive
                __proj_archive "$parts[2]"
                set reopen

            case edit
                # cd in, rather than passing the path as an argument. An editor
                # started elsewhere inherits that elsewhere: :terminal, :!, LSP
                # root detection and every fuzzy-finder resolve against the
                # directory `proj` was launched from, not the worktree you asked
                # to edit. lazygit does not need this -- its --path chdirs.
                pushd "$parts[2]"
                or begin
                    echo "proj: cannot enter $parts[2]" >&2
                    continue
                end
                $EDITOR .
                popd
                set pause 0

            case lazygit
                lazygit --path "$parts[2]"
                set pause 0

            case notes
                # Same cd-in reasoning as `edit`: the README links to other files
                # in the project, and an editor started elsewhere resolves them
                # against elsewhere.
                pushd "$parts[2]"
                or begin
                    echo "proj: cannot enter $parts[2]" >&2
                    continue
                end
                $EDITOR README.md
                popd
                set pause 0

            case rebase
                __proj_rebase_and_build "$parts[2]"

            case push
                __proj_push "$parts[2]" "$parts[3]"

            case push-force
                __proj_push "$parts[2]" "$parts[3]" --force

            case gh-ready
                gh pr ready "$parts[2]" --repo "$parts[3]"

            case gh-review
                gh pr edit "$parts[2]" --repo "$parts[4]" --add-reviewer "$parts[3]"

            case gh-ready-review
                gh pr ready "$parts[2]" --repo "$parts[4]"
                and gh pr edit "$parts[2]" --repo "$parts[4]" --add-reviewer "$parts[3]"

            case ci-rerun
                __proj_ci_rerun "$parts[2]"

            case review-checkout
                __proj_review_checkout "$parts[2]" "$parts[3]" "$parts[4]"
                return $status

            case review-update
                __proj_review_update "$parts[2]" "$parts[3]"
                return $status

            case delete
                __proj_remove "$parts[2]"
                # The row is gone, so reopening on it would land nowhere.
                set reopen

            case delete-branch
                __proj_delete_branch "$parts[3]" "$parts[4]" "$parts[5]"
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

function __proj_ci_rerun --description "Re-run the failed jobs of the workflow a CircleCI job URL belongs to"
    set -l url "$argv[1]"
    set -l m (string match -r -g '/gh/([^/]+/[^/]+)/(\d+)/?$' -- "$url")
    if test (count $m) -ne 2
        echo "proj: no project and job number in $url" >&2
        return 1
    end
    set -l project $m[1]
    set -l job $m[2]

    set -l api https://circleci.com/api
    set -l workflow (curl -sS --max-time 30 "$api/v1.1/project/github/$project/$job" | jq -r '.workflows.workflow_id // empty')
    if test -z "$workflow"
        echo "proj: could not resolve job $job to a workflow" >&2
        return 1
    end

    echo "Re-running failed jobs in workflow $workflow..."
    # vmguard injects Circle-Token on this endpoint and validates the body; only
    # from_failed/jobs/sparse_tree are accepted.
    curl -sS --max-time 30 -X POST "$api/v2/workflow/$workflow/rerun" \
        -H 'Content-Type: application/json' \
        -d '{"from_failed": true}' | jq -r '.message // .'
end

function __proj_review_checkout --description "Check out REPO's PR NUMBER into the review worktree named DIR"
    set -l number "$argv[1]"
    # The whole directory name, number and all, decided by the dashboard: it is
    # the PR number in that name that lets a later run pair the worktree back up
    # with the review, so the two must agree on it.
    set -l name "$argv[2]"
    set -l repo "$argv[3]"
    set -l clone (__proj_clone "$repo")
    or return 1
    set -l root (__proj_root)/review
    set -l wt_path "$root/$name"

    if test -e "$wt_path"
        echo "Worktree already exists at $wt_path"
        __proj_goto "$wt_path"
        return $status
    end

    if not test -f "$root/README.md"
        mkdir -p "$root"
        printf '%s\n' '---' 'emoji: "👀"' 'kind: review' 'name: Code review' "repos: [$repo]" '---' '' '# Code review' '' 'Worktrees checked out for review. Disposable.' > "$root/README.md"
    end

    set -l branch "review/$number"
    echo "Fetching PR #$number..."
    git -C "$clone" fetch --quiet origin "+refs/pull/$number/head:refs/heads/$branch"
    or begin
        echo "proj: could not fetch PR #$number" >&2
        return 1
    end

    __proj_worktree_add "$clone" "$wt_path" "$wt_path" "$branch"
    or return 1

    __proj_goto "$wt_path"
    or return 1
    __proj_build
end

function __proj_review_update --description "Fast-forward the review worktree at PATH to PR NUMBER's head, then build"
    set -l wt_path "$argv[1]"
    set -l number "$argv[2]"

    __proj_goto "$wt_path"
    or return 1

    # Fetched here rather than in the canonical clone: the branch is checked out
    # in this worktree, and git refuses to fetch straight into a ref that is.
    echo "Fetching PR #$number..."
    git fetch --quiet origin "refs/pull/$number/head"
    or begin
        echo "proj: could not fetch PR #$number" >&2
        return 1
    end

    # Fast-forward only. A review checkout holds nothing of yours, so anything
    # that is not a fast-forward means the author rebased or force-pushed -- and
    # the honest answer there is to say so and let the checkout be deleted and
    # taken again, not to reset a worktree out from under you.
    git merge --ff-only FETCH_HEAD
    or begin
        echo "" >&2
        echo "proj: #$number was rewritten; delete this checkout (d) and take it again" >&2
        return 1
    end

    __proj_build
end

function __proj_push --description "Push WORKTREE's branch to its remote name, setting upstream"
    set -l wt_path "$argv[1]"
    set -l remote_branch "$argv[2]"
    set -l force
    if contains -- --force $argv
        set force --force-with-lease
    end

    set -l branch (git -C "$wt_path" rev-parse --abbrev-ref HEAD 2>/dev/null)
    if test -z "$branch"; or test "$branch" = HEAD
        echo "proj push: refusing to push a detached HEAD" >&2
        return 1
    end

    # An explicit refspec, because local and remote names differ by design:
    # Explicit, because a branch's remote name can differ from its local one
    # (older branches were pushed under `brian/`), and `branch.<name>.merge` is
    # what records it.
    echo "Pushing '$branch' to origin/$remote_branch..."
    git -C "$wt_path" push $force origin "$branch:refs/heads/$remote_branch"
    or return 1

    git -C "$wt_path" branch -u "origin/$remote_branch" "$branch" >/dev/null 2>&1
    echo "Upstream set to origin/$remote_branch"
end

# ---------------------------------------------------------------------------
# Paths and configuration
# ---------------------------------------------------------------------------

function __proj_clone --description "Print the canonical clone of REPO, failing if there is none"
    set -l root ~/code
    set -q PROJ_CODE_ROOT; and set root $PROJ_CODE_ROOT
    set -l clone "$root/$argv[1]"
    if test -z "$argv[1]"; or not test -e "$clone/.git"
        echo "proj: no clone of '$argv[1]' at $clone" >&2
        return 1
    end
    echo $clone
end

function __proj_project_repos --description "Print each repo in PROJECT's repos: list"
    set -l raw (__proj_meta "$argv[1]" repos)
    for r in (string split , -- (string trim -c '[] ' -- "$raw"))
        set r (string trim -c " \"'" -- "$r")
        test -n "$r"; and echo $r
    end
end

function __proj_clone_of --description "Print the canonical clone the worktree at PATH was added from"
    set -l common (git -C "$argv[1]" rev-parse --path-format=absolute --git-common-dir 2>/dev/null)
    or return 1
    string replace -r '/[^/]*$' '' -- "$common"
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

function __proj_local_branch --description "Succeed if BRANCH exists locally in the repo at DIR"
    git -C "$argv[1]" show-ref --verify --quiet "refs/heads/$argv[2]"
end

function __proj_remote_ref --description "Print <remote>/<branch> if an already-fetched remote BRANCH exists in the repo at DIR"
    set -l dir "$argv[1]"
    set -l branch "$argv[2]"

    for remote in (git -C "$dir" remote)
        if git -C "$dir" show-ref --verify --quiet "refs/remotes/$remote/$branch"
            echo "$remote/$branch"
            return 0
        end
    end

    return 1
end

function __proj_fetch_branch --description "Look for BRANCH on the remotes of the repo at DIR, fetch it, print <remote>/<branch>"
    set -l dir "$argv[1]"
    set -l branch "$argv[2]"

    for remote in (git -C "$dir" remote)
        if git -C "$dir" ls-remote --exit-code --heads "$remote" "refs/heads/$branch" >/dev/null 2>&1
            echo "Fetching '$branch' from $remote..." >&2
            if git -C "$dir" fetch --quiet "$remote" "+refs/heads/$branch:refs/remotes/$remote/$branch"
                echo "$remote/$branch"
                return 0
            end
        end
    end

    return 1
end

function __proj_resolve_branch --description "Print <branch> or <remote>/<branch> if BRANCH exists anywhere for the repo at DIR, fetching if needed"
    set -l dir "$argv[1]"
    set -l branch "$argv[2]"

    if __proj_local_branch "$dir" "$branch"
        echo "$branch"
        return 0
    end

    set -l remote_ref (__proj_remote_ref "$dir" "$branch")
    if test -z "$remote_ref"
        set remote_ref (__proj_fetch_branch "$dir" "$branch")
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
    set -l onto (__proj_fetch_branch "$wt_path" main)
    if test -z "$onto"
        set onto (__proj_remote_ref "$wt_path" main)
    end
    if test -z "$onto"; and __proj_local_branch "$wt_path" main
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
    if not test -f pnpm-lock.yaml
        echo "No pnpm-lock.yaml here; skipping the build"
        return 0
    end
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

function __proj_worktree_add --description "Add the worktree at PATH from the clone REPO (git worktree add ARGS...), reflink-cloning the clone into it where the filesystem allows"
    set -l repo "$argv[1]"
    set -l wt_path "$argv[2]"
    set -e argv[1..2]

    git -C "$repo" worktree add --no-checkout $argv
    or return 1

    # `--reflink=always` fails rather than falling back to a full copy of every
    # node_modules and build output; a plain checkout is cheaper than that.
    set -l entries (string match -v -- "$repo/.git" $repo/* $repo/.*)
    if not cp -a --reflink=always $entries "$wt_path/" 2>/dev/null
        echo "proj: cannot reflink $repo, checking out instead" >&2
        rm -rf (string match -v -- "$wt_path/.git" $wt_path/* $wt_path/.*)
    end

    # The clone carries the canonical clone's checkout, so bring the tracked
    # files to this branch -- rewriting only those that differ, which keeps the
    # rest shared -- and drop its untracked files. Ignored files (node_modules,
    # build output) stay, and are what makes the first build incremental.
    git -C "$wt_path" reset -q
    and git -C "$wt_path" reset -q --hard
    and git -C "$wt_path" clean -fdq
end

function __proj_goto --description "cd to a path, remembering where we came from"
    # In proj's herdr popup, going somewhere means that place's workspace.
    if test "$HERDR_PLUGIN_ID" = proj
        proj-tui --herdr-open "$argv[1]"
        return $status
    end
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
    set -l repo ""
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
            case --repo
                set i (math $i + 1)
                if test $i -le (count $argv)
                    set repo $argv[$i]
                else
                    echo "proj new: --repo requires an argument" >&2
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
    set -l project $parts[1]
    set -l workstream $parts[2]
    if test -z "$project"; or test (count $parts) -eq 2 -a -z "$workstream"
        echo "proj new: expected <project>/<workstream> or <project>, got '$target'" >&2
        return 1
    end

    # A project that does not exist yet is made on the spot rather than refused:
    # the first workstream is when a project becomes real, and sending you off to
    # write a README by hand first is a detour the prompt can take instead.
    if not test -f "$root/$project/README.md"
        __proj_create_project "$project"
        or return 1
    else if test -z "$workstream"
        echo "proj new: project '$project' already exists" >&2
        return 1
    end
    if test -z "$workstream"
        return 0
    end

    set -l wt_path "$root/$project/$workstream"
    if test -e "$wt_path"
        echo "proj new: $wt_path already exists" >&2
        return 1
    end

    if test -z "$repo"
        set repo (__proj_project_repos "$project")[1]
        if test -z "$repo"
            echo "proj new: $root/$project/README.md names no repos; add e.g. 'repos: [vxsuite]'" >&2
            return 1
        end
    end
    set -l clone (__proj_clone "$repo")
    or return 1

    # The branch name IS the workstream's identity: `<project>/<workstream>`,
    # locally and on the remote. Nothing to configure
    # and nothing to guess -- a path and a branch name carry the same
    # information, in both directions.
    if test -z "$branch_name"
        set branch_name "$project/$workstream"
    end

    set -l git_args
    if __proj_local_branch "$clone" "$branch_name"
        if test -n "$base_branch"
            echo "proj new: branch '$branch_name' already exists, ignoring --branch $base_branch" >&2
        end
        echo "Checking out existing branch '$branch_name' at $wt_path..."
        set git_args "$wt_path" "$branch_name"
    else
        set -l remote_ref (__proj_remote_ref "$clone" "$branch_name")
        if test -z "$remote_ref"
            set remote_ref (__proj_fetch_branch "$clone" "$branch_name")
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

            set -l start (__proj_resolve_branch "$clone" "$base")
            if test -z "$start"
                echo "proj new: base branch '$base' not found locally or on any remote" >&2
                return 1
            end

            echo "Creating branch '$branch_name' from $start at $wt_path..."
            # `--no-track`: the base is where the branch starts, not where it
            # pushes; `proj push` sets the real upstream.
            set git_args --no-track -b "$branch_name" "$wt_path" "$start"
        end
    end

    __proj_worktree_add "$clone" "$wt_path" $git_args
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

function __proj_create_project --description "Create PROJECT's directory and README, prompting for the frontmatter it was not given"
    set -l root (__proj_root)
    set -l project "$argv[1]"
    set -l name "$argv[2]"
    set -l emoji "$argv[3]"
    set -l repos "$argv[4]"
    set -l project_status active

    if test -z "$project"
        echo "proj new: a project needs a directory name" >&2
        return 1
    end
    if string match -q -r '/|\s' -- "$project"; or test "$project" = ARCHIVE
        echo "proj new: '$project' cannot be a project directory" >&2
        return 1
    end
    if test -e "$root/$project"
        echo "proj new: $root/$project already exists" >&2
        return 1
    end

    # Prompt for what was not passed, and only when there is someone to ask.
    # The dashboard hands over every field; the command line hands over the
    # directory and asks for the rest.
    if test -z "$name"
        if not isatty stdin
            echo "proj new: no project '$project' (expected $root/$project/README.md)" >&2
            return 1
        end
        echo "Creating project '$project' at $root/$project"
        while test -z "$name"
            read -P "  name: " name
            or return 1
        end
        read -P "  emoji [📁]: " emoji
        or return 1
        # Defaults to the repo you are standing in, if any.
        set -l clone (__proj_clone_of (pwd))
        and set repos (string replace -r '.*/' '' -- "$clone")
        set -l answer
        while true
            read -P "  repos [$repos]: " answer
            or return 1
            test -n "$answer"; and set repos (string trim -c '[] ' -- "$answer")
            test -n "$repos"; and break
        end
        read -P "  status [$project_status]: " answer
        or return 1
        test -n "$answer"; and set project_status "$answer"
    end
    test -n "$emoji"; or set emoji 📁
    for r in (string split , -- "$repos")
        __proj_clone (string trim -- "$r") >/dev/null
        or echo "proj new: clone it there before adding workstreams" >&2
    end

    mkdir -p "$root/$project"
    or return 1
    printf '%s\n' \
        '---' \
        "emoji: \"$emoji\"" \
        "name: $name" \
        'kind: project' \
        "repos: [$repos]" \
        "status: $project_status" \
        '---' \
        '' \
        "# $name" \
        '' > "$root/$project/README.md"
    or return 1

    echo "Project '$project' created at $root/$project"
end

function __proj_archive --description "Move a project to ARCHIVE"
    set -l root (__proj_root)
    set -l project "$argv[1]"

    if test (count $argv) -gt 1
        echo "proj archive: unexpected argument '$argv[2]'" >&2
        return 1
    end

    if test -z "$project"
        # The project you are standing in, by the same containment rule as
        # `proj rm` -- except a project directory itself counts too.
        set -l here (realpath (pwd))
        set -l rel (string replace -- (realpath "$root")/ "" "$here")
        if test "$rel" != "$here"
            set project (string split -m1 / -- "$rel")[1]
        end
        if test -z "$project"; or not test -f "$root/$project/README.md"
            echo "proj archive: '"(pwd)"' is not inside a project" >&2
            return 1
        end
    end

    set -l from "$root/$project"
    if not test -f "$from/README.md"
        echo "proj archive: no project '$project'" >&2
        return 1
    end
    if test "$project" = ARCHIVE
        echo "proj archive: cannot archive the archive" >&2
        return 1
    end

    set -l to "$root/ARCHIVE/$project"
    if test -e "$to"
        echo "proj archive: $to already exists" >&2
        return 1
    end

    # Step out before the floor goes.
    set -l here (realpath (pwd))
    if test "$here" = (realpath "$from"); or string match -q (realpath "$from")"/*" "$here"
        __proj_goto "$root"
        echo "Changed directory to $root"
    end

    mkdir -p "$root/ARCHIVE"
    and mv "$from" "$to"
    or begin
        echo "proj archive: failed to move $from" >&2
        return 1
    end

    # The worktrees move with the project; point git at their new home.
    set -l moved
    for dir in $to/*/
        test -e "$dir/.git"; and set -a moved (string trim -r -c / -- "$dir")
    end
    for dir in $moved
        git -C "$dir" worktree repair &>/dev/null
        or echo "proj archive: git worktree repair failed in $dir; run it by hand" >&2
    end

    echo "Archived '$project' to $to"
end

function __proj_stranded_commits --description "Commits on WORKTREE's HEAD that exist neither in main nor on its remote"
    # `proj-tui` owns this answer, since it knows whether the PR merged.
    proj-tui --stranded "$argv[1]" 2>/dev/null
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
    set -l clone (__proj_clone_of "$wt_path")
    or begin
        echo "proj rm: $wt_path is not a git worktree" >&2
        return 1
    end

    if test $force -eq 0
        set -l dirty (git -C "$wt_path" status --porcelain 2>/dev/null)
        if test -n "$dirty"
            echo "proj rm: workstream has uncommitted changes:" >&2
            git -C "$wt_path" status --short >&2
            echo "" >&2
            echo "Use --force to remove anyway" >&2
            return 1
        end

        # Commits that exist nowhere but here. A review checkout has none by
        # definition: every commit in it came from `refs/pull/<n>/head`, which
        # GitHub keeps for good, and none of them are yours. The check would
        # otherwise refuse every one of these, since no remote-tracking ref
        # reaches them.
        if test -n "$branch"; and test "$branch" != HEAD
            and test (__proj_meta "$project" kind) != review
            set -l stranded (__proj_stranded_commits "$wt_path")
            if test -n "$stranded"
                echo "proj rm: '$branch' has commits that are neither in main nor on a remote:" >&2
                for sha in $stranded
                    git -C "$wt_path" log --oneline -1 $sha >&2
                end
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

    git -C "$clone" worktree remove "$wt_path" --force
    or begin
        echo "proj rm: failed to remove worktree" >&2
        return 1
    end

    if test -n "$branch"; and test "$branch" != HEAD
        # `-D`, not `-d`, once the checks above found nothing stranded. `-d`
        # asks about ancestry, and a squash-merged branch is not an ancestor of
        # main -- so the branch this most wants to delete is exactly the one it
        # would otherwise keep forever.
        if git -C "$clone" branch -D "$branch" >/dev/null 2>&1
            echo "Deleted branch '$branch'"
        else
            echo "Kept branch '$branch' (delete it with: git -C $clone branch -D $branch)"
        end
    end

    echo "Workstream '$project/$workstream' removed"
end

function __proj_delete_branch --description "Delete BRANCH (REMOTE on origin) of REPO, which has no worktree, locally and on origin"
    set -l branch "$argv[1]"
    set -l remote "$argv[2]"
    set -l repo (__proj_clone "$argv[3]")
    or return 1

    echo "Deleting branch '$branch'..."
    # `-D`, not `-d`: a squash-merged branch is not an ancestor of main, and the
    # dashboard already said what is at stake.
    git -C "$repo" branch -D "$branch"
    or return 1

    # Ask the remote rather than trusting the tracking ref: a merged PR deletes
    # its branch on GitHub, and pushing a delete for a branch that is already
    # gone fails loudly over something that is not a problem.
    if test -n "$remote"; and git -C "$repo" ls-remote --exit-code --heads origin "$remote" >/dev/null 2>&1
        git -C "$repo" push origin --delete "$remote"
        or echo "proj: kept origin/$remote (delete it with: git -C $repo push origin --delete $remote)" >&2
    end
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
        set -l base (__proj_remote_ref "$path" main)
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

# The fetcher every dashboard shares. Nobody starts it by hand -- the first
# `proj` that finds no socket does -- so this is only ever "is it running" and
# "stop it", which is what you want when a query has gone wrong and you would
# like the next dashboard to start over.
function __proj_daemon
    switch "$argv[1]"
        case "" status
            proj-tui --daemon-status
        case stop
            proj-tui --daemon-stop
        case '*'
            echo "proj daemon: expected status or stop" >&2
            return 1
    end
end

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

function __proj_complete_branches --description "Print 'branch<TAB>description' for the branches of the repo of the project named in ARGS"
    # ARGS is the command line so far; its `<project>/...` picks the repo.
    set -l repo
    for arg in $argv
        set -l project (string split -m1 / -- "$arg")[1]
        if test -f (__proj_root)/$project/README.md
            set repo (__proj_project_repos "$project")[1]
            break
        end
    end
    set -l clone
    if test -n "$repo"
        set clone (__proj_clone "$repo" 2>/dev/null)
    else
        set clone (__proj_clone_of (pwd))
    end
    test -n "$clone"; or return 0

    set -l seen

    for branch in (git -C "$clone" for-each-ref --format='%(refname:short)' refs/heads 2>/dev/null)
        set -a seen "$branch"
        printf '%s\t%s\n' "$branch" "local branch"
    end

    for remote in (git -C "$clone" remote)
        for ref in (git -C "$clone" for-each-ref --format='%(refname:short)' "refs/remotes/$remote" 2>/dev/null)
            set -l branch (string replace -- "$remote/" "" "$ref")
            test "$branch" != HEAD; or continue
            contains -- "$branch" $seen; and continue
            set -a seen "$branch"
            printf '%s\t%s\n' "$branch" "$remote branch"
        end
    end
end
