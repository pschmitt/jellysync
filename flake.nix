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
      homeModule =
        {
          config,
          pkgs,
          lib,
          ...
        }:
        let
          inherit (lib)
            literalExpression
            mkIf
            mkOption
            types
            ;
          cfg = config.services.jellysync;
          defaultPkg = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
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
                inherit (cfg.settings) remote local player;
              }
              // lib.optionalAttrs (cfg.settings.jellyfin != null) {
                inherit (cfg.settings) jellyfin;
              }
              // lib.optionalAttrs (cfg.settings.file_manager != null) {
                inherit (cfg.settings) file_manager;
              };
              libConfig = lib.optionalAttrs (cfg.settings.library != null) {
                inherit (cfg.settings) library;
              };
              rsyncConfig = lib.optionalAttrs (cfg.settings.rsync != null) {
                inherit (cfg.settings) rsync;
              };
              transferConfig = {
                download = {
                  mode = cfg.settings.downloadMode;
                };
                parallelism = cfg.settings.parallelism;
              };
              jobsConfig =
                if (cfg.settings.jobs != null && cfg.settings.jobs != { }) then
                  {
                    jobs = convertJobs cfg.settings.jobs;
                  }
                else
                  { };
            in
            yamlFormat.generate "jellysync-config.yaml" (
              baseConfig // libConfig // rsyncConfig // transferConfig // jobsConfig
            );
        in
        {
          options.services.jellysync = {
            enable = lib.mkEnableOption "jellysync file synchronization service";

            package = lib.mkOption {
              type = lib.types.package;
              default = defaultPkg;
              defaultText = literalExpression "inputs.jellysync.packages.\${pkgs.stdenv.hostPlatform.system}.default";
              description = "The jellysync package to use.";
            };

            settings = {
              downloadMode = lib.mkOption {
                type = lib.types.enum [
                  "rsync"
                  "jellyfin"
                ];
                default = "jellyfin";
                description = "Download files through rsync over SSH or directly from Jellyfin.";
              };

              parallelism = lib.mkOption {
                type = lib.types.ints.positive;
                default = 2;
                description = "Maximum number of jobs downloaded concurrently.";
              };

              player = lib.mkOption {
                type = lib.types.str;
                default = "mpv";
                description = "Command used by the TUI to play selected downloads.";
                example = "mpv";
              };

              file_manager = lib.mkOption {
                type = lib.types.nullOr lib.types.str;
                default = null;
                description = "Command used by the TUI to open download directories. Defaults to xdg-open, falling back to gio open.";
                example = "nautilus";
              };

              remote = lib.mkOption {
                type = lib.types.submodule {
                  options = {
                    hostname = lib.mkOption {
                      type = lib.types.str;
                      description = "Remote SSH hostname.";
                      example = "jellyfin.example.com";
                    };

                    username = lib.mkOption {
                      type = lib.types.str;
                      description = "Remote SSH username.";
                      example = "jelly";
                    };

                    port = lib.mkOption {
                      type = lib.types.port;
                      default = 22;
                      description = "Remote SSH port.";
                    };

                    root = lib.mkOption {
                      type = lib.types.str;
                      description = "Remote root directory (all remote paths are relative to this).";
                      example = "/mnt/data/videos";
                    };

                    directories = lib.mkOption {
                      type = lib.types.attrsOf lib.types.str;
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

              local = lib.mkOption {
                type = lib.types.submodule {
                  options = {
                    root = lib.mkOption {
                      type = lib.types.str;
                      description = "Local root directory (all local paths are relative to this).";
                      example = "~/Videos";
                    };

                    directories = lib.mkOption {
                      type = lib.types.attrsOf lib.types.str;
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

              jellyfin = lib.mkOption {
                type = lib.types.nullOr (
                  lib.types.submodule {
                    options = {
                      base_url = lib.mkOption {
                        type = lib.types.str;
                        description = "Jellyfin server base URL.";
                        example = "https://jellyfin.example.com";
                      };

                      username = lib.mkOption {
                        type = lib.types.nullOr lib.types.str;
                        default = null;
                        description = "Jellyfin user used to query watched status and resolve user_id.";
                      };

                      password_file = lib.mkOption {
                        type = lib.types.nullOr lib.types.str;
                        default = null;
                        description = "Path to a file containing the Jellyfin user's password (legacy authentication).";
                      };

                      api_key_file = lib.mkOption {
                        type = lib.types.nullOr lib.types.str;
                        default = null;
                        description = "Path to a file containing a Jellyfin API key.";
                      };

                      user_id = lib.mkOption {
                        type = lib.types.nullOr lib.types.str;
                        default = null;
                        description = "Jellyfin user ID; set this or username when using an API key.";
                      };
                    };
                  }
                );
                default = null;
                description = "Jellyfin authentication for watched-state filtering.";
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
                      jellyfin_name = mkOption {
                        type = types.nullOr types.str;
                        default = null;
                        description = "Jellyfin library title when it differs from the job name.";
                      };

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

                      unwatched = mkOption {
                        type = types.nullOr types.bool;
                        default = null;
                        description = "Sync only episodes marked unplayed for the configured Jellyfin user.";
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
              default = "hourly";
              description = "Systemd timer schedule (OnCalendar format). Default: hourly";
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

            systemd.user = {
              services.jellysync = {
                Unit = {
                  Description = "Jellysync file synchronization";
                  After = [ "network-online.target" ];
                  Wants = [ "network-online.target" ];
                };

                Service = {
                  ExecStart =
                    let
                      jobArgs =
                        if cfg.jobNames != [ ] then
                          lib.concatMapStringsSep " " (job: lib.escapeShellArg job) cfg.jobNames
                        else
                          "";
                    in
                    "${cfg.package}/bin/jellysync sync ${jobArgs}";
                };
              };

              timers.jellysync = {
                Unit = {
                  Description = "Jellysync file synchronization timer";
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
        };
    in
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        version = "1.1.0";
        runtimeInputs = [
          pkgs.rsync
          pkgs.openssh
        ]
        ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux [ pkgs.systemd ];
      in
      {
        packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = "jellysync";
          inherit version;
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;
          nativeBuildInputs = [ pkgs.makeWrapper ];
          postInstall = ''
            wrapProgram $out/bin/jellysync \
              --prefix PATH : ${pkgs.lib.makeBinPath runtimeInputs}
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
