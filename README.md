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

1. Clone this repository
2. Copy `jellysync.yaml.example` to `jellysync.yaml` and configure it
3. Run `./jellysync`

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

#### Jobs Section

Each job defines a sync operation. Three syntax options:

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

## Environment Variables

- `JELLYSYNC_CONFIG`: Default config file path (overrides `./jellysync.yaml`)

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

## Tips

- Set up SSH key authentication to avoid password prompts
- Use `--dry-run` first to preview changes
- The `--delete` flag removes files locally that don't exist remotely
- Absolute paths in `local.directories` override the `local.root`

## License

MIT
