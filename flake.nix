{
  description = "Sync Jellyfin media from rsync/SSH or Jellyfin HTTP";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
    }:
    let
      # Home Manager module
      homeModule = import ./nix/module-hm.nix {
        packageForSystem = system: self.packages.${system}.default;
      };
    in
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        # Single source of truth: the crate version.
        inherit ((builtins.fromTOML (builtins.readFile ./Cargo.toml)).package) version;
        runtimeInputs = [
          pkgs.rsync
          pkgs.openssh
          pkgs.ffmpeg-headless
        ]
        ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux [ pkgs.systemd ];
      in
      {
        packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = "jellysync";
          inherit version;
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;
          # For --version (see build.rs): the source has no .git in the Nix build.
          # Tags are not visible to flakes, so Nix builds always name the commit.
          env.JELLYSYNC_GIT_REV = self.shortRev or self.dirtyShortRev or "";
          nativeBuildInputs = [
            pkgs.installShellFiles
            pkgs.makeWrapper
          ];
          postInstall = ''
            wrapProgram $out/bin/jellysync \
              --prefix PATH : ${pkgs.lib.makeBinPath runtimeInputs}
          ''
          # The binary generates its own completions, which needs to run it.
          + pkgs.lib.optionalString (pkgs.stdenv.buildPlatform.canExecute pkgs.stdenv.hostPlatform) ''
            installShellCompletion --cmd jellysync \
              --bash <($out/bin/jellysync completions bash) \
              --fish <($out/bin/jellysync completions fish) \
              --zsh <($out/bin/jellysync completions zsh)
          '';

          meta = {
            description = "Sync Jellyfin media from rsync/SSH or Jellyfin HTTP";
            homepage = "https://github.com/pschmitt/jellysync";
            license = pkgs.lib.licenses.gpl3Only;
            maintainers = with pkgs.lib.maintainers; [ pschmitt ];
            mainProgram = "jellysync";
            platforms = pkgs.lib.platforms.linux;
          };
        };

        # Alias for convenience
        packages.jellysync = self.packages.${system}.default;

        # Development shell
        devShells.default = pkgs.mkShell {
          packages = with pkgs; [
            cargo
            rustc
            rustfmt
            clippy
            rsync
            openssh
            just
            nixfmt
            statix
            deadnix
          ];
        };
      }
    )
    // {
      # Linux package builds and checks run on the self-hosted nixbot.
      hydraJobs = nixpkgs.lib.genAttrs [ "x86_64-linux" "aarch64-linux" ] (system: {
        packages = self.packages.${system} or { };
        checks = self.checks.${system} or { };
      });

      homeManagerModules.default = homeModule;
    };
}
