# jellysync

Jellysync is a Rust CLI for syncing media from Jellyfin or over rsync and SSH. Jellyfin HTTP downloads are the default and resume interrupted files; season, episode, and unwatched filters are supported. The rsync-over-SSH mode currently supports unfiltered jobs.

## Features

- 📁 Template-based path configuration with variable substitution
- 🔄 Sync specific jobs or all at once
- 🎯 Shorthand syntax for common patterns
- 🔍 Dry-run mode to preview changes
- 📊 Progress reporting with colored output
- ⚙️ Flexible YAML configuration
- 🖥️ Terminal status view and systemd service controls
- ⚡ Configurable parallel job execution (default: 2)

## Installation

### Using Nix (Recommended)

```bash
# Run directly with nix run
nix run github:pschmitt/jellysync -- --help

# Install to your profile
nix profile install github:pschmitt/jellysync

# Or add to your NixOS configuration
{
  inputs.jellysync.url = "github:pschmitt/jellysync";
  # ...
  environment.systemPackages = [ inputs.jellysync.packages.${system}.default ];
}
```

### Using Home Manager

Jellysync includes a Home Manager module for automated synchronization with systemd timers.

**1. Add jellysync to your flake inputs:**
```nix
{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    home-manager.url = "github:nix-community/home-manager";
    jellysync.url = "github:pschmitt/jellysync";
  };
}
```

**2. Import the module in your home-manager configuration:**
```nix
{
  home-manager.users.youruser = { pkgs, ... }: {
    imports = [
      inputs.jellysync.homeManagerModules.default
    ];

    services.jellysync = {
      enable = true;

      # The package is automatically provided from the flake
      # You can override it if needed:
      # package = inputs.jellysync.packages.${pkgs.system}.default;

      settings = {
        remote = {
          hostname = "jellyfin.example.com";
          username = "jelly";
          port = 22;
          root = "/mnt/data/videos";
          directories = {
            movies = "movies";
            tv_shows = "tv_shows";
          };
        };

        local = {
          root = "~/Videos";
          directories = {
            movies = "Movies";
            tv_shows = "TV Shows";
          };
        };

        jobs = {
          pluribus = {
            remote_dir = "$tv_shows/Pluribus";
            local_dir = "$tv_shows/Pluribus";
          };
          "Star Trek" = {
            directory = "tv_shows";
          };
          Andor = {
            directory = "tv_shows";
            seasons = "latest-2";
          };
        };
      };

      # Sync schedule (systemd timer format)
      schedule = "0 3 * * *";  # Daily at 3 AM
      
      # Run missed jobs after system restart
      persistent = true;

      # Optional: sync only specific jobs
      jobNames = [ "pluribus" "Star Trek" ];
    };
  };
}
```

**Module Options:**

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `enable` | bool | `false` | Enable the jellysync service |
| `package` | package | *(auto)* | The jellysync package (automatically provided from flake) |
| `settings` | attrs | - | Configuration settings (see Configuration section) |
| `schedule` | string | `"0 3 * * *"` | Systemd timer schedule (OnCalendar format) |
| `persistent` | bool | `true` | Run missed jobs after system restart |
| `jobNames` | list of strings | `[]` | Specific jobs to sync (empty = all jobs) |

**Schedule Examples:**

```nix
# Daily at 3 AM (default)
schedule = "0 3 * * *";

# Every hour
schedule = "hourly";

# Every 6 hours
schedule = "*-*-* 0/6:00:00";

# Twice daily (6 AM and 6 PM)
schedule = "0 6,18 * * *";

# Every Monday at 2 AM
schedule = "Mon *-*-* 02:00:00";
```

**What it does:**
- Creates `~/.config/jellysync/config.yaml` with your settings
- Installs jellysync package (automatically from the flake)
- Sets up systemd user service and timer
- Automatically syncs on schedule

**Note:** The `package` option is optional and automatically defaults to the package provided by the jellysync flake. You only need to set it if you want to use a different version or build.

### Commands

```console
jellysync                     # Open the TUI (same as `jellysync tui`)
jellysync download [JOB]...   # Download all jobs or selected ones (aliases: fetch, sync)
jellysync status              # Show recent job status and timer state
jellysync prune [JOB]...      # Preview stale managed downloads
jellysync prune --apply       # Apply the prune
jellysync start               # Start the user systemd service
jellysync stop                # Stop the user systemd service
jellysync config              # Print the parsed config
```

