{
  lib,
  rustPlatform,
  makeWrapper,
  git,
  gh,
}:

rustPlatform.buildRustPackage {
  pname = "proj-tui";
  version = "0.1.0";

  src = lib.cleanSource ../.;
  cargoLock.lockFile = ../Cargo.lock;

  # It shells out to `git` and `gh` rather than linking a git library or an
  # HTTP client, so both have to be on PATH wherever it runs -- including in
  # the daemon it starts from this same binary, which is the process that
  # actually runs them.
  nativeBuildInputs = [ makeWrapper ];
  # The merged-ness tests build scratch repos, because what they are checking
  # is what real `git` does with patch-ids.
  nativeCheckInputs = [ git ];
  postInstall = ''
    wrapProgram $out/bin/proj-tui \
      --prefix PATH : ${
        lib.makeBinPath [
          git
          gh
        ]
      }
  '';

  meta.description = "Dashboard for ~/projects worktrees, PRs and CI";
}
