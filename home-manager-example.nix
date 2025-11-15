# Example Home Manager configuration for jellysync
#
# This file demonstrates how to integrate jellysync into your home-manager setup.
# Copy relevant sections to your home.nix or modules.

{
  pkgs,
  inputs,
  ...
}:
{
  # Import the jellysync home-manager module
  # Add this to your imports list
  imports = [
    inputs.jellysync.homeManagerModules.default
  ];

  # Configure jellysync service
  services.jellysync = {
    # Enable the service
    enable = true;

    # Optionally override the package
    # package = inputs.jellysync.packages.${pkgs.system}.default;

    # Configuration settings
    settings = {
      # Remote server configuration
      remote = {
        hostname = "jellyfin.example.com";
        username = "jelly";
        port = 22;
        root = "/mnt/data/videos";
        directories = {
          tv_shows = "tv_shows";
          movies = "movies";
          documentaries = "documentaries";
        };
      };

      # Local directory configuration
      local = {
        root = "~/Videos";
        directories = {
          tv_shows = "TV Shows";
          movies = "Movies";
          documentaries = "~/Documentaries"; # Absolute path example
        };
      };

      # Optional: Library organization settings
      library = {
        season_pattern = "Season $season_number";
        episode_pattern = "E$episode_number";
      };

      # Optional: Custom rsync flags
      rsync = {
        flags = [
          "-a"
          "-v"
          "-z"
          "--delete"
          # Add more flags as needed:
          # "--progress"
          # "--bwlimit=5000"
        ];
      };

      # Sync jobs - use attrset format where keys are job names
      jobs = {
        # Simple job with explicit paths
        pluribus = {
          remote_dir = "$tv_shows/Pluribus";
          local_dir = "$tv_shows/Pluribus";
        };

        # Using shorthand syntax
        "Star Trek" = {
          directory = "tv_shows";
        };

        # Season filtering - latest 2 seasons
        Andor = {
          directory = "tv_shows";
          seasons = "latest-2";
        };

        # Season filtering - specific seasons (list)
        "Breaking Bad" = {
          directory = "tv_shows";
          seasons = [
            1
            2
            5
          ];
        };

        # Latest season only
        "The Paper" = {
          directory = "tv_shows";
          seasons = "latest";
        };

        # Season range
        "South Park" = {
          directory = "tv_shows";
          seasons = "1-10";
        };

        # Season and episode filtering
        "The Office" = {
          directory = "tv_shows";
          seasons = "1";
          episodes = "1-5";
        };

        # Latest episodes from multiple seasons
        Friends = {
          directory = "tv_shows";
          seasons = [
            1
            2
          ];
          episodes = "latest";
        };

        # Wildcard matching
        "The Paper (wildcard)" = {
          directory = "tv_shows";
          wildcard = true;
        };

        # Movies
        "Favorite Movie" = {
          remote_dir = "$movies/Favorite Movie";
          local_dir = "$movies/Favorite Movie";
        };
      };
    };

    # Systemd timer schedule
    # Format: systemd OnCalendar format
    # Default: "0 3 * * *" (daily at 3 AM)
    schedule = "0 3 * * *";

    # Examples:
    # schedule = "hourly";                  # Every hour
    # schedule = "*-*-* 0/6:00:00";        # Every 6 hours
    # schedule = "0 6,18 * * *";           # Twice daily (6 AM and 6 PM)
    # schedule = "Mon *-*-* 02:00:00";     # Every Monday at 2 AM

    # Run missed jobs after system restart
    persistent = true;

    # Optional: Sync only specific jobs
    # If empty list, all jobs will be synced
    jobNames = [ ];

    # Example: sync only specific jobs
    # jobNames = [
    #   "pluribus"
    #   "Star Trek"
    #   "Andor"
    # ];
  };

  # The module automatically:
  # 1. Creates ~/.config/jellysync/config.yaml with your settings
  # 2. Installs the jellysync package
  # 3. Sets up systemd user service (jellysync.service)
  # 4. Sets up systemd user timer (jellysync.timer)
  # 5. Enables the timer to run on schedule

  # Manual control:
  # systemctl --user status jellysync.timer   # Check timer status
  # systemctl --user start jellysync.service  # Run sync now
  # systemctl --user stop jellysync.timer     # Stop scheduled syncs
  # journalctl --user -u jellysync.service    # View logs
}