Ad-hoc library downloads started from Explore run in a detached background
worker, so they keep going after the TUI exits. Interrupted ones (e.g. the
worker died) are resumed automatically when the TUI starts and Jellyfin is
reachable, and by every full `jellysync download` (such as the systemd timer).
Select a
file and press `p` (or double-click it) to launch the configured player
(default: `mpv`); set `player` in the YAML or Home Manager settings to change
it. `Ctrl-C` closes Explore, and pressing it twice in the main view quits the
TUI. Press `x` on an ad-hoc job in the Jobs list to delete its tracked files and
remove it from the list. Press `o` to open the selected show's or movie's
download directory (or, with a file focused, its folder) with `file_manager`
(default: `xdg-open`, falling back to `gio open`).

The TUI works offline. It pings Jellyfin (`/System/Ping`) in the background and
shows `ONLINE`/`OFFLINE` in the header; while offline, local files, playback,
media info and clearing keep working, and browsing (plus syncing in `jellyfin`
mode) is disabled with a notice instead of failing. Reconciliation and poster
loading start once Jellyfin becomes reachable.

In Explore, `Tab`, `→` or `Enter` moves focus from the library to the details
pane of the selected show; there, `↑`/`↓` move through episodes, `Space`
selects one, `a` selects all, and `d` or `Enter` downloads the selection
(`d`/`Enter` on a movie downloads it). `Tab`, `←` or `Esc` returns to the
library; after closing Explore, the main view selects the job and file you just
queued.

In the Files list, `Space` pauses a queued or running download (keeping its
partial file and freeing its worker slot for the next queued one) and resumes a
paused one. Paused downloads are not resumed automatically.

Use `--config FILE` to select a config and `-j N` (`--parallelism`, `--parallel`) to override the worker limit for a download.

### Manual Installation

Install the flake package with Nix, then copy `jellysync-config.sample.yaml` to `~/.config/jellysync/config.yaml` and configure it.

GitHub Releases publish archives for Linux x86_64 (glibc and musl), macOS Apple Silicon, and Android aarch64 for Termux. In Termux, extract the `aarch64-linux-android` archive and copy `jellysync` into `$PREFIX/bin`.

## Configuration

Create a `jellysync.yaml` file with your sync configuration:

```yaml
remote:
  hostname: jellyfin.example.com
  username: jelly
  port: 22
  root: /mnt/data/videos
  directories:
    tv_shows: tv_shows
    movies: movies
    documentaries: documentaries

local:
  root: ~/Videos
  directories:
    tv_shows: "TV Shows"
    movies: Movies
    documentaries: ~/Documentaries

# Required for Jellyfin download mode. Keep the API key in a secret
# file and point api_key_file at it; never put the key in this config.
jellyfin:
  base_url: https://jellyfin.example.com
  api_key_file: /run/secrets/jellyfin/api-key
  username: jelly
  # Optional when Jellysync can resolve username through the API.
  user_id: 0123456789abcdef0123456789abcdef

library:
  season_pattern: "Season $season_number"

rsync:
  flags:
    - -a
    - -v
    - -z

download:
  mode: jellyfin # jellyfin (HTTP, default) or rsync (SSH)

parallelism: 2 # Concurrent jobs; default is 2
player: mpv # TUI player command; default is mpv
file_manager: xdg-open # TUI `o` command; default is xdg-open, then gio open

jobs:
  # Sync all of pluribus
  - name: pluribus
    remote_dir: $tv_shows/Pluribus
    local_dir: $tv_shows/Pluribus

  # Sync all of Star Trek (shorthand syntax)
  - name: Star Trek
    directory: tv_shows

  # Sync season 1 of The Penguin
  - name: The Penguin
    remote_dir: "$tv_shows/$name/Season 1"
    local_dir: "$tv_shows/The Penguin - Season 1"

  # Sync seasons 1-10 of Andor (range)
  - name: Andor
    directory: tv_shows
    seasons: "1-10"

  # Sync only episodes marked unplayed by this Jellyfin user
  - name: South Park
    directory: tv_shows
    seasons: latest-2
    unwatched: true

  # Sync the latest season
  - name: Slow Horses
    directory: tv_shows
    seasons: latest

  # Sync season 13 and any later seasons
  - name: American Horror Story
    directory: tv_shows
    seasons: "13-9999"

  # Sync specific seasons of Breaking Bad (list)
  - name: "Breaking Bad"
    directory: tv_shows
    seasons: [1, 2, 5]

  # Sync only latest season of The Paper
  - name: "The Paper"
    directory: tv_shows
    seasons: "latest"

  # Sync using wildcards (resolves to first match)
  - name: "The Paper (2025)"
    remote_dir: "$tv_shows/The Paper*"
    local_dir: "$tv_shows/The Paper (2025)"
```

