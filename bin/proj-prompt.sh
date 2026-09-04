# Print the prompt segment for the current directory: "<emoji> <project>[/<workstream>]".
#
# Its own script, not part of proj.fish, because starship runs this on every
# prompt and a fish function would mean spawning fish each time -- tens of
# milliseconds on every keystroke's worth of prompt redraw. POSIX sh plus one
# awk gets it into single digits. `proj prompt` delegates here so there is still
# only one implementation.
#
# Silence and exit 0 whenever we are not inside a project: starship renders the
# module as nothing, which is what we want everywhere else on the filesystem.
set -eu

root="${PROJECTS_ROOT:-$HOME/projects}"
here="$PWD"

case "$here" in
"$root"/*) ;;
*) exit 0 ;;
esac

rel="${here#"$root"/}"
project="${rel%%/*}"
readme="$root/$project/README.md"

[ -f "$readme" ] || exit 0

# Second path component, when there is one. A deeper path (inside a workstream's
# own subdirectories) still reports the workstream, which is the point.
rest="${rel#"$project"}"
rest="${rest#/}"
workstream="${rest%%/*}"

# Only the leading `---` fenced block is metadata; an `emoji:` in the prose below
# it is not. Falls back to a folder so an un-annotated project still shows.
emoji=$(awk '
    NR == 1 && $0 != "---" { exit }
    NR == 1 { next }
    $0 == "---" { exit }
    {
        eq = index($0, ":")
        if (eq == 0) next
        k = substr($0, 1, eq - 1)
        gsub(/^[ \t]+|[ \t]+$/, "", k)
        if (k != "emoji") next
        v = substr($0, eq + 1)
        gsub(/^[ \t]+|[ \t]+$/, "", v)
        gsub(/^"|"$/, "", v)
        print v
        exit
    }
' "$readme")
[ -n "$emoji" ] || emoji="📁"

if [ -n "$workstream" ]; then
    printf '%s %s/%s' "$emoji" "$project" "$workstream"
else
    printf '%s %s' "$emoji" "$project"
fi
