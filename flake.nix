{
  description = "proj: a dashboard and worktree manager for ~/projects";

  inputs.nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAllSystems (pkgs: rec {
        proj-tui = pkgs.callPackage ./nix/package.nix { };
        proj-prompt = pkgs.callPackage ./nix/prompt.nix { };
        default = proj-tui;
      });

      # The whole tool -- binaries, fish function, completions, starship
      # segment. See nix/home-manager.nix.
      homeModules.default = ./nix/home-manager.nix;

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          packages = [
            pkgs.cargo
            pkgs.rustc
            pkgs.rust-analyzer
            pkgs.clippy
            pkgs.rustfmt
            pkgs.git
            pkgs.gh
          ];
        };
      });
    };
}