Jellyfin is the default `download.mode`. The HTTP downloader keeps interrupted data in `.partial` files and resumes from the saved byte offset with an HTTP Range request. If the server declines the range, Jellysync restarts that file from byte zero. `parallelism` defaults to two concurrent transfers.

### Configuration Sections

#### Remote Section

| Setting | Required | Default | Description |
|---------|----------|---------|-------------|
| `hostname` | Yes | - | SSH server hostname or IP address |
| `username` | Yes | - | SSH username for authentication |
| `port` | No | 22 | SSH port number |
| `root` | Yes | - | Base directory on remote server (all remote paths are relative to this) |
| `directories` | No | - | Named directory mappings for use in templates |

**Example:**
```yaml
remote:
  hostname: jellyfin.example.com
  username: jelly
  port: 22
  root: /mnt/data/videos
  directories:
    tv_shows: tv_shows        # -> /mnt/data/videos/tv_shows
    movies: movies            # -> /mnt/data/videos/movies
```

#### Local Section

| Setting | Required | Default | Description |
|---------|----------|---------|-------------|
| `root` | Yes | - | Base directory on local machine (supports `~` and `$HOME` expansion) |
| `directories` | No | - | Named directory mappings for use in templates |

**Notes:**
- Supports tilde (`~`) and `$HOME` environment variable expansion
- Directory paths can be relative (to `root`) or absolute
- Absolute paths in `directories` override the `root` setting

**Example:**
```yaml
local:
  root: ~/Videos
  directories:
    tv_shows: "TV Shows"           # Relative -> ~/Videos/TV Shows
    movies: Movies                 # Relative -> ~/Videos/Movies
    documentaries: ~/Documentaries # Absolute -> ~/Documentaries
```

#### Jellyfin Section

Required when jobs use Jellyfin library sync. Jellysync uses a Jellyfin API key and a user ID to query media and per-user watched status. Store the API key in a separate readable file, such as a SOPS-managed secret.

| Setting | Required | Description |
|---------|----------|-------------|
| `base_url` | Yes | Jellyfin server base URL, without a trailing slash |
| `api_key_file` | Recommended | Path to a file containing a Jellyfin API key |
| `user_id` | No | Jellyfin user ID; otherwise resolved from `username` |
| `username` | With API key or password | Jellyfin account used for watched status and user lookup |
| `password_file` | Legacy only | Path to a file containing that account's password |

Jellysync sends the API key using Jellyfin's `Authorization: MediaBrowser` header. The key stays in memory for the run and is never written to the state database.

#### Library Section

| Setting | Required | Default | Description |
|---------|----------|---------|-------------|
| `season_pattern` | No | `"Season $season_number"` | Pattern for season directory names |
| `episode_pattern` | No | `"E$episode_number"` | Pattern for episode file names |

**Season Pattern:**

Available Variables:
- `$name` - Show name (from job name)
- `$season_number` - Season number (1, 2, 3, etc.)

Pattern Examples:
```yaml
library:
  # Default pattern
  season_pattern: "Season $season_number"
  # Results: "Season 1", "Season 2", "Season 3"

  # Include show name
  season_pattern: "$name - Season $season_number"
  # Results: "Breaking Bad - Season 1", "Breaking Bad - Season 2"

  # Short format
  season_pattern: "S$season_number"
  # Results: "S1", "S2", "S3"
```

**Episode Pattern:**

Available Variables:
- `$episode_number` - Episode number (1, 2, 3, etc.)

Pattern Examples:
```yaml
library:
  # Default pattern
  episode_pattern: "E$episode_number"
  # Matches: "E01", "E02", "E10"

  # Full word pattern
  episode_pattern: "Episode $episode_number"
  # Matches: "Episode 01", "Episode 02"

  # Common S##E## pattern
  episode_pattern: "S[0-9]+E$episode_number"
  # Matches: "S01E01", "S02E05"
```

**Notes:**
- `season_pattern` is used when `seasons` filtering is specified in jobs
- `episode_pattern` is used when `episodes` filtering is specified in jobs
- Patterns are used to find and match directories/files on remote server
- Must match the actual naming structure on your media server

#### Rsync Section

| Setting | Required | Default | Description |
|---------|----------|---------|-------------|
| `flags` | No | `-a -v -z` | Array of rsync command-line flags |

