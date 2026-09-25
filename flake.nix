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
              cleanupConfig = lib.optionalAttrs (cfg.settings.cleanup != null) {
                inherit (cfg.settings) cleanup;
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
              baseConfig // libConfig // rsyncConfig // cleanupConfig // transferConfig // jobsConfig
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

              cleanup = mkOption {
                type = types.nullOr (
                  types.submodule {
                    options = {
                      delete_watched_after = mkOption {
                        type = types.str;
                        default = "7d";
                        description = "Grace period before watched files of jobs with delete_watched are deleted (e.g. 7d, 12h, 0).";
                        example = "3d";
                      };
                    };
                  }
                );
                default = null;
                description = "Automatic removal of watched downloads.";
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
                        example = "$tv_shows/Pioneer One";
                      };

                      local_dir = mkOption {
                        type = types.nullOr types.str;
                        default = null;
                        description = "Local directory path (supports templating with $var).";
                        example = "$tv_shows/Pioneer One";
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

                      delete_watched = mkOption {
                        type = types.nullOr types.bool;
                        default = null;
                        description = "Delete downloaded files once watched (after the grace period) and do not download watched items again.";
                      };

                      delete_watched_after = mkOption {
                        type = types.nullOr types.str;
                        default = null;
                        description = "Per-job grace period before watched files are deleted (overrides cleanup.delete_watched_after).";
                        example = "3d";
                      };

                      auto = mkOption {
                        type = types.nullOr (
                          types.either types.bool (
                            types.enum [
                              "all"
                              "movies"
                              "shows"
                            ]
                          )
                        );
                        default = null;
                        description = "Instead of a title: the newest unwatched movies and/or episodes in Jellyfin (true/all, movies, shows).";
                        example = "movies";
                      };

                      max_items = mkOption {
                        type = types.nullOr types.ints.unsigned;
                        default = null;
                        description = "Auto jobs: keep at most this many of the newest items (5 when no limit is set).";
                        example = 10;
                      };

                      max_size = mkOption {
                        type = types.nullOr types.number;
                        default = null;
                        description = "Auto jobs: size budget in GiB.";
                        example = 30;
                      };

                      library = mkOption {
                        type = types.nullOr types.str;
                        default = null;
                        description = "Auto jobs: only pick from this Jellyfin library (by name).";
                        example = "Kids";
                      };

                      enabled = mkOption {
                        type = types.nullOr types.bool;
                        default = null;
                        description = "Set to false to skip this job when syncing all jobs (it can still be synced by name).";
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
                  "Pioneer One" = {
                    remote_dir = "$tv_shows/Pioneer One";
                    local_dir = "$tv_shows/Pioneer One";
                  };
                  "Night of the Living Dead" = {
                    directory = "movies";
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
                "Pioneer One"
                "Night of the Living Dead"
              ];
            };
          };

          config = mkIf cfg.enable {
            assertions = [
              {
                assertion = cfg.settings.downloadMode != "jellyfin" || cfg.settings.jellyfin != null;
                message = "services.jellysync.settings.jellyfin must be set when downloadMode is \"jellyfin\" (the default).";
              }
            ];
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
                    "${cfg.package}/bin/jellysync download ${jobArgs}";
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
      homeManagerModules = {
        jellysync = homeModule;
        default = homeModule;
      };
    };
}
