# home-manager module for proj: the binaries, the fish half, and the starship
# segment. Import it by path from a host's home.nix, or as this flake's
# `homeModules.default`.
{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.programs.proj;
in

{
  options.programs.proj = {
    # Defaults on: importing the module is the opt-in.
    enable = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = "Install proj, the ~/projects worktree manager.";
    };

    package = lib.mkOption {
      type = lib.types.package;
      default = pkgs.callPackage ./package.nix { };
      defaultText = lib.literalExpression "pkgs.callPackage ./package.nix { }";
      description = "The dashboard, installed as `proj-tui`.";
    };

    promptPackage = lib.mkOption {
      type = lib.types.package;
      default = pkgs.callPackage ./prompt.nix { };
      defaultText = lib.literalExpression "pkgs.callPackage ./prompt.nix { }";
      description = "The `proj-prompt` script starship runs on every prompt.";
    };

    herdr.enable = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = ''
        Install the herdr plugin manifest at ~/.config/proj/herdr-plugin. Register
        it once with `herdr plugin link ~/.config/proj/herdr-plugin`.
      '';
    };

    starship.enable = lib.mkOption {
      type = lib.types.bool;
      default = config.programs.starship.enable;
      defaultText = lib.literalExpression "config.programs.starship.enable";
      description = ''
        Define the `custom.proj` starship segment. Placing it in the prompt is
        still the host's job: splice `''${custom.proj}` into its `format`.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    # Both on PATH: `proj prompt` and anything else that wants the segment can
    # then call it by name rather than by store path.
    home.packages = [
      cfg.package
      cfg.promptPackage
    ];

    xdg.configFile = {
      # The completions call back into the function itself (`proj __projects` /
      # `proj __workstreams`) so the two stay in sync, which is why they have to
      # travel together.
      "fish/functions/proj.fish".source = ../fish/proj.fish;
      "fish/completions/proj.fish".source = ../fish/completions/proj.fish;

      # Per-project CARGO_TARGET_DIR. Numbered to load after the toolchain
      # setup; see the file for why it is driven off PWD rather than `proj cd`.
      "fish/conf.d/30-proj-cargo.fish".source = ../fish/conf.d/30-proj-cargo.fish;
    };

    # A copy, not a home-manager symlink: herdr records the resolved manifest
    # path when linking, so a store symlink would pin it to this generation.
    # Store paths in the commands keep the hooks independent of herdr's PATH.
    home.activation.projHerdrPlugin = lib.mkIf cfg.herdr.enable (
      let
        manifest = pkgs.writeText "herdr-plugin.toml" (
          builtins.replaceStrings [ ''"proj-tui"'' ] [ ''"${cfg.package}/bin/proj-tui"'' ] (
            builtins.readFile ../herdr/herdr-plugin.toml
          )
        );
      in
      # Replaced by rename, since herdr watches it and reloads on every change.
      lib.hm.dag.entryAfter [ "writeBoundary" ] ''
        dest="${config.xdg.configHome}/proj/herdr-plugin/herdr-plugin.toml"
        if ! cmp -s ${manifest} "$dest"; then
          run install -Dm644 ${manifest} "$dest.tmp"
          run mv -f "$dest.tmp" "$dest"
        fi
      ''
    );

    programs.starship.settings = lib.mkIf cfg.starship.enable {
      custom.proj = {
        # The standalone script, not `proj prompt`: this runs on every prompt and
        # a fish function would mean spawning fish each time.
        command = "${cfg.promptPackage}/bin/proj-prompt";
        when = true;
        # Nothing but the emoji and name; the script emits both. Wrapped in an
        # optional group so the padding goes too when the output is empty --
        # without the parens, every directory outside ~/projects gets two stray
        # spaces of background before the path.
        format = "([ $output ](fg:#e3e5e5 bg:#6638b6))";
        # No `-c`: starship feeds the command to a custom shell on **stdin**,
        # not as an argv, so `dash -c` gets no operand and dies with "-c
        # requires an argument" -- silently, since starship swallows the
        # failure and renders an empty segment.
        #
        # dash rather than letting starship pick: unset, it falls back to
        # $STARSHIP_SHELL, which `starship init fish` sets to fish -- so every
        # prompt would spawn a fish, which is the whole thing this script exists
        # to avoid.
        #
        # Cheap enough to run everywhere -- it exits immediately outside
        # ~/projects -- so no directory filter to keep in sync.
        shell = [ "${pkgs.dash}/bin/dash" ];
      };
    };
  };
}