**Notes:**
- If not specified, uses default flags: `-a -v -z`
- Common flags: `--progress`, `--bwlimit=RATE`, `--exclude=PATTERN`
- SSH connection flags (`-e "ssh -p PORT"`) are added automatically

**Example:**
```yaml
rsync:
  flags:
    - -a              # Archive mode
    - -v              # Verbose
    - -z              # Compress
    - --progress      # Show progress
    - --bwlimit=5000  # Limit bandwidth to 5000 KB/s
```

#### Jobs Section

Each job defines a sync operation.

**Job Options:**

| Option | Required | Type | Description |
|--------|----------|------|-------------|
| `name` | Yes | string | Name of the job (used in templates as `$name`) |
| `jellyfin_name` | No | string | Jellyfin title to search when it differs from the job name |
| `remote_dir` | No | string | Remote directory path (supports templates and wildcards) |
| `local_dir` | No | string | Local directory path (supports templates) |
| `directory` | No | string | Shorthand: expands to `$directory/$name` for both remote and local |
| `seasons` | No | string or array | Season filter: `"latest"`, `"1-10"`, or `[1, 2, 3]` |
| `episodes` | No | string or array | Episode filter: `"latest"`, `"1-10"`, or `[1, 2, 3]` |
| `wildcard` | No | boolean | If `true`, adds `*name*` pattern to remote path |
| `unwatched` | No | boolean | If `true`, syncs episodes marked unplayed by the configured Jellyfin user |

**Season Filtering:**

The `seasons` option allows selective syncing of TV show seasons:

| Format | Example | Description |
|--------|---------|-------------|
| String (latest) | `"latest"` | Syncs only the most recent season |
| String (latest-N) | `"latest-3"` | Syncs the latest 3 seasons |
| String (range) | `"1-10"` | Syncs seasons 1 through 10 (inclusive) |
| Array (list) | `[1, 2, 5]` | Syncs only seasons 1, 2, and 5 |

**Notes:**
- Season directories are matched using the `library.season_pattern` setting
- Default pattern is `"Season $season_number"` (e.g., "Season 1", "Season 2")
- Pattern must match actual directory names on remote server

**Episode Filtering:**

The `episodes` option allows selective syncing of episodes within seasons:

| Format | Example | Description |
|--------|---------|-------------|
| String (latest) | `"latest"` | Syncs only the most recent episode |
| String (latest-N) | `"latest-5"` | Syncs the latest 5 episodes |
| String (range) | `"1-10"` | Syncs episodes 1 through 10 (inclusive) |
| Array (list) | `[1, 2, 5]` | Syncs only episodes 1, 2, and 5 |

**Notes:**
- Episode files are matched using the `library.episode_pattern` setting
- Default pattern is `"E$episode_number"` (e.g., "E01", "E02")
- Can be combined with `seasons` to filter both seasons and episodes
- Can be used without `seasons` for single-directory content
- Pattern must match actual file names on remote server
- `unwatched: true` requires the Jellyfin section and cannot be combined with `episodes`; it can be combined with `seasons`
- If `seasons` is omitted, `unwatched: true` scans all seasons

**Syntax options:**

| Option | Description | Use Case |
|--------|-------------|----------|
| **Explicit paths** | Define both `remote_dir` and `local_dir` | When remote and local paths differ |
| **Shorthand** | Use `directory` key only | When both sides have the same relative path |
| **Templates** | Use `$name` and `$directory` variables | For dynamic path construction |

**1. Explicit paths:**
```yaml
- name: My Show
  remote_dir: $tv_shows/My Show
  local_dir: $tv_shows/My Show
```

**2. Shorthand syntax:**
```yaml
- name: My Show
  directory: tv_shows  # Expands to $tv_shows/$name on both remote and local
```

**3. Template variables:**
- `$name`: Replaced with job name
- `$tv_shows`, `$movies`, etc.: Replaced with directory mappings

```yaml
- name: The Penguin
  remote_dir: "$tv_shows/$name/Season 1"
  local_dir: "$tv_shows/$name - Season 1"
```

**4. Wildcard support:**
Remote paths support wildcards (`*` and `?`) for pattern matching. The first matching directory will be used.

```yaml
# Explicit wildcard pattern
- name: The Paper (2025)
  remote_dir: "$tv_shows/The Paper*"  # Resolves to first match, e.g., "The Paper (2025)"
  local_dir: "$tv_shows/The Paper (2025)"

# Wildcard flag (automatically adds *name* pattern)
- name: The Paper
  directory: tv_shows
  wildcard: true  # Will match *The Paper* on remote
```

