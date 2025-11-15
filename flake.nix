{
  description = "A bash tool to sync files from an SSH server to local directories";

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
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        version = "0.1.0";
      in
      {
        packages.default = pkgs.stdenv.mkDerivation {
          pname = "jellysync";
          inherit version;

          src = ./.;

          nativeBuildInputs = [ pkgs.makeWrapper ];

          buildInputs = [
            pkgs.bash
            pkgs.yq-go
            pkgs.rsync
            pkgs.openssh
          ];

          dontBuild = true;

          installPhase = ''
            runHook preInstall

            mkdir -p $out/bin
            install -Dm755 jellysync $out/bin/jellysync

            wrapProgram $out/bin/jellysync \
              --prefix PATH : ${
                pkgs.lib.makeBinPath [
                  pkgs.yq-go
                  pkgs.rsync
                  pkgs.openssh
                ]
              }

            runHook postInstall
          '';

          meta = {
            description = "Sync files from SSH server to local directories with flexible YAML configuration";
            homepage = "https://github.com/pschmitt/jellysync";
            license = pkgs.lib.licenses.gpl3Only;
            maintainers = with pkgs.lib.maintainers; [ pschmitt ];
            mainProgram = "jellysync";
            platforms = pkgs.lib.platforms.unix;
          };
        };

        # Alias for convenience
        packages.jellysync = self.packages.${system}.default;

        # Development shell
        devShells.default = pkgs.mkShell {
          inputsFrom = [ self.packages.${system}.default ];
          packages = with pkgs; [
            # dependencies
            bash
            yq-go
            rsync
            openssh

            # linting tools
            shellcheck
            statix
          ];
        };
      }
    );
}
