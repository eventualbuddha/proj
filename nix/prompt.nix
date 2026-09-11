{ writeShellScriptBin }:

# The prompt segment as a real binary. starship runs it on every prompt, so it
# cannot be the `proj` fish function -- that would mean spawning fish per
# prompt. `proj prompt` delegates here, so there is still one implementation.
writeShellScriptBin "proj-prompt" (builtins.readFile ../bin/proj-prompt.sh)