**5. Season and Episode Filtering:**

Combine season and episode filters for precise control:

```yaml
# Sync episodes 1-5 of season 1
- name: The Office
  directory: tv_shows
  seasons: "1"
  episodes: "1-5"

# Sync latest episode from seasons 1 and 2
- name: Friends
  directory: tv_shows
  seasons: [1, 2]
  episodes: "latest"

# Sync latest 3 episodes from latest 2 seasons
- name: Modern Family
  directory: tv_shows
  seasons: "latest-2"
  episodes: "latest-3"

# Sync specific episodes from latest season
- name: Seinfeld
  directory: tv_shows
  seasons: "latest"
  episodes: [1, 2, 10]

# Episodes without seasons (single directory)
- name: Stand-up Special
  remote_dir: "$movies/Stand-up Special"
  local_dir: "$movies/Stand-up Special"
  episodes: "1-3"
```

## Usage

```bash
jellysync --help
jellysync download
jellysync download pluribus
jellysync download "Star Trek" "The Penguin"
jellysync status
jellysync tui
jellysync prune
jellysync prune --apply
jellysync --config /path/to/config.yaml config
```

## Options

- `-c, --config FILE`: Specify config file path (default: `jellysync.yaml`)
- `--parallelism N`: Override the concurrent worker limit
- `sync [JOB...]`: Sync all jobs or selected jobs
- `status`: Show latest job state and systemd timer state
- `prune [--apply] [JOB...]`: Preview deletions, or apply them
- `tui`: Select a job to see its files; `s` syncs that job, `S` syncs all jobs, `b` opens Jellyfin Explore, and `o` opens the job's download directory. Explore searches the library, renders posters with `ratatui-image` (including Kitty graphics protocol support), shows a series' episodes in the details pane (`Tab` to focus, `Space`/`a` to select), and downloads movies or selected episodes with `d`. In the main view, `x` (file), `X` (season), `c` (show), then `y` clears media; `x` on an ad-hoc job removes it. `p` or a double-click plays a file. `i` shows the job configuration, or, with a file selected, `i`/`Enter` show a short `ffprobe` summary (container, duration, size, bitrate, video/audio/subtitle streams), plus the episode still, title, air date, rating, runtime, watched state and overview from Jellyfin when online; `p` plays the file from there. Mouse clicks and scrolling select rows.
- `-h, --help`: Show help message
- `--version`: Show version

## Configuration File Locations

jellysync searches for configuration files in the following order:

1. **`JELLYSYNC_CONFIG` environment variable** (if set)
2. **`./jellysync.yaml`** (current directory)
3. **`~/.config/jellysync/config.yaml`** (user config directory)
4. Falls back to `./jellysync.yaml` (will error if not found)

The active configuration file path is displayed when jellysync runs.

**Examples:**
```bash
# Use default search order
jellysync config

# Use specific config file
jellysync --config /path/to/config.yaml config

# Use environment variable
JELLYSYNC_CONFIG=~/my-config.yaml jellysync config
```

## Environment Variables

- `JELLYSYNC_CONFIG`: Override config file path (highest priority)

## How It Works

1. Reads YAML configuration file
2. Expands path templates with variables
3. Resolves relative/absolute paths
4. Uses `rsync` over SSH to sync files
5. Creates local directories as needed

### Rsync Options

The Rust tool uses these rsync options:
- `-a`: Archive mode (preserves permissions, timestamps, etc.)
- `-v`: Verbose output (or `--info=progress2` in non-verbose mode)
- `-z`: Compress during transfer
- `--partial`: Keep interrupted transfers so a later run can resume them

## Examples

### Sync Everything
```bash
./jellysync
```

### Sync One Show
```bash
./jellysync "Star Trek"
```

### Preview Changes
```bash
jellysync prune
```

### Sync Multiple Specific Jobs
```bash
jellysync download pluribus "The Penguin"
```

## Requirements

- Jellysync Rust executable
- `rsync`
- `ssh` access to remote server
- SSH key authentication recommended

**Note:** When using Nix, all dependencies are automatically provided.

## Tips

- Set up SSH key authentication to avoid password prompts
- Use `jellysync prune` to preview deletions before applying them
- Absolute paths in `local.directories` override the `local.root`

## License

This project is licensed under the GNU General Public License v3.0 (GPLv3).

See [LICENSE](LICENSE) for details.
