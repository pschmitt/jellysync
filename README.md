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
# Make executable
chmod +x jellysync

# Run
./jellysync
```

## Configuration

Create a `jellysync.yaml` file with your sync configuration:

```yaml
remote:
  hostname: tv.brkn.lol
  username: pschmitt
  port: 22
  root: /mnt/data/videos  # all remote paths are relative to this root dir
  directories:
    tv_shows: tv_shows  # relative path -> /mnt/data/videos/tv_shows
    movies: movies
    documentaries: documentaries

local:
  root: /home/pschmitt/Videos  # all local paths are relative to this dir
  directories:
    tv_shows: "TV Shows"  # relative path -> /home/pschmitt/Videos/TV Shows
    movies: Movies
    documentaries: /home/pschmitt/Documentaries  # absolute path

jobs:
  # Sync all of pluribus
  - name: pluribus
    remote_dir: $tv_shows/Pluribus  # templated -> /mnt/data/videos/tv_shows/Pluribus
    local_dir: $tv_shows/Pluribus   # templated -> /home/pschmitt/Videos/TV Shows/Pluribus

  # Sync all of Star Trek (shorthand syntax)
  - name: Star Trek
    directory: tv_shows  # shorthand for remote_dir: $tv_shows/$name + local_dir: $tv_shows/$name

  # Sync season 1 of The Penguin
  - name: The Penguin
    remote_dir: "$tv_shows/$name/Season 1"
    local_dir: "$tv_shows/The Penguin - Season 1"
```

### Configuration Sections

#### Remote Section
- `hostname`: SSH server hostname
- `username`: SSH username
- `port`: SSH port (default: 22)
- `root`: Base directory on remote server
- `directories`: Named directory mappings for templates

#### Local Section
- `root`: Base directory on local machine
- `directories`: Named directory mappings for templates
  - Can be relative (to root) or absolute paths

#### Jobs Section

Each job defines a sync operation. Three syntax options:

1. **Explicit paths**: Define both `remote_dir` and `local_dir`
   ```yaml
   - name: My Show
     remote_dir: $tv_shows/My Show
     local_dir: $tv_shows/My Show
   ```

2. **Shorthand syntax**: Use `directory` for same relative path on both sides
   ```yaml
   - name: My Show
     directory: tv_shows  # Expands to $tv_shows/$name on both sides
   ```

3. **Template variables**:
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
