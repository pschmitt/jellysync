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
    let
      # Home Manager module
      homeModule =
        {
          config,
          pkgs,
          lib,
          ...
        }:
        let
          cfg = config.services.jellysync;
          defaultPkg = self.packages.${pkgs.system}.default;
          yamlFormat = pkgs.formats.yaml { };

          # Convert jobs from attrset to list format for YAML
          convertJobs =
            jobs:
            lib.mapAttrsToList (
              name: job:
              {
                inherit name;
              }
              // (lib.filterAttrs (k: v: k != "name" && v != null) job)
            ) jobs;

          # Build the config file
          configFile =
            let
              baseConfig = {
                inherit (cfg.settings) remote local;
              };
              libConfig = lib.optionalAttrs (cfg.settings.library != null) {
                inherit (cfg.settings) library;
              };
              rsyncConfig = lib.optionalAttrs (cfg.settings.rsync != null) {
                inherit (cfg.settings) rsync;
              };
              jobsConfig =
                if (cfg.settings.jobs != null && cfg.settings.jobs != { }) then
                  {
                    jobs = convertJobs cfg.settings.jobs;
                  }
                else
                  { };
            in
            yamlFormat.generate "jellysync-config.yaml" (baseConfig // libConfig // rsyncConfig // jobsConfig);
        in
        with lib;
        {
          options.services.jellysync = {
            enable = mkEnableOption "jellysync file synchronization service";

            package = mkOption {
              type = types.package;
              default = defaultPkg;
              defaultText = literalExpression "inputs.jellysync.packages.\${pkgs.system}.default";
              description = "The jellysync package to use.";
            };

            settings = {
              remote = mkOption {
                type = types.submodule {
                  options = {
                    hostname = mkOption {
                      type = types.str;
                      description = "Remote SSH hostname.";
                      example = "jellyfin.example.com";
                    };

                    username = mkOption {
                      type = types.str;
                      description = "Remote SSH username.";
                      example = "jelly";
                    };

                    port = mkOption {
                      type = types.port;
                      default = 22;
                      description = "Remote SSH port.";
                    };

                    root = mkOption {
                      type = types.str;
                      description = "Remote root directory (all remote paths are relative to this).";
                      example = "/mnt/data/videos";
                    };

                    directories = mkOption {
                      type = types.attrsOf types.str;
                      default = { };
                      description = "Named remote directory mappings (relative to root).";
                      example = {
                        tv_shows = "tv_shows";
                        movies = "movies";
                      };
                    };
                  };
                };
                description = "Remote server configuration.";
              };

              local = mkOption {
                type = types.submodule {
                  options = {
                    root = mkOption {
                      type = types.str;
                      description = "Local root directory (all local paths are relative to this).";
                      example = "~/Videos";
                    };

                    directories = mkOption {
                      type = types.attrsOf types.str;
                      default = { };
                      description = "Named local directory mappings (relative to root or absolute).";
                      example = {
                        tv_shows = "TV Shows";
                        movies = "Movies";
                      };
                    };
                  };
                };
                description = "Local directory configuration.";
              };

              library = mkOption {
                type = types.nullOr (
                  types.submodule {
                    options = {
                      season_pattern = mkOption {
                        type = types.str;
                        default = "Season $season_number";
                        description = "Pattern for season directories. Available variables: $name, $season_number";
                        example = "$name - Season $season_number";
                      };

                      episode_pattern = mkOption {
                        type = types.str;
                        default = "E$episode_number";
                        description = "Pattern for episode files. Available variables: $episode_number";
                        example = "S[0-9]+E$episode_number";
                      };
                    };
                  }
                );
                default = null;
                description = "Library organization settings.";
              };

              rsync = mkOption {
                type = types.nullOr (
                  types.submodule {
                    options = {
                      flags = mkOption {
                        type = types.listOf types.str;
                        default = [
                          "-a"
                          "-v"
                          "-z"
                          "--delete"
                        ];
                        description = "Custom rsync flags.";
                      };
                    };
                  }
                );
                default = null;
                description = "Rsync configuration.";
              };

              jobs = mkOption {
                type = types.attrsOf (
                  types.submodule {
                    options = {
                      remote_dir = mkOption {
                        type = types.nullOr types.str;
                        default = null;
                        description = "Remote directory path (supports templating with $var).";
                        example = "$tv_shows/Pluribus";
                      };

                      local_dir = mkOption {
                        type = types.nullOr types.str;
                        default = null;
                        description = "Local directory path (supports templating with $var).";
                        example = "$tv_shows/Pluribus";
                      };

                      directory = mkOption {
                        type = types.nullOr types.str;
                        default = null;
                        description = "Shorthand: use same directory name for both remote and local.";
                        example = "tv_shows";
                      };

                      seasons = mkOption {
                        type = types.nullOr (
                          types.oneOf [
                            types.str
                            (types.listOf types.int)
                          ]
                        );
                        default = null;
                        description = "Season filter: 'latest', 'latest-N', '1-10', or [1, 2, 5].";
                        example = "latest-2";
                      };

                      episodes = mkOption {
                        type = types.nullOr (
                          types.oneOf [
                            types.str
                            (types.listOf types.int)
                          ]
                        );
                        default = null;
                        description = "Episode filter: 'latest', 'latest-N', '1-10', or [1, 2, 3].";
                        example = "latest-5";
                      };

                      wildcard = mkOption {
                        type = types.nullOr types.bool;
                        default = null;
                        description = "Enable wildcard matching (*name*).";
                      };
                    };
                  }
                );
                default = { };
                description = "Sync jobs configuration (attrset where key is the job name).";
                example = {
                  pluribus = {
                    remote_dir = "$tv_shows/Pluribus";
                    local_dir = "$tv_shows/Pluribus";
                  };
                  "Star Trek" = {
                    directory = "tv_shows";
                  };
                };
              };
            };

            schedule = mkOption {
              type = types.str;
              default = "0 3 * * *";
              description = "Systemd timer schedule (OnCalendar format). Default: daily at 3 AM.";
              example = "hourly";
            };

            persistent = mkOption {
              type = types.bool;
              default = true;
              description = "Whether missed runs should be executed after system restart.";
            };

            jobNames = mkOption {
              type = types.listOf types.str;
              default = [ ];
              description = "List of specific job names to sync. If empty, all jobs are synced.";
              example = [
                "pluribus"
                "Star Trek"
              ];
            };
          };

          config = mkIf cfg.enable {
            home.packages = [ cfg.package ];

            xdg.configFile."jellysync/config.yaml".source = configFile;

            systemd.user.services.jellysync = {
              Unit = {
                Description = "Jellysync file synchronization";
                After = [ "network-online.target" ];
                Wants = [ "network-online.target" ];
              };

              Service = {
                Type = "oneshot";
                ExecStart =
                  let
                    jobArgs =
                      if cfg.jobNames != [ ] then
                        lib.concatMapStringsSep " " (job: lib.escapeShellArg job) cfg.jobNames
                      else
                        "";
                  in
                  "${cfg.package}/bin/jellysync ${jobArgs}";
                Environment = [ "PATH=${lib.makeBinPath [ cfg.package ]}" ];
              };
            };

            systemd.user.timers.jellysync = {
              Unit = {
                Description = "Jellysync file synchronization timer";
                Requires = [ "jellysync.service" ];
              };

              Timer = {
                OnCalendar = cfg.schedule;
                Persistent = cfg.persistent;
              };

              Install = {
                WantedBy = [ "timers.target" ];
              };
            };
          };
        };
    in
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
    )
    // {
      homeManagerModules = {
        jellysync = homeModule;
        default = homeModule;
      };
    };
}
