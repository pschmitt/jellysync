# jellysync

A bash tool to sync files from an SSH server to local directories with flexible configuration.

## Features

- 📁 Template-based path configuration with variable substitution
- 🔄 Sync specific jobs or all at once
- 🎯 Shorthand syntax for common patterns
- 🔍 Dry-run mode to preview changes
- 📊 Progress reporting with colored output
- ⚙️ Flexible YAML configuration

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

### Manual Installation

1. Clone this repository
2. Ensure dependencies are installed: `bash`, `yq` (Go version), `rsync`, `ssh`
3. Copy `jellysync-config.sample.yaml` to `jellysync.yaml` and configure it
4. Run `./jellysync`

```bash
# Run
./jellysync --help
```

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

library:
  season_pattern: "Season $season_number"

rsync:
  flags:
    - -a
    - -v
    - -z
    - --delete

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

#### Library Section

| Setting | Required | Default | Description |
|---------|----------|---------|-------------|
| `season_pattern` | No | `"Season $season_number"` | Pattern for season directory names |

**Available Variables:**
- `$name` - Show name (from job name)
- `$season_number` - Season number (1, 2, 3, etc.)

**Pattern Examples:**
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

**Notes:**
- Used when `seasons` filtering is specified in jobs
- Pattern is used to find and match season directories on remote server
- Must match the actual directory structure on your media server

#### Rsync Section

| Setting | Required | Default | Description |
|---------|----------|---------|-------------|
| `flags` | No | `-a -v -z --delete` | Array of rsync command-line flags |

**Notes:**
- If not specified, uses default flags: `-a -v -z --delete`
- Common flags: `--progress`, `--bwlimit=RATE`, `--exclude=PATTERN`
- SSH connection flags (`-e "ssh -p PORT"`) are added automatically

**Example:**
```yaml
rsync:
  flags:
    - -a              # Archive mode
    - -v              # Verbose
    - -z              # Compress
    - --delete        # Delete extraneous files
    - --progress      # Show progress
    - --bwlimit=5000  # Limit bandwidth to 5000 KB/s
```

#### Jobs Section

Each job defines a sync operation.

**Job Options:**

| Option | Required | Type | Description |
|--------|----------|------|-------------|
| `name` | Yes | string | Name of the job (used in templates as `$name`) |
| `remote_dir` | No | string | Remote directory path (supports templates and wildcards) |
| `local_dir` | No | string | Local directory path (supports templates) |
| `directory` | No | string | Shorthand: expands to `$directory/$name` for both remote and local |
| `seasons` | No | string or array | Season filter: `"latest"`, `"1-10"`, or `[1, 2, 3]` |
| `wildcard` | No | boolean | If `true`, adds `*name*` pattern to remote path |

**Season Filtering:**

The `seasons` option allows selective syncing of TV show seasons:

| Format | Example | Description |
|--------|---------|-------------|
| String (latest) | `"latest"` | Syncs only the most recent season |
| String (range) | `"1-10"` | Syncs seasons 1 through 10 (inclusive) |
| Array (list) | `[1, 2, 5]` | Syncs only seasons 1, 2, and 5 |

**Notes:**
- Season directories are matched using the `library.season_pattern` setting
- Default pattern is `"Season $season_number"` (e.g., "Season 1", "Season 2")
- Pattern must match actual directory names on remote server

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

## Usage

```bash
# Show help
./jellysync --help

# List all configured jobs
./jellysync --list

# Sync all jobs
./jellysync

# Sync specific job(s)
./jellysync pluribus
./jellysync "Star Trek" "The Penguin"

# Dry run (preview changes)
./jellysync --dry-run
./jellysync -n pluribus

# Verbose output
./jellysync --verbose
./jellysync -v

# Use custom config file
./jellysync --config /path/to/config.yaml
```

## Options

- `-c, --config FILE`: Specify config file path (default: `jellysync.yaml`)
- `-n, --dry-run`: Preview changes without syncing
- `-v, --verbose`: Show detailed output
- `-l, --list`: List all configured jobs
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
./jellysync --list

# Use specific config file
./jellysync --config /path/to/config.yaml

# Use environment variable
JELLYSYNC_CONFIG=~/my-config.yaml ./jellysync
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

The tool uses these rsync options:
- `-a`: Archive mode (preserves permissions, timestamps, etc.)
- `-v`: Verbose output (or `--info=progress2` in non-verbose mode)
- `-z`: Compress during transfer
- `--delete`: Remove files from destination that don't exist in source

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
./jellysync --dry-run --verbose
```

### Sync Multiple Specific Jobs
```bash
./jellysync pluribus "The Penguin"
```

## Requirements

- `bash` 4.0+
- `yq` (Go version: https://github.com/mikefarah/yq)
- `rsync`
- `ssh` access to remote server
- SSH key authentication recommended

**Note:** When using Nix, all dependencies are automatically provided.

## Tips

- Set up SSH key authentication to avoid password prompts
- Use `--dry-run` first to preview changes
- The `--delete` flag removes files locally that don't exist remotely
- Absolute paths in `local.directories` override the `local.root`

## License

This project is licensed under the GNU General Public License v3.0 (GPLv3).

See [LICENSE](LICENSE) for details.
