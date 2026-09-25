// jellysync - Sync media from SSH or Jellyfin
// Copyright (C) 2026 pschmitt
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::style::{Color as TerminalColor, Stylize};
use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers, MouseButton, MouseEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures_util::StreamExt;
use image::imageops::FilterType;
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph, Scrollbar,
        ScrollbarOrientation, ScrollbarState, Wrap,
    },
};
use ratatui_image::{
    Resize, StatefulImage,
    picker::Picker,
    protocol::StatefulProtocol,
    thread::{ResizeRequest, ThreadProtocol},
};
use reqwest::{Client as HttpClient, StatusCode};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    env,
    io::stdout,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{process::Command, sync::Semaphore, task::JoinSet};

/// Print a CLI line, stripping ANSI styling when stdout is not a TTY or NO_COLOR is set.
macro_rules! say {
    ($($arg:tt)*) => {
        cli_print(format!($($arg)*))
    };
}

const HELP_STYLES: clap::builder::Styles = {
    use clap::builder::styling::{AnsiColor, Effects, Styles};
    Styles::styled()
        .header(AnsiColor::Magenta.on_default().effects(Effects::BOLD))
        .usage(AnsiColor::Magenta.on_default().effects(Effects::BOLD))
        .literal(AnsiColor::Cyan.on_default().effects(Effects::BOLD))
        .placeholder(AnsiColor::Yellow.on_default())
        .valid(AnsiColor::Green.on_default())
        .invalid(AnsiColor::Red.on_default().effects(Effects::BOLD))
        .error(AnsiColor::Red.on_default().effects(Effects::BOLD))
};

/// The CLI as completions see it. clap_complete's static generators ignore
/// `hide`, so hidden (internal) subcommands are left out explicitly.
fn completion_command() -> clap::Command {
    use clap::CommandFactory as _;
    let full = Cli::command();
    let mut command = clap::Command::new("jellysync")
        .version(version_string())
        .args(full.get_arguments().cloned())
        .subcommands(
            full.get_subcommands()
                .filter(|sub| !sub.is_hide_set())
                .cloned(),
        );
    if let Some(about) = full.get_about() {
        command = command.about(about.clone());
    }
    command
}

/// Git revision from build.rs: empty on a tagged release, otherwise the short
/// commit, with `-dirty` for uncommitted changes.
const GIT_REVISION: &str = env!("JELLYSYNC_REVISION");

/// `1.1.0` on a tagged release, `1.1.0-135343d` or `1.1.0-135343d-dirty` otherwise.
fn version_string() -> &'static str {
    static VERSION: std::sync::LazyLock<String> =
        std::sync::LazyLock::new(|| format_version(env!("CARGO_PKG_VERSION"), GIT_REVISION));
    &VERSION
}

fn format_version(version: &str, revision: &str) -> String {
    if revision.is_empty() {
        version.to_string()
    } else {
        format!("{version}-{revision}")
    }
}

#[derive(Parser)]
#[command(
    name = "jellysync",
    version = version_string(),
    about = "◆ jellysync · keep a local copy of your Jellyfin shows and movies",
    long_about = None,
    styles = HELP_STYLES,
    after_help = "Run without a command to open the TUI."
)]
struct Cli {
    /// Config file (default: $JELLYSYNC_CONFIG, ./jellysync.yaml, ~/.config/jellysync/config.yaml)
    #[arg(short, long, global = true, value_name = "FILE", value_hint = clap::ValueHint::FilePath)]
    config: Option<PathBuf>,
    /// Number of jobs downloaded concurrently (overrides the config)
    #[arg(
        short = 'j',
        long,
        visible_alias = "parallel",
        global = true,
        value_name = "N"
    )]
    parallelism: Option<usize>,
    /// Print machine-readable JSON
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Open the interactive terminal UI (default)
    Tui,
    /// Download all jobs, or only the given ones
    #[command(visible_aliases = ["fetch", "sync"])]
    Download {
        /// Job names to download (default: all jobs); `auto` works without a
        /// configured auto job: the newest unwatched items in Jellyfin
        #[arg(value_name = "JOB")]
        target: Vec<String>,
        /// Item limit for auto jobs (overrides their max_items)
        #[arg(long, value_name = "N")]
        max_items: Option<usize>,
        /// Size budget for auto jobs in GiB (overrides their max_size)
        #[arg(long, value_name = "GIB")]
        max_size: Option<f64>,
    },
    /// Show recent job status and the timer state
    Status,
    /// Delete tracked files whose media is gone from Jellyfin (or the remote, in
    /// rsync mode) and watched files past their grace period (jobs with delete_watched)
    Prune {
        /// Only show what would be deleted
        #[arg(short = 'k', long = "dry-run", visible_alias = "dryrun")]
        dry_run: bool,
        /// Also delete watched files of jobs without delete_watched (after the grace period)
        #[arg(long)]
        watched: bool,
        /// Former opt-in to deleting; prune deletes by default now (see --dry-run)
        #[arg(long, hide = true)]
        apply: bool,
        /// Job names to prune (default: all jobs)
        #[arg(value_name = "JOB")]
        target: Vec<String>,
    },
    /// Print the parsed configuration
    Config,
    /// Print the version (same as --version / -V)
    Version,
    /// Print a shell completion script (the Nix package installs them)
    Completions {
        /// Shell to generate the completion script for
        shell: clap_complete::Shell,
    },
    /// Start the jellysync user service
    Start,
    /// Stop the jellysync user service
    Stop,
    /// Download the pending transfers of an ad-hoc library job (spawned by the TUI)
    #[command(hide = true)]
    Worker { job: String },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Config {
    remote: Remote,
    local: Local,
    #[serde(default)]
    download: Download,
    #[serde(default = "default_parallelism")]
    parallelism: usize,
    #[serde(default = "default_player")]
    player: String,
    /// Command used to open download directories; xdg-open or gio open when unset.
    #[serde(default)]
    file_manager: Option<String>,
    #[serde(default)]
    rsync: Rsync,
    #[serde(default)]
    jellyfin: Option<Jellyfin>,
    #[serde(default)]
    library: Option<Library>,
    #[serde(default)]
    cleanup: Option<Cleanup>,
    #[serde(default)]
    jobs: Vec<Job>,
}
/// Automatic removal of watched downloads (for jobs with `delete_watched`).
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct Cleanup {
    /// How long watched files are kept before they are deleted, e.g. `7d`.
    #[serde(default, deserialize_with = "duration_spec")]
    delete_watched_after: Option<String>,
}

/// A duration setting written as text (`7d`) or a bare number of days (`0`).
fn duration_spec<'de, D>(deserializer: D) -> std::result::Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Spec {
        Text(String),
        Days(u64),
    }
    Ok(
        Option::<Spec>::deserialize(deserializer)?.map(|spec| match spec {
            Spec::Text(text) => text,
            Spec::Days(days) => days.to_string(),
        }),
    )
}
/// Default grace period before watched files are deleted.
const DEFAULT_DELETE_WATCHED_AFTER: &str = "7d";
#[derive(Clone, Debug, Deserialize, Serialize)]
struct Remote {
    hostname: String,
    username: String,
    #[serde(default = "default_port")]
    port: u16,
    root: String,
    #[serde(default)]
    directories: BTreeMap<String, String>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
struct Local {
    root: String,
    #[serde(default)]
    directories: BTreeMap<String, String>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
struct Download {
    mode: String,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
struct Rsync {
    #[serde(default = "default_rsync_flags")]
    flags: Vec<String>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
struct Jellyfin {
    base_url: String,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    password_file: Option<String>,
    #[serde(default)]
    api_key_file: Option<String>,
    #[serde(default)]
    user_id: Option<String>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
struct Library {
    #[serde(default = "default_season_pattern")]
    season_pattern: String,
}
fn default_season_pattern() -> String {
    "Season $season_number".into()
}
#[derive(Clone, Debug, Deserialize, Serialize)]
struct Job {
    name: String,
    #[serde(default)]
    jellyfin_name: Option<String>,
    remote_dir: Option<String>,
    local_dir: Option<String>,
    directory: Option<String>,
    seasons: Option<serde_yaml::Value>,
    episodes: Option<serde_yaml::Value>,
    wildcard: Option<bool>,
    unwatched: Option<bool>,
    /// false skips the job when syncing all jobs; it can still be synced by name.
    #[serde(default)]
    enabled: Option<bool>,
    /// Delete downloaded files once they are watched (after the grace period)
    /// and do not download watched items again.
    #[serde(default)]
    delete_watched: Option<bool>,
    /// Per-job grace period, overriding `cleanup.delete_watched_after`.
    #[serde(default, deserialize_with = "duration_spec")]
    delete_watched_after: Option<String>,
    /// Instead of a title: the newest unwatched movies and/or episodes in
    /// Jellyfin (`true`/`all`, `movies`, `shows`), up to `max_items` (5 when no
    /// limit is set) and/or `max_size`.
    #[serde(default, deserialize_with = "auto_kind")]
    auto: Option<AutoKind>,
    #[serde(default)]
    max_items: Option<usize>,
    /// Size budget in GiB.
    #[serde(default)]
    max_size: Option<f64>,
    /// Restrict an auto job to one Jellyfin library (by name).
    #[serde(default)]
    library: Option<String>,
}

/// What an auto job picks from.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
enum AutoKind {
    All,
    Movies,
    Shows,
}

impl AutoKind {
    fn item_types(self) -> &'static str {
        match self {
            AutoKind::All => "Movie,Episode",
            AutoKind::Movies => "Movie",
            AutoKind::Shows => "Episode",
        }
    }
}

/// `auto: true`/`all`, `movies` or `shows` (`false` or absent: not an auto job).
fn auto_kind<'de, D>(deserializer: D) -> std::result::Result<Option<AutoKind>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Spec {
        Flag(bool),
        Kind(String),
    }
    match Option::<Spec>::deserialize(deserializer)? {
        None | Some(Spec::Flag(false)) => Ok(None),
        Some(Spec::Flag(true)) => Ok(Some(AutoKind::All)),
        Some(Spec::Kind(kind)) => match kind.trim().to_lowercase().as_str() {
            "all" | "true" | "yes" => Ok(Some(AutoKind::All)),
            "movies" | "movie" => Ok(Some(AutoKind::Movies)),
            "shows" | "show" | "tv" | "series" | "episodes" => Ok(Some(AutoKind::Shows)),
            "false" | "no" | "" => Ok(None),
            other => Err(serde::de::Error::custom(format!(
                "auto must be true, all, movies or shows, not '{other}'"
            ))),
        },
    }
}

/// max_items for auto jobs that set no limit at all.
const DEFAULT_AUTO_MAX_ITEMS: usize = 5;

impl Job {
    fn enabled(&self) -> bool {
        self.enabled != Some(false)
    }

    fn is_auto(&self) -> bool {
        self.auto.is_some()
    }

    /// An auto job's item limit: its own, or the default when it sets no limit.
    fn auto_max_items(&self) -> Option<usize> {
        match (self.max_items, self.max_size) {
            (None, None) => Some(DEFAULT_AUTO_MAX_ITEMS),
            (max_items, _) => max_items,
        }
    }

    /// Auto jobs delete watched files unless told otherwise; others only on request.
    fn deletes_watched(&self) -> bool {
        self.delete_watched.unwrap_or(self.is_auto())
    }

    /// Grace period before this job's watched files are deleted, if it deletes them.
    fn watched_grace(&self, config: &Config) -> Option<Duration> {
        if !self.deletes_watched() {
            return None;
        }
        let spec = self
            .delete_watched_after
            .as_deref()
            .or(config
                .cleanup
                .as_ref()
                .and_then(|cleanup| cleanup.delete_watched_after.as_deref()))
            .unwrap_or(DEFAULT_DELETE_WATCHED_AFTER);
        // Validated when the config is loaded.
        parse_duration(spec).ok()
    }
}

/// A duration like `7d`, `12h`, `30m`, `45s` or `0`; a bare number means days.
fn parse_duration(spec: &str) -> Result<Duration> {
    let spec = spec.trim();
    let (number, unit) = match spec.find(|c: char| !c.is_ascii_digit()) {
        Some(split) => spec.split_at(split),
        None => (spec, "d"),
    };
    let number: u64 = number
        .parse()
        .with_context(|| format!("invalid duration '{spec}' (e.g. 7d, 12h, 30m)"))?;
    let seconds = match unit.trim() {
        "d" | "day" | "days" => 86_400,
        "h" | "hour" | "hours" => 3600,
        "m" | "min" | "minutes" => 60,
        "s" | "sec" | "seconds" => 1,
        _ => bail!("invalid duration '{spec}' (e.g. 7d, 12h, 30m)"),
    };
    Ok(Duration::from_secs(number.saturating_mul(seconds)))
}

/// "7d", "5h", "12m" for a remaining time, rounded up: something watched a
/// minute ago with a 7-day grace period is "deleted in 7d", not "6d".
/// "1 item", "2 items".
fn plural(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("1 {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

fn format_duration_short(seconds: u64) -> String {
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m", seconds.div_ceil(60)),
        3600..86_400 => format!("{}h", seconds.div_ceil(3600)),
        _ => format!("{}d", seconds.div_ceil(86_400)),
    }
}
impl Default for Download {
    fn default() -> Self {
        Self {
            mode: "jellyfin".into(),
        }
    }
}
impl Default for Rsync {
    fn default() -> Self {
        Self {
            flags: default_rsync_flags(),
        }
    }
}
fn default_parallelism() -> usize {
    2
}
fn default_player() -> String {
    "mpv".into()
}
fn default_port() -> u16 {
    22
}
fn default_rsync_flags() -> Vec<String> {
    vec!["-a".into(), "-v".into(), "-z".into()]
}

fn config_path(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = explicit.or_else(|| env::var_os("JELLYSYNC_CONFIG").map(PathBuf::from)) {
        return Ok(path);
    }
    let cwd = PathBuf::from("jellysync.yaml");
    if cwd.is_file() {
        return Ok(cwd);
    }
    Ok(dirs::config_dir()
        .context("cannot determine config directory")?
        .join("jellysync/config.yaml"))
}
fn load_config(path: &Path) -> Result<Config> {
    load_config_with_overlay(path, &state_config_path()?)
}

/// The writable settings overlay. The main config is often read-only (e.g. a
/// Nix store symlink from the Home Manager module); settings changed in the TUI
/// then land here and are merged on top of it by every command.
fn state_config_path() -> Result<PathBuf> {
    Ok(state_dir()?.join("config.yaml"))
}

fn read_yaml(path: &Path) -> Result<serde_yaml::Value> {
    let content =
        std::fs::read_to_string(path).with_context(|| format!("read config {}", path.display()))?;
    let value: serde_yaml::Value = serde_yaml::from_str(&content)
        .with_context(|| format!("parse YAML config {}", path.display()))?;
    // An empty file parses as null; treat it as an empty mapping.
    Ok(if value.is_null() {
        serde_yaml::Value::Mapping(Default::default())
    } else {
        value
    })
}

/// Deep-merge `overlay` into `base`. Mappings merge key by key, lists of named
/// entries (the jobs) merge by `name`, and anything else — including `null`,
/// which unsets a value — replaces what was there.
fn merge_config(base: &mut serde_yaml::Value, overlay: serde_yaml::Value) {
    use serde_yaml::Value;
    let named = |items: &[Value]| items.iter().all(|item| item.get("name").is_some());
    match (base, overlay) {
        (Value::Mapping(base), Value::Mapping(overlay)) => {
            for (key, value) in overlay {
                match base.get_mut(&key) {
                    Some(existing) => merge_config(existing, value),
                    None => {
                        base.insert(key, value);
                    }
                }
            }
        }
        (Value::Sequence(base), Value::Sequence(overlay)) if named(base) && named(&overlay) => {
            for item in overlay {
                let name = item.get("name").cloned();
                match base
                    .iter_mut()
                    .find(|entry| entry.get("name") == name.as_ref())
                {
                    Some(existing) => merge_config(existing, item),
                    None => base.push(item),
                }
            }
        }
        (base, overlay) => *base = overlay,
    }
}

fn load_config_with_overlay(path: &Path, overlay: &Path) -> Result<Config> {
    let mut value = read_yaml(path)?;
    let same_file = path.canonicalize().ok() == overlay.canonicalize().ok();
    if overlay.is_file() && !same_file {
        merge_config(&mut value, read_yaml(overlay)?);
    }
    parse_config(value).with_context(|| {
        if overlay.is_file() && !same_file {
            format!("with settings overlay {}", overlay.display())
        } else {
            format!("in {}", path.display())
        }
    })
}

fn parse_config(value: serde_yaml::Value) -> Result<Config> {
    let mut config: Config = serde_yaml::from_value(value).context("parse YAML config")?;
    if config.parallelism == 0 {
        bail!("parallelism must be at least 1");
    }
    if config.download.mode != "rsync" && config.download.mode != "jellyfin" {
        bail!("download.mode must be either 'rsync' or 'jellyfin'");
    }
    if let Some(spec) = config
        .cleanup
        .as_ref()
        .and_then(|cleanup| cleanup.delete_watched_after.as_deref())
    {
        parse_duration(spec).context("cleanup.delete_watched_after")?;
    }
    for job in config.jobs.iter().filter(|job| job.is_auto()) {
        if job
            .max_size
            .is_some_and(|size| !size.is_finite() || size <= 0.0)
        {
            bail!(
                "auto job '{}': max_size must be a positive number of GiB",
                job.name
            );
        }
        if config.download.mode != "jellyfin" {
            bail!("auto job '{}' needs Jellyfin download mode", job.name);
        }
    }
    for job in &config.jobs {
        if let Some(spec) = job.delete_watched_after.as_deref() {
            parse_duration(spec)
                .with_context(|| format!("job '{}': delete_watched_after", job.name))?;
        }
    }
    config.remote.root = expand_home(&config.remote.root);
    config.local.root = expand_home(&config.local.root);
    Ok(config)
}
fn expand_home(value: &str) -> String {
    let home = env::var("HOME").unwrap_or_default();
    let value = value.replace("$HOME", &home);
    // Only a bare `~` or `~/…`; `~user/…` names another user's home.
    if value == "~" {
        home
    } else if let Some(rest) = value.strip_prefix("~/") {
        format!("{home}/{rest}")
    } else {
        value
    }
}
fn state_dir() -> Result<PathBuf> {
    let base = env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".local/state")))
        .context("cannot determine state directory")?;
    let dir = base.join("jellysync");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}
fn state_db() -> Result<PathBuf> {
    Ok(state_dir()?.join("state.db"))
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum SettingKind {
    Text,
    Filter,
    Toggle,
    Duration,
    /// Enter cycles through the values; the first one clears the setting.
    Choice(&'static [&'static str]),
    Number,
    /// A size in GiB.
    Gib,
}

/// A job setting that the TUI can edit.
#[derive(Clone, Copy)]
struct JobSetting {
    key: &'static str,
    label: &'static str,
    kind: SettingKind,
    help: &'static str,
}

const AUTO_SETTING: JobSetting = JobSetting {
    key: "auto",
    label: "Auto (newest)",
    kind: SettingKind::Choice(&["off", "all", "movies", "shows"]),
    help: "instead of a title: the newest unwatched movies and/or episodes in Jellyfin",
};

/// Settings of an auto job: what it picks and its limits, plus cleanup.
fn auto_job_settings() -> Vec<JobSetting> {
    vec![
        AUTO_SETTING,
        JobSetting {
            key: "max_items",
            label: "Max items",
            kind: SettingKind::Number,
            help: "keep at most this many of the newest items (5 when no limit is set)",
        },
        JobSetting {
            key: "max_size",
            label: "Max size (GiB)",
            kind: SettingKind::Gib,
            help: "keep the newest items that fit this size budget, e.g. 30 or 7.5",
        },
        JobSetting {
            key: "library",
            label: "Library",
            kind: SettingKind::Text,
            help: "only pick from this Jellyfin library (by name); empty: all libraries",
        },
        JobSetting {
            key: "enabled",
            label: "Enabled",
            kind: SettingKind::Toggle,
            help: "disabled jobs are skipped when syncing all jobs",
        },
        JobSetting {
            key: "delete_watched",
            label: "Delete watched",
            kind: SettingKind::Toggle,
            help: "auto jobs delete watched files after the grace period unless this is off",
        },
        JobSetting {
            key: "delete_watched_after",
            label: "Keep watched for",
            kind: SettingKind::Duration,
            help: "grace period before watched files are deleted, e.g. 7d, 12h (default: cleanup setting, 7d)",
        },
    ]
}

fn job_settings(mode: &str, job: &Job) -> Vec<JobSetting> {
    if job.is_auto() {
        return auto_job_settings();
    }
    let mut settings = vec![
        JobSetting {
            key: "jellyfin_name",
            label: "Jellyfin name",
            kind: SettingKind::Text,
            help: "title to look up in Jellyfin (defaults to the job name)",
        },
        JobSetting {
            key: "seasons",
            label: "Seasons",
            kind: SettingKind::Filter,
            help: "all, latest, latest-2, 3, 1-4 or 1,3,5",
        },
        JobSetting {
            key: "episodes",
            label: "Episodes",
            kind: SettingKind::Filter,
            help: "per season: all, latest, latest-3, 1-6 or 1,2",
        },
        JobSetting {
            key: "enabled",
            label: "Enabled",
            kind: SettingKind::Toggle,
            help: "disabled jobs are skipped when syncing all jobs",
        },
        JobSetting {
            key: "delete_watched",
            label: "Delete watched",
            kind: SettingKind::Toggle,
            help: "delete files once watched (after the grace period); watched items are not downloaded again",
        },
        JobSetting {
            key: "delete_watched_after",
            label: "Keep watched for",
            kind: SettingKind::Duration,
            help: "grace period before watched files are deleted, e.g. 7d, 12h (default: cleanup setting, 7d)",
        },
        JobSetting {
            key: "unwatched",
            label: "Unwatched only",
            kind: SettingKind::Toggle,
            help: "only download episodes not yet played in Jellyfin",
        },
    ];
    // Remote path wildcards only exist for rsync transfers.
    if mode == "rsync" {
        settings.push(JobSetting {
            key: "wildcard",
            label: "Wildcard",
            kind: SettingKind::Toggle,
            help: "match the remote directory as *name*",
        });
    } else {
        settings.push(AUTO_SETTING);
    }
    settings
}

/// The current value of a text or filter setting, as the user would type it.
fn job_setting_text(job: &Job, key: &str) -> Option<String> {
    let yaml = |value: Option<&serde_yaml::Value>| {
        value.map(|value| match value {
            serde_yaml::Value::String(text) => text.clone(),
            serde_yaml::Value::Sequence(items) => items
                .iter()
                .map(|item| match item {
                    serde_yaml::Value::String(text) => text.clone(),
                    other => serde_yaml::to_string(other)
                        .unwrap_or_default()
                        .trim()
                        .to_string(),
                })
                .collect::<Vec<_>>()
                .join(","),
            other => serde_yaml::to_string(other)
                .unwrap_or_default()
                .trim()
                .to_string(),
        })
    };
    match key {
        "jellyfin_name" => job.jellyfin_name.clone(),
        "delete_watched_after" => job.delete_watched_after.clone(),
        "auto" => job.auto.map(|kind| {
            match kind {
                AutoKind::All => "all",
                AutoKind::Movies => "movies",
                AutoKind::Shows => "shows",
            }
            .to_string()
        }),
        "max_items" => job.max_items.map(|max| max.to_string()),
        "max_size" => job.max_size.map(|max| format!("{max}")),
        "library" => job.library.clone(),
        "seasons" => yaml(job.seasons.as_ref()),
        "episodes" => yaml(job.episodes.as_ref()),
        _ => None,
    }
}

fn job_setting_bool(job: &Job, key: &str) -> Option<bool> {
    match key {
        "unwatched" => job.unwatched,
        "wildcard" => job.wildcard,
        "enabled" => Some(job.enabled()),
        "delete_watched" => Some(job.deletes_watched()),
        _ => None,
    }
}

/// Turn what was typed for a setting into a change; empty input clears it.
fn parse_setting_input(kind: SettingKind, input: &str) -> Result<SettingChange> {
    let input = input.trim();
    if input.is_empty() {
        return Ok(SettingChange::Clear);
    }
    match kind {
        SettingKind::Text => Ok(SettingChange::Set(input.into())),
        SettingKind::Duration => {
            parse_duration(input)?;
            Ok(SettingChange::Set(input.into()))
        }
        SettingKind::Number => Ok(SettingChange::Set(
            input
                .parse::<u64>()
                .with_context(|| format!("'{input}' is not a whole number"))?
                .into(),
        )),
        SettingKind::Gib => {
            let size: f64 = input
                .parse()
                .with_context(|| format!("'{input}' is not a size in GiB"))?;
            if !size.is_finite() || size <= 0.0 {
                bail!("the size must be a positive number of GiB");
            }
            Ok(SettingChange::Set(size.into()))
        }
        // Chosen by cycling, not typed.
        SettingKind::Choice(_) => Ok(SettingChange::Set(input.into())),
        SettingKind::Filter => {
            let value = match input.parse::<u64>() {
                Ok(number) => serde_yaml::Value::from(number),
                Err(_) => serde_yaml::Value::from(input),
            };
            number_filter(Some(&value), &[1, 2, 3])?;
            Ok(SettingChange::Set(value))
        }
        SettingKind::Toggle => match input {
            "true" | "yes" | "on" | "1" => Ok(SettingChange::Set(true.into())),
            "false" | "no" | "off" | "0" => Ok(SettingChange::Set(false.into())),
            _ => bail!("expected yes or no"),
        },
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum FieldKind {
    Text,
    Number,
    /// Enter cycles through the choices.
    Choice(&'static [&'static str]),
    /// Whitespace-separated words, stored as a YAML list.
    List,
    /// A duration such as `7d` or `12h`.
    Duration,
}

/// A top-level config option the Settings screen edits natively.
struct GlobalField {
    section: &'static str,
    path: &'static [&'static str],
    label: &'static str,
    kind: FieldKind,
    /// Shown when the option is unset.
    default: &'static str,
    help: &'static str,
}

const GLOBAL_FIELDS: &[GlobalField] = &[
    GlobalField {
        section: "General",
        path: &["download", "mode"],
        label: "Download mode",
        kind: FieldKind::Choice(&["jellyfin", "rsync"]),
        default: "jellyfin",
        help: "jellyfin: HTTP downloads with resume; rsync: over SSH (no filters)",
    },
    GlobalField {
        section: "General",
        path: &["parallelism"],
        label: "Parallel transfers",
        kind: FieldKind::Number,
        default: "2",
        help: "how many files download at the same time",
    },
    GlobalField {
        section: "General",
        path: &["player"],
        label: "Player",
        kind: FieldKind::Text,
        default: "mpv",
        help: "command used to play files (p)",
    },
    GlobalField {
        section: "General",
        path: &["file_manager"],
        label: "File manager",
        kind: FieldKind::Text,
        default: "xdg-open",
        help: "command used to open download folders (o)",
    },
    GlobalField {
        section: "Jellyfin",
        path: &["jellyfin", "base_url"],
        label: "Server URL",
        kind: FieldKind::Text,
        default: "",
        help: "e.g. https://jellyfin.example.com",
    },
    GlobalField {
        section: "Jellyfin",
        path: &["jellyfin", "username"],
        label: "Username",
        kind: FieldKind::Text,
        default: "",
        help: "Jellyfin user whose library and watched state are used",
    },
    GlobalField {
        section: "Jellyfin",
        path: &["jellyfin", "api_key_file"],
        label: "API key file",
        kind: FieldKind::Text,
        default: "",
        help: "path to a file holding the API key (never the key itself)",
    },
    GlobalField {
        section: "Jellyfin",
        path: &["jellyfin", "password_file"],
        label: "Password file",
        kind: FieldKind::Text,
        default: "",
        help: "path to a file holding the password, instead of an API key",
    },
    GlobalField {
        section: "Jellyfin",
        path: &["jellyfin", "user_id"],
        label: "User ID",
        kind: FieldKind::Text,
        default: "",
        help: "optional; looked up from the username when unset",
    },
    GlobalField {
        section: "Remote (rsync)",
        path: &["remote", "hostname"],
        label: "Host",
        kind: FieldKind::Text,
        default: "",
        help: "SSH host serving the media",
    },
    GlobalField {
        section: "Remote (rsync)",
        path: &["remote", "username"],
        label: "User",
        kind: FieldKind::Text,
        default: "",
        help: "SSH user",
    },
    GlobalField {
        section: "Remote (rsync)",
        path: &["remote", "port"],
        label: "Port",
        kind: FieldKind::Number,
        default: "22",
        help: "SSH port",
    },
    GlobalField {
        section: "Remote (rsync)",
        path: &["remote", "root"],
        label: "Root",
        kind: FieldKind::Text,
        default: "",
        help: "media root on the remote host",
    },
    GlobalField {
        section: "Local",
        path: &["local", "root"],
        label: "Download root",
        kind: FieldKind::Text,
        default: "",
        help: "where downloads go (~ and $HOME are expanded)",
    },
    GlobalField {
        section: "Local",
        path: &["library", "season_pattern"],
        label: "Season folders",
        kind: FieldKind::Text,
        default: "Season $season_number",
        help: "season folder name; $season_number and $name are replaced",
    },
    GlobalField {
        section: "Local",
        path: &["rsync", "flags"],
        label: "Rsync flags",
        kind: FieldKind::List,
        default: "-a -v -z",
        help: "space-separated rsync options",
    },
    GlobalField {
        section: "Cleanup",
        path: &["cleanup", "delete_watched_after"],
        label: "Keep watched for",
        kind: FieldKind::Duration,
        default: "7d",
        help: "grace period before watched files of delete-watched jobs are deleted",
    },
];

/// How a config value reads in the Settings screen.
fn field_display(value: Option<&serde_yaml::Value>) -> Option<String> {
    match value? {
        serde_yaml::Value::Null => None,
        serde_yaml::Value::String(text) => Some(text.clone()),
        serde_yaml::Value::Sequence(items) => Some(
            items
                .iter()
                .filter_map(|item| field_display(Some(item)))
                .collect::<Vec<_>>()
                .join(" "),
        ),
        other => Some(
            serde_yaml::to_string(other)
                .unwrap_or_default()
                .trim()
                .to_string(),
        ),
    }
}

/// Turn what was typed for a global field into a change; empty input clears it.
fn parse_field_input(kind: FieldKind, input: &str) -> Result<SettingChange> {
    let input = input.trim();
    if input.is_empty() {
        return Ok(SettingChange::Clear);
    }
    Ok(SettingChange::Set(match kind {
        FieldKind::Number => serde_yaml::Value::from(
            input
                .parse::<u64>()
                .with_context(|| format!("'{input}' is not a number"))?,
        ),
        FieldKind::List => serde_yaml::Value::Sequence(
            input
                .split_whitespace()
                .map(serde_yaml::Value::from)
                .collect(),
        ),
        FieldKind::Duration => {
            parse_duration(input)?;
            input.into()
        }
        FieldKind::Text | FieldKind::Choice(_) => input.into(),
    }))
}

/// A row of the Settings screen.
#[derive(Clone, Debug, PartialEq)]
enum SettingsRow {
    Header(String),
    /// Index into [GLOBAL_FIELDS].
    Field(usize),
    Job(String),
    AddJob,
    OpenEditor,
}

impl SettingsRow {
    fn selectable(&self) -> bool {
        !matches!(self, SettingsRow::Header(_))
    }
}

fn settings_rows(jobs: &[Job]) -> Vec<SettingsRow> {
    let mut rows = Vec::new();
    let mut section = "";
    for (index, field) in GLOBAL_FIELDS.iter().enumerate() {
        if field.section != section {
            section = field.section;
            rows.push(SettingsRow::Header(section.to_string()));
        }
        rows.push(SettingsRow::Field(index));
    }
    rows.push(SettingsRow::Header("Jobs".into()));
    rows.extend(jobs.iter().map(|job| SettingsRow::Job(job.name.clone())));
    rows.push(SettingsRow::AddJob);
    rows.push(SettingsRow::Header("Everything else".into()));
    rows.push(SettingsRow::OpenEditor);
    rows
}

/// A job being added from the Settings screen.
struct NewJob {
    name: String,
    /// Index into the configured library directories.
    directory: usize,
}

/// State of the Settings screen.
#[derive(Default)]
struct SettingsScreen {
    cursor: usize,
    offset: usize,
    input: Option<String>,
    new_job: Option<NewJob>,
    confirm_delete: Option<String>,
    error: Option<String>,
    /// Merged config and overlay documents, for values and override markers.
    merged: serde_yaml::Value,
    overlay: serde_yaml::Value,
}

impl SettingsScreen {
    fn open(config_path: &Path) -> Self {
        let mut screen = SettingsScreen::default();
        screen.reload(config_path);
        // Start on the first field, not a heading.
        screen.cursor = 1;
        screen
    }

    fn reload(&mut self, config_path: &Path) {
        let overlay_path = state_config_path().ok();
        self.overlay = overlay_path
            .as_deref()
            .filter(|path| path.is_file())
            .and_then(|path| read_yaml(path).ok())
            .unwrap_or_else(|| serde_yaml::Value::Mapping(Default::default()));
        self.merged = read_yaml(config_path).unwrap_or_default();
        merge_config(&mut self.merged, self.overlay.clone());
    }
}

/// Library directory names a new job can use (the `local.directories` keys).
fn library_directories(config: &Config) -> Vec<String> {
    let mut names: Vec<String> = config.local.directories.keys().cloned().collect();
    for name in config.remote.directories.keys() {
        if !names.contains(name) {
            names.push(name.clone());
        }
    }
    if names.is_empty() {
        names.push("tv_shows".into());
    }
    names
}

/// The file `E` opens: the config when writable, otherwise the overlay (created
/// with a short explanation when missing).
fn editable_config_file(config_path: &Path) -> Result<PathBuf> {
    if config_is_writable(config_path) {
        return Ok(config_path.to_path_buf());
    }
    let overlay = state_config_path()?;
    if !overlay.exists() {
        std::fs::write(
            &overlay,
            format!(
                "# jellysync settings overlay, merged on top of {}\n\
                 # (read-only). Jobs merge by name; null unsets a value.\n\
                 # Settings saved from the TUI rewrite this file without comments.\n{{}}\n",
                config_path.display()
            ),
        )?;
    }
    Ok(overlay)
}

/// Suspend the TUI, edit `path` in $VISUAL/$EDITOR (vi by default) and come back.
fn run_editor(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    path: &Path,
) -> Result<()> {
    // Empty variables count as unset.
    let editor = ["VISUAL", "EDITOR"]
        .iter()
        .filter_map(|name| env::var(name).ok())
        .find(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "vi".into());
    disable_raw_mode()?;
    execute!(stdout(), DisableMouseCapture, LeaveAlternateScreen)?;
    // Through sh so editors configured with arguments ("code -w") work.
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} \"$1\""))
        .arg("sh")
        .arg(path)
        .status();
    enable_raw_mode()?;
    execute!(stdout(), EnterAlternateScreen, EnableMouseCapture)?;
    terminal.clear()?;
    let status = status.with_context(|| format!("start editor '{editor}'"))?;
    if !status.success() {
        bail!("editor '{editor}' exited with {status}");
    }
    Ok(())
}

/// State of the job configuration dialog's settings editor.
#[derive(Default)]
struct JobEditor {
    cursor: usize,
    /// Text being typed for the selected setting.
    input: Option<String>,
    /// Settings the overlay file sets for this job.
    overrides: HashSet<String>,
    error: Option<String>,
}

/// A change to one job setting made in the TUI.
#[derive(Clone, Debug, PartialEq)]
enum SettingChange {
    Set(serde_yaml::Value),
    /// Remove the setting (in the overlay: override it with null).
    Clear,
    /// Drop the overlay's value so the main config's applies again.
    Reset,
}

/// Whether the main config accepts edits (a Nix store file does not). Opening
/// for append never truncates or changes the file.
fn config_is_writable(path: &Path) -> bool {
    std::fs::OpenOptions::new().append(true).open(path).is_ok()
}

/// Settings of `job` that the overlay file sets.
fn overlay_job_keys(overlay: &Path, job: &str) -> HashSet<String> {
    let Ok(value) = read_yaml(overlay) else {
        return HashSet::new();
    };
    value
        .get("jobs")
        .and_then(|jobs| jobs.as_sequence())
        .and_then(|jobs| {
            jobs.iter()
                .find(|entry| entry.get("name").and_then(|v| v.as_str()) == Some(job))
        })
        .and_then(|entry| entry.as_mapping())
        .map(|entry| {
            entry
                .keys()
                .filter_map(|key| key.as_str())
                .filter(|key| *key != "name")
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// The mapping for job `name` in a config document, created when `create` is set.
fn job_entry<'a>(
    document: &'a mut serde_yaml::Value,
    name: &str,
    create: bool,
) -> Option<&'a mut serde_yaml::Mapping> {
    use serde_yaml::Value;
    let root = document.as_mapping_mut()?;
    if !root.contains_key("jobs") {
        if !create {
            return None;
        }
        root.insert("jobs".into(), Value::Sequence(Vec::new()));
    }
    let jobs = root.get_mut("jobs")?.as_sequence_mut()?;
    let position = jobs
        .iter()
        .position(|entry| entry.get("name").and_then(|v| v.as_str()) == Some(name));
    let index = match position {
        Some(index) => index,
        None if create => {
            let mut entry = serde_yaml::Mapping::new();
            entry.insert("name".into(), name.into());
            jobs.push(Value::Mapping(entry));
            jobs.len() - 1
        }
        None => return None,
    };
    jobs[index].as_mapping_mut()
}

/// Write a YAML document next to its (resolved) target and rename it into place,
/// so a crash never leaves a half-written config. A symlink to a writable file
/// keeps pointing at that file.
fn write_yaml_atomic(path: &Path, value: &serde_yaml::Value) -> Result<()> {
    let target = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let dir = target
        .parent()
        .context("config path has no parent directory")?;
    std::fs::create_dir_all(dir)?;
    let temporary = dir.join(format!(
        ".{}.tmp",
        target.file_name().unwrap_or_default().to_string_lossy()
    ));
    std::fs::write(&temporary, serde_yaml::to_string(value)?)
        .with_context(|| format!("write {}", temporary.display()))?;
    if let Ok(metadata) = std::fs::metadata(&target) {
        std::fs::set_permissions(&temporary, metadata.permissions())?;
    }
    std::fs::rename(&temporary, &target)
        .with_context(|| format!("replace {}", target.display()))?;
    Ok(())
}

/// What a settings change applies to.
#[derive(Clone, Copy, Debug)]
enum SettingTarget<'a> {
    /// A top-level setting, by YAML key path (e.g. `["jellyfin", "base_url"]`).
    Global(&'a [&'a str]),
    /// A setting of the named job.
    Job(&'a str, &'a str),
}

/// The mapping holding the target's last key, created along the way when
/// `create` is set. None when (part of) the path does not exist.
fn setting_parent<'a>(
    document: &'a mut serde_yaml::Value,
    target: SettingTarget,
    create: bool,
) -> Option<&'a mut serde_yaml::Mapping> {
    match target {
        SettingTarget::Job(name, _) => job_entry(document, name, create),
        SettingTarget::Global(path) => {
            let (_, parents) = path.split_last()?;
            let mut current = document.as_mapping_mut()?;
            for key in parents {
                if !current.contains_key(*key) {
                    if !create {
                        return None;
                    }
                    current.insert(
                        (*key).into(),
                        serde_yaml::Value::Mapping(Default::default()),
                    );
                }
                let next = current.get_mut(*key)?;
                if next.is_null() && create {
                    *next = serde_yaml::Value::Mapping(Default::default());
                }
                current = next.as_mapping_mut()?;
            }
            Some(current)
        }
    }
}

fn setting_key<'a>(target: SettingTarget<'a>) -> &'a str {
    match target {
        SettingTarget::Global(path) => path.last().copied().unwrap_or_default(),
        SettingTarget::Job(_, key) => key,
    }
}

/// The value a document sets for the target, if any (null counts as set).
fn setting_value(document: &serde_yaml::Value, target: SettingTarget) -> Option<serde_yaml::Value> {
    let mut document = document.clone();
    setting_parent(&mut document, target, false)?
        .get(setting_key(target))
        .cloned()
}

/// Read the config and overlay documents, apply `edit` to one of them, check
/// that the merged result is a valid config and write what changed. Returns
/// the file that received the edit: the config when it is writable (and, for
/// job edits, defines the job), otherwise the overlay.
fn edit_config(
    config_path: &Path,
    overlay_path: &Path,
    use_base: impl FnOnce(&mut serde_yaml::Value) -> bool,
    edit: impl FnOnce(&mut serde_yaml::Value, &mut serde_yaml::Value, bool) -> Result<()>,
) -> Result<PathBuf> {
    let mut base = read_yaml(config_path)?;
    let mut overlay = if overlay_path.is_file() {
        read_yaml(overlay_path)?
    } else {
        serde_yaml::Value::Mapping(Default::default())
    };
    let original_overlay = overlay.clone();
    let write_base = config_is_writable(config_path) && use_base(&mut base);
    edit(&mut base, &mut overlay, write_base)?;
    let mut merged = base.clone();
    merge_config(&mut merged, overlay.clone());
    let config = parse_config(merged)?;
    for job in &config.jobs {
        validate_job_filters(job).with_context(|| format!("job '{}'", job.name))?;
    }
    if write_base {
        write_yaml_atomic(config_path, &base)?;
    }
    if overlay != original_overlay {
        write_yaml_atomic(overlay_path, &overlay)?;
    }
    Ok(if write_base {
        config_path.to_path_buf()
    } else {
        overlay_path.to_path_buf()
    })
}

/// Apply a settings change (see [edit_config] for where it is written).
fn save_setting(
    config_path: &Path,
    overlay_path: &Path,
    target: SettingTarget,
    change: SettingChange,
) -> Result<PathBuf> {
    let key = setting_key(target);
    edit_config(
        config_path,
        overlay_path,
        |base| match target {
            SettingTarget::Job(name, _) => job_entry(base, name, false).is_some(),
            SettingTarget::Global(_) => true,
        },
        |base, overlay, write_base| {
            if write_base {
                if let Some(parent) =
                    setting_parent(base, target, matches!(change, SettingChange::Set(_)))
                {
                    match &change {
                        SettingChange::Set(value) => {
                            parent.insert(key.into(), value.clone());
                        }
                        SettingChange::Clear | SettingChange::Reset => {
                            parent.remove(key);
                        }
                    }
                }
                // An older overlay value would otherwise hide the edit.
                if let Some(parent) = setting_parent(overlay, target, false) {
                    parent.remove(key);
                }
            } else {
                match &change {
                    SettingChange::Set(value) => {
                        setting_parent(overlay, target, true)
                            .context("overlay is not a mapping")?
                            .insert(key.into(), value.clone());
                    }
                    SettingChange::Clear => {
                        setting_parent(overlay, target, true)
                            .context("overlay is not a mapping")?
                            .insert(key.into(), serde_yaml::Value::Null);
                    }
                    SettingChange::Reset => {
                        if let Some(parent) = setting_parent(overlay, target, false) {
                            parent.remove(key);
                        }
                    }
                }
            }
            Ok(())
        },
    )
}

fn save_job_setting(
    config_path: &Path,
    overlay_path: &Path,
    job: &str,
    key: &str,
    change: SettingChange,
) -> Result<PathBuf> {
    save_setting(
        config_path,
        overlay_path,
        SettingTarget::Job(job, key),
        change,
    )
}

/// Add a job with the given library directory (a `local.directories` key).
fn add_job(
    config_path: &Path,
    overlay_path: &Path,
    name: &str,
    directory: &str,
) -> Result<PathBuf> {
    let name = name.trim();
    if name.is_empty() {
        bail!("the job needs a name");
    }
    let current = load_config_with_overlay(config_path, overlay_path)?;
    if current.jobs.iter().any(|job| job.name == name) {
        bail!("a job named '{name}' already exists");
    }
    edit_config(
        config_path,
        overlay_path,
        |_| true,
        |base, overlay, write_base| {
            let document = if write_base { base } else { overlay };
            let entry = job_entry(document, name, true).context("config is not a mapping")?;
            entry.insert("directory".into(), directory.into());
            Ok(())
        },
    )
}

/// Remove a job. A job from a read-only config cannot be removed, only disabled.
fn delete_job(config_path: &Path, overlay_path: &Path, name: &str) -> Result<PathBuf> {
    let in_base = job_entry(&mut read_yaml(config_path)?, name, false).is_some();
    if in_base && !config_is_writable(config_path) {
        bail!("'{name}' comes from the read-only config; disable it instead");
    }
    let remove = |document: &mut serde_yaml::Value| {
        if let Some(jobs) = document
            .get_mut("jobs")
            .and_then(|jobs| jobs.as_sequence_mut())
        {
            jobs.retain(|entry| entry.get("name").and_then(|v| v.as_str()) != Some(name));
        }
    };
    edit_config(
        config_path,
        overlay_path,
        |_| in_base,
        |base, overlay, write_base| {
            if write_base {
                remove(base);
            }
            remove(overlay);
            Ok(())
        },
    )
}

/// Reject seasons/episodes filters that would fail at sync time.
fn validate_job_filters(job: &Job) -> Result<()> {
    // Any existing numbers do: only the syntax matters here.
    let sample = [1, 2, 3];
    number_filter(job.seasons.as_ref(), &sample).context("invalid seasons filter")?;
    number_filter(job.episodes.as_ref(), &sample).context("invalid episodes filter")?;
    Ok(())
}
fn db() -> Result<Connection> {
    open_db(&state_db()?)
}

/// Open the state database, setting up its schema once per process.
///
/// Parallel jobs used to each run the WAL switch and `CREATE TABLE`s on their own
/// connection; on a fresh database SQLite can answer that race with "database
/// is locked" without waiting for the busy timeout. Setup is now serialized
/// in-process and retried when another process is doing the same.
fn open_db(path: &Path) -> Result<Connection> {
    static READY: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());
    let conn = Connection::open(path)?;
    conn.busy_timeout(Duration::from_secs(10))?;
    let mut ready = READY
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if !ready.iter().any(|done| done == path) {
        let mut attempt = 0;
        loop {
            match conn.execute_batch("PRAGMA journal_mode=WAL;
                CREATE TABLE IF NOT EXISTS jobs (name TEXT PRIMARY KEY, status TEXT NOT NULL, message TEXT, updated_at TEXT NOT NULL);
                CREATE TABLE IF NOT EXISTS transfers (item_id TEXT PRIMARY KEY, job TEXT NOT NULL, path TEXT NOT NULL, status TEXT NOT NULL, bytes INTEGER NOT NULL DEFAULT 0, total INTEGER, rate INTEGER, updated_at TEXT NOT NULL);
                CREATE TABLE IF NOT EXISTS watched (item_id TEXT PRIMARY KEY, played INTEGER NOT NULL, pending INTEGER NOT NULL DEFAULT 0, played_at TEXT, updated_at TEXT NOT NULL);")
            {
                Ok(()) => break,
                Err(rusqlite::Error::SqliteFailure(error, _))
                    if error.code == rusqlite::ErrorCode::DatabaseBusy && attempt < 50 =>
                {
                    attempt += 1;
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(error) => return Err(error).context("set up the state database"),
            }
        }
        // State databases from before throughput tracking lack the rate column;
        // another process may add it concurrently, so re-check on failure.
        let has_rate = |conn: &Connection| -> Result<bool> {
            Ok(conn
                .prepare("SELECT 1 FROM pragma_table_info('transfers') WHERE name='rate'")?
                .exists([])?)
        };
        if !has_rate(&conn)?
            && let Err(error) = conn.execute_batch("ALTER TABLE transfers ADD COLUMN rate INTEGER")
            && !has_rate(&conn)?
        {
            return Err(error).context("add the rate column to the state database");
        }
        // Job messages used to read "downloaded N item(s)", then "N item(s) …".
        conn.execute_batch(
            "UPDATE jobs SET message=substr(message,12)||' downloaded' WHERE message LIKE 'downloaded % item(s)';
            UPDATE jobs SET message=replace(message,' item(s)',' items') WHERE message LIKE '% item(s)%';
            UPDATE jobs SET message='1 item'||substr(message,8) WHERE message LIKE '1 items%';",
        )
        .context("update job messages in the state database")?;
        ready.push(path.to_path_buf());
    }
    Ok(conn)
}
fn update_job(name: &str, status: &str, message: &str) -> Result<()> {
    db()?.execute("INSERT INTO jobs(name,status,message,updated_at) VALUES(?1,?2,?3,datetime('now')) ON CONFLICT(name) DO UPDATE SET status=excluded.status,message=excluded.message,updated_at=excluded.updated_at", params![name, status, message])?;
    Ok(())
}
fn forget_job(name: &str) -> Result<()> {
    db()?.execute("DELETE FROM jobs WHERE name=?1", params![name])?;
    Ok(())
}
/// Played state of a tracked item: Jellyfin's, or a local mark not yet pushed.
#[derive(Clone, Debug, PartialEq)]
struct WatchedState {
    played: bool,
    /// Marked in jellysync (possibly offline) and not yet sent to Jellyfin.
    pending: bool,
    /// When it was (first seen) played, as an SQLite UTC datetime.
    played_at: Option<String>,
}

fn watched_states(conn: &Connection) -> Result<HashMap<String, WatchedState>> {
    let mut statement = conn.prepare("SELECT item_id,played,pending,played_at FROM watched")?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            WatchedState {
                played: row.get::<_, i64>(1)? != 0,
                pending: row.get::<_, i64>(2)? != 0,
                played_at: row.get(3)?,
            },
        ))
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Jellyfin's `LastPlayedDate` (`2026-09-25T10:55:43.71Z`) as an SQLite datetime.
fn jellyfin_datetime(value: &str) -> Option<String> {
    let value = value.replace('T', " ");
    (value.len() >= 19).then(|| value[..19].to_string())
}

/// Record played state reported by Jellyfin. Items with a pending local mark
/// keep it: that change is newer and will be pushed.
fn record_server_watched(
    conn: &Connection,
    items: &[(String, bool, Option<String>)],
) -> Result<()> {
    let mut statement = conn.prepare(
        "INSERT INTO watched(item_id,played,pending,played_at,updated_at)
         VALUES(?1,?2,0,CASE WHEN ?2 THEN COALESCE(?3,datetime('now')) END,datetime('now'))
         ON CONFLICT(item_id) DO UPDATE SET
           played=excluded.played,
           played_at=CASE WHEN excluded.played THEN COALESCE(watched.played_at,excluded.played_at) END,
           updated_at=excluded.updated_at
         WHERE watched.pending=0",
    )?;
    for (item_id, played, played_at) in items {
        statement.execute(params![item_id, played, played_at])?;
    }
    Ok(())
}

/// Mark items watched or unwatched locally; pushed to Jellyfin when online.
fn mark_watched(conn: &Connection, item_ids: &[String], played: bool) -> Result<()> {
    let mut statement = conn.prepare(
        "INSERT INTO watched(item_id,played,pending,played_at,updated_at)
         VALUES(?1,?2,1,CASE WHEN ?2 THEN datetime('now') END,datetime('now'))
         ON CONFLICT(item_id) DO UPDATE SET
           played=excluded.played,
           pending=1,
           played_at=CASE WHEN excluded.played THEN COALESCE(watched.played_at,excluded.played_at) END,
           updated_at=excluded.updated_at",
    )?;
    for item_id in item_ids {
        statement.execute(params![item_id, played])?;
    }
    Ok(())
}

/// Send pending local marks to Jellyfin; returns how many were pushed.
async fn push_pending_watched(api: &JellyfinApi) -> Result<usize> {
    let pending: Vec<(String, bool)> = {
        let conn = db()?;
        let mut statement = conn.prepare("SELECT item_id,played FROM watched WHERE pending=1")?;
        statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? != 0))
            })?
            .collect::<rusqlite::Result<_>>()?
    };
    let mut pushed = 0;
    for (item_id, played) in pending {
        let method = if played {
            reqwest::Method::POST
        } else {
            reqwest::Method::DELETE
        };
        let send = |url: String| {
            api.client
                .request(method.clone(), url)
                .header("Authorization", jellyfin_authorization(&api.token))
                .timeout(Duration::from_secs(10))
                .send()
        };
        // Jellyfin 10.9+ API first, the older per-user route as a fallback.
        let mut response = send(format!(
            "{}/UserPlayedItems/{item_id}?userId={}",
            api.base, api.user_id
        ))
        .await
        .context("update Jellyfin played state")?;
        if response.status() == StatusCode::NOT_FOUND {
            response = send(format!(
                "{}/Users/{}/PlayedItems/{item_id}",
                api.base, api.user_id
            ))
            .await
            .context("update Jellyfin played state")?;
        }
        response
            .error_for_status()
            .with_context(|| format!("Jellyfin rejected the played state of {item_id}"))?;
        // Only clear the flag if nobody changed the mark while we pushed it.
        db()?.execute(
            "UPDATE watched SET pending=0 WHERE item_id=?1 AND played=?2",
            params![item_id, played],
        )?;
        pushed += 1;
    }
    Ok(pushed)
}

/// Fetch Jellyfin's played state for the given items into the watched table.
async fn refresh_watched(api: &JellyfinApi, item_ids: &[String]) -> Result<()> {
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct PlayedData {
        #[serde(default)]
        played: bool,
        last_played_date: Option<String>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct PlayedItem {
        id: String,
        user_data: Option<PlayedData>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct PlayedItems {
        items: Vec<PlayedItem>,
    }
    for chunk in item_ids.chunks(100) {
        let response: PlayedItems = api
            .client
            .get(format!("{}/Users/{}/Items", api.base, api.user_id))
            .query(&[("Ids", chunk.join(",")), ("Fields", "UserData".into())])
            .header("Authorization", jellyfin_authorization(&api.token))
            .timeout(Duration::from_secs(20))
            .send()
            .await
            .context("query Jellyfin played state")?
            .error_for_status()
            .context("Jellyfin played state query failed")?
            .json()
            .await
            .context("parse Jellyfin played state")?;
        let items: Vec<_> = response
            .items
            .into_iter()
            .map(|item| {
                let data = item.user_data;
                let played = data.as_ref().is_some_and(|data| data.played);
                let played_at = data
                    .and_then(|data| data.last_played_date)
                    .and_then(|date| jellyfin_datetime(&date));
                (item.id, played, played_at)
            })
            .collect();
        record_server_watched(&db()?, &items)?;
    }
    Ok(())
}

/// Push pending marks and refresh the played state of every tracked item.
async fn sync_watched(config: &Config) -> Result<usize> {
    let credentials = config
        .jellyfin
        .as_ref()
        .context("watched state requires Jellyfin credentials")?;
    let api = jellyfin_login(credentials).await?;
    let pushed = push_pending_watched(&api).await?;
    let item_ids: Vec<String> = tracked_downloads()?
        .into_iter()
        .map(|entry| entry.item_id)
        .collect();
    refresh_watched(&api, &item_ids).await?;
    Ok(pushed)
}

/// Ignore a tracked item (syncs leave it alone; a downloaded file is kept) or
/// allow it again (the next sync downloads it if its file is missing).
fn set_ignored(entry: &DownloadEntry, ignored: bool) -> Result<()> {
    let status = if ignored {
        "ignored"
    } else if entry.path.exists() {
        "complete"
    } else {
        "queued"
    };
    // A requeued file starts over; its old byte count would read as 100%.
    db()?.execute(
        "UPDATE transfers SET status=?2,bytes=CASE WHEN ?2='queued' THEN 0 ELSE bytes END,updated_at=datetime('now') WHERE item_id=?1",
        params![entry.item_id, status],
    )?;
    Ok(())
}

/// Items the user removed in the TUI; syncs must not download them again.
fn ignored_items() -> Result<HashSet<String>> {
    let conn = db()?;
    let mut statement = conn.prepare("SELECT item_id FROM transfers WHERE status='ignored'")?;
    Ok(statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<_>>()?)
}

fn update_transfer(
    item_id: &str,
    job: &str,
    path: &Path,
    status: &str,
    bytes: u64,
    total: Option<u64>,
) -> Result<()> {
    db()?.execute("INSERT INTO transfers(item_id,job,path,status,bytes,total,updated_at) VALUES(?1,?2,?3,?4,?5,?6,datetime('now'))
        ON CONFLICT(item_id) DO UPDATE SET job=excluded.job,path=excluded.path,status=excluded.status,bytes=excluded.bytes,total=excluded.total,rate=NULL,updated_at=excluded.updated_at",
        params![item_id, job, path.to_string_lossy(), status, bytes as i64, total.map(|v| v as i64)])?;
    Ok(())
}
fn transfer_total(item_id: &str) -> Result<Option<u64>> {
    Ok(db()?
        .query_row(
            "SELECT total FROM transfers WHERE item_id=?1",
            params![item_id],
            |row| row.get::<_, Option<u64>>(0),
        )
        .optional()?
        .flatten())
}
/// Replace `$key` with the matching directory, longest keys first so `$tv_shows`
/// is not mistaken for `$tv` followed by "_shows".
fn substitute_directories<'a>(
    value: &str,
    directories: impl IntoIterator<Item = (&'a String, &'a String)>,
) -> String {
    let mut directories: Vec<_> = directories.into_iter().collect();
    directories.sort_by_key(|(key, _)| std::cmp::Reverse(key.len()));
    let mut value = value.to_string();
    for (key, path) in directories {
        value = value.replace(&format!("${key}"), path);
    }
    value
}

fn resolved_path(config: &Config, job: &Job, remote: bool) -> Result<String> {
    let (root, directories) = if remote {
        (&config.remote.root, &config.remote.directories)
    } else {
        (&config.local.root, &config.local.directories)
    };
    let specified = if remote {
        job.remote_dir.as_deref()
    } else {
        job.local_dir.as_deref()
    };
    let raw = if let Some(path) = specified {
        path.to_string()
    } else if let Some(directory) = &job.directory {
        let base = directories
            .get(directory)
            .cloned()
            .unwrap_or_else(|| directory.clone());
        format!("{base}/{}", job.name)
    } else {
        bail!("job '{}' needs remote_dir/local_dir or directory", job.name)
    };
    let mut value = substitute_directories(&raw.replace("$name", &job.name), directories);
    // `~` and `$HOME` refer to this machine, so only local paths expand them.
    if !remote {
        value = expand_home(&value);
    }
    if Path::new(&value).is_absolute() {
        Ok(value)
    } else {
        Ok(format!("{root}/{value}"))
    }
}
#[derive(Clone)]
struct JellyfinApi {
    client: HttpClient,
    base: String,
    token: String,
    user_id: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct AuthResult {
    access_token: String,
    user: JellyfinUser,
}
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct JellyfinUser {
    id: String,
    #[serde(default)]
    name: Option<String>,
}
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ItemsResponse {
    items: Vec<MediaItem>,
}
#[derive(Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct MediaItem {
    id: String,
    name: String,
    #[serde(rename = "Type")]
    #[serde(default)]
    item_type: Option<String>,
    #[serde(default)]
    production_year: Option<u32>,
    path: Option<String>,
    parent_index_number: Option<u32>,
    index_number: Option<u32>,
    user_data: Option<UserData>,
    #[serde(default)]
    series_name: Option<String>,
    #[serde(default)]
    media_sources: Option<Vec<MediaSource>>,
}
#[derive(Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct MediaSource {
    #[serde(default)]
    size: Option<u64>,
}

type PosterFetch = Result<Option<(Vec<u8>, StatefulProtocol)>>;
type PosterTask = tokio::task::JoinHandle<(String, PosterFetch)>;
type CatalogTask = tokio::task::JoinHandle<Result<(JellyfinApi, Vec<MediaItem>)>>;
type MediaSummary = Vec<(String, String)>;
/// File name plus the ffprobe summary once it has finished.
type FileInfo = (String, Option<std::result::Result<MediaSummary, String>>);
type FileInfoTask = tokio::task::JoinHandle<Result<MediaSummary>>;
/// Jellyfin rows, overview and (optional) still image for the file info popup.
type FileMeta = (MediaSummary, String, Option<StatefulProtocol>);
type FileMetaTask = tokio::task::JoinHandle<Result<FileMeta>>;
/// ffprobe result for a local file: detail rows plus short badges for list rows.
#[derive(Clone)]
struct MediaProbe {
    rows: MediaSummary,
    badges: Vec<String>,
}
type ProbeTask = tokio::task::JoinHandle<Result<MediaProbe>>;
/// Files probed in the background at once for the Files list badges.
const PROBE_CONCURRENCY: usize = 4;
/// How long a successful footer notice stays; failures stay twice as long.
const NOTICE_TTL: Duration = Duration::from_secs(8);
const QUIT_HINT: &str = "Press Ctrl-C again to quit";
struct EpisodeInfo {
    season: Option<u32>,
    episode: Option<u32>,
    name: String,
}
/// Jellyfin metadata shown above the Files list for the selected job.
struct JobDetails {
    series: bool,
    title: String,
    years: Option<String>,
    facts: Vec<String>,
    tagline: Option<String>,
    overview: String,
    episodes: HashMap<String, EpisodeInfo>,
    poster: Option<StatefulProtocol>,
}
type JobDetailsTask = tokio::task::JoinHandle<Result<JobDetails>>;
/// A list index together with the identity (job name or item id) it pointed at.
type Anchor = (usize, String);
/// A row in the Files list: a season heading or an index into the file list.
enum FileRow {
    Season(String),
    File(usize),
}
type PreviewEpisodeTask = tokio::task::JoinHandle<(String, Result<Vec<MediaItem>>)>;
type JobPosterCatalogTask =
    tokio::task::JoinHandle<Result<(JellyfinApi, Vec<(String, MediaItem)>)>>;
// Posters are 4 rows tall; at the usual 1:2 cell aspect a 2:3 poster is ~5.3
// cells wide. The indent leaves one blank column between poster and text.
const POSTER_WIDTH: u16 = 6;
const POSTER_INDENT: &str = "       ";

struct ExploreState {
    api: Option<JellyfinApi>,
    items: Vec<MediaItem>,
    episodes: Vec<MediaItem>,
    selected: usize,
    episode_cursor: usize,
    selected_episodes: HashSet<usize>,
    search: String,
    type_filter: ExploreTypeFilter,
    details_focus: bool,
    poster_id: Option<String>,
    poster_loading: bool,
    poster_loaded: bool,
    preview_item_id: Option<String>,
    preview_loading: bool,
    catalog_loading: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ExploreTypeFilter {
    All,
    Shows,
    Movies,
}

impl ExploreTypeFilter {
    fn next(self) -> Self {
        match self {
            Self::All => Self::Shows,
            Self::Shows => Self::Movies,
            Self::Movies => Self::All,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Shows => "TV shows",
            Self::Movies => "Movies",
        }
    }

    fn matches(self, item_type: Option<&str>) -> bool {
        match self {
            Self::All => true,
            Self::Shows => item_type == Some("Series"),
            Self::Movies => item_type == Some("Movie"),
        }
    }
}

impl ExploreState {
    fn filtered_items(&self) -> Vec<&MediaItem> {
        self.items
            .iter()
            .filter(|item| {
                self.type_filter.matches(item.item_type.as_deref())
                    && (self.search.is_empty()
                        || item
                            .name
                            .to_lowercase()
                            .contains(&self.search.to_lowercase()))
            })
            .collect()
    }
}
#[derive(Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct UserData {
    /// Jellyfin calls it `Played`; PascalCase alone would look for `IsPlayed`,
    /// which never matched and left the unwatched filter with nothing.
    #[serde(rename = "Played")]
    is_played: Option<bool>,
    #[serde(default)]
    last_played_date: Option<String>,
}

/// Cheap unauthenticated reachability check used by the TUI's offline mode.
async fn jellyfin_reachable(config: &Jellyfin) -> bool {
    let Ok(client) = HttpClient::builder()
        .user_agent(concat!("jellysync/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(3))
        .build()
    else {
        return false;
    };
    client
        .get(format!(
            "{}/System/Ping",
            config.base_url.trim_end_matches('/')
        ))
        .send()
        .await
        .is_ok_and(|response| response.status().is_success())
}

/// Log in to Jellyfin, reusing this process's recent session.
///
/// Password logins all use the same device id, and Jellyfin revokes a device's
/// previous token when it logs in again, so jobs syncing in parallel used to
/// invalidate each other's sessions (401 mid-download). Logins are serialized
/// and shared per server and account; sessions are renewed after a while so a
/// long-running TUI recovers when another process (e.g. the timer) took over.
async fn jellyfin_login(config: &Jellyfin) -> Result<JellyfinApi> {
    const SESSION_REUSE: Duration = Duration::from_secs(300);
    static SESSIONS: std::sync::LazyLock<
        tokio::sync::Mutex<HashMap<String, (Instant, JellyfinApi)>>,
    > = std::sync::LazyLock::new(Default::default);
    let key = format!(
        "{}\n{:?}\n{:?}\n{:?}\n{:?}",
        config.base_url, config.username, config.user_id, config.api_key_file, config.password_file
    );
    let mut sessions = SESSIONS.lock().await;
    if let Some((at, api)) = sessions.get(&key)
        && at.elapsed() < SESSION_REUSE
    {
        return Ok(api.clone());
    }
    let api = jellyfin_login_uncached(config).await?;
    sessions.insert(key, (Instant::now(), api.clone()));
    Ok(api)
}

async fn jellyfin_login_uncached(config: &Jellyfin) -> Result<JellyfinApi> {
    let base = config.base_url.trim_end_matches('/').to_string();
    let client = HttpClient::builder()
        .user_agent(concat!("jellysync/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(5))
        .read_timeout(Duration::from_secs(60))
        .build()?;
    let (token, user_id) = if let Some(api_key_file) = &config.api_key_file {
        let token = std::fs::read_to_string(api_key_file)
            .with_context(|| format!("read Jellyfin API key file {api_key_file}"))?;
        let token = token.trim();
        if token.is_empty() {
            bail!("Jellyfin API key file is empty");
        }
        let user_id =
            if let Some(user_id) = config.user_id.as_deref().filter(|id| !id.trim().is_empty()) {
                user_id.to_string()
            } else {
                let username = config.username.as_deref().context(
                "Jellyfin API key authentication requires jellyfin.username or jellyfin.user_id",
            )?;
                let users = client
                    .get(format!("{base}/Users"))
                    .header("Authorization", jellyfin_authorization(token))
                    .send()
                    .await
                    .context("query Jellyfin users")?
                    .error_for_status()
                    .context("Jellyfin API key rejected while querying users")?
                    .json::<Vec<JellyfinUser>>()
                    .await
                    .context("parse Jellyfin user response")?;
                let mut matching = users.into_iter().filter(|user| {
                    user.name
                        .as_deref()
                        .is_some_and(|name| name.eq_ignore_ascii_case(username))
                });
                let user = matching
                    .next()
                    .context("Jellyfin API key could not resolve configured username")?;
                if matching.next().is_some() {
                    bail!("configured Jellyfin username is ambiguous; set jellyfin.user_id");
                }
                user.id
            };
        (token.to_string(), user_id)
    } else {
        let password_file = config
            .password_file
            .as_deref()
            .context("configure jellyfin.api_key_file and jellyfin.user_id")?;
        let username = config
            .username
            .as_deref()
            .context("password authentication requires jellyfin.username")?;
        let password = std::fs::read_to_string(password_file)
            .with_context(|| format!("read Jellyfin password file {password_file}"))?;
        let authorization = concat!(
            r#"MediaBrowser Client="jellysync", Device="jellysync", DeviceId="jellysync", Version=""#,
            env!("CARGO_PKG_VERSION"),
            '"'
        );
        let response = client
            .post(format!("{base}/Users/AuthenticateByName"))
            .header("Authorization", authorization)
            .json(&json!({ "Username": username, "Pw": password.trim_end() }))
            .send()
            .await
            .context("authenticate to Jellyfin")?
            .error_for_status()
            .context("Jellyfin rejected authentication")?
            .json::<AuthResult>()
            .await
            .context("parse Jellyfin authentication response")?;
        (response.access_token, response.user.id)
    };
    Ok(JellyfinApi {
        client,
        base,
        token,
        user_id,
    })
}

fn jellyfin_authorization(token: &str) -> String {
    format!(
        r#"MediaBrowser Client="jellysync", Device="jellysync", DeviceId="jellysync", Version="{}", Token="{token}""#,
        env!("CARGO_PKG_VERSION")
    )
}

async fn jellyfin_items(api: &JellyfinApi, query: &[(&str, &str)]) -> Result<Vec<MediaItem>> {
    Ok(api
        .client
        .get(format!("{}/Users/{}/Items", api.base, api.user_id))
        .header("Authorization", jellyfin_authorization(&api.token))
        .query(query)
        .send()
        .await
        .context("query Jellyfin library")?
        .error_for_status()
        .context("Jellyfin library query failed")?
        .json::<ItemsResponse>()
        .await
        .context("parse Jellyfin item response")?
        .items)
}

async fn jellyfin_catalog(config: &Config) -> Result<(JellyfinApi, Vec<MediaItem>)> {
    let credentials = config
        .jellyfin
        .as_ref()
        .context("Explore requires Jellyfin credentials")?;
    let api = jellyfin_login(credentials).await?;
    let mut items = jellyfin_items(
        &api,
        &[
            ("Recursive", "true"),
            ("IncludeItemTypes", "Series,Movie"),
            ("Fields", "Path,ProductionYear"),
            ("SortBy", "SortName"),
            ("Limit", "1000"),
        ],
    )
    .await?;
    items.sort_by_key(|item| item.name.to_lowercase());
    Ok((api, items))
}

async fn jellyfin_job_poster_catalog(
    config: &Config,
) -> Result<(JellyfinApi, Vec<(String, MediaItem)>)> {
    let credentials = config
        .jellyfin
        .as_ref()
        .context("job posters require Jellyfin credentials")?;
    let api = jellyfin_login(credentials).await?;
    let mut poster_jobs: Vec<(String, String)> = config
        .jobs
        .iter()
        .filter(|job| !job.is_auto())
        .map(|job| {
            (
                job.name.clone(),
                job.jellyfin_name
                    .as_deref()
                    .unwrap_or(&job.name)
                    .to_string(),
            )
        })
        .collect();
    let ad_hoc_jobs: Vec<String> = {
        let conn = db()?;
        let mut statement = conn.prepare(
            "SELECT name FROM jobs WHERE name LIKE 'library:%'
             UNION SELECT job FROM transfers WHERE job LIKE 'library:%'",
        )?;
        statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    for job_name in ad_hoc_jobs {
        if !poster_jobs.iter().any(|(name, _)| name == &job_name) {
            poster_jobs.push((
                job_name.clone(),
                job_name.trim_start_matches("library:").to_string(),
            ));
        }
    }

    let mut posters = Vec::new();
    for (job_name, name) in poster_jobs {
        let Ok(items) = jellyfin_items(
            &api,
            &[
                ("Recursive", "true"),
                ("IncludeItemTypes", "Series,Movie"),
                ("SearchTerm", &name),
                ("Fields", "ProductionYear"),
                ("Limit", "100"),
            ],
        )
        .await
        else {
            continue;
        };
        if let Ok(Some(item)) = find_one(items, &name) {
            posters.push((job_name, item));
        }
    }
    Ok((api, posters))
}

async fn fetch_poster_protocol(
    api: JellyfinApi,
    item: MediaItem,
    picker: Picker,
    cached_data: Option<Vec<u8>>,
) -> Result<Option<(Vec<u8>, StatefulProtocol)>> {
    let data = if let Some(data) = cached_data {
        Some(data)
    } else {
        let mut data: Option<Vec<u8>> = None;
        let mut last_error = None;
        for attempt in 0..3 {
            match jellyfin_poster(&api, &item).await {
                Ok(result) => {
                    data = result;
                    break;
                }
                Err(error) => {
                    last_error = Some(error);
                    if attempt < 2 {
                        tokio::time::sleep(Duration::from_millis(250 * (attempt + 1))).await;
                    }
                }
            }
        }
        if data.is_none() {
            return Err(last_error.context("poster request failed")?);
        }
        data
    };
    let Some(data) = data else {
        return Ok(None);
    };
    let cached_data = data.clone();
    let protocol = tokio::task::spawn_blocking(move || poster_protocol(&picker, &data))
        .await
        .context("poster decoder worker stopped")??;
    Ok(Some((cached_data, protocol)))
}

fn request_poster(
    browser: &mut ExploreState,
    item: MediaItem,
    picker: &Picker,
    cache: &HashMap<String, Vec<u8>>,
    image: &mut ThreadProtocol,
    image_task: &mut Option<PosterTask>,
) -> Result<()> {
    if browser.poster_id.as_deref() == Some(item.id.as_str())
        && (browser.poster_loading || browser.poster_loaded)
    {
        return Ok(());
    }
    let api = browser
        .api
        .clone()
        .context("Explore Jellyfin session is not ready")?;
    browser.poster_id = Some(item.id.clone());
    browser.poster_loading = true;
    browser.poster_loaded = false;
    image.empty_protocol();
    if let Some(task) = image_task.take() {
        task.abort();
    }
    let picker = picker.clone();
    let item_id = item.id.clone();
    let cached_data = cache.get(&item.id).cloned();
    *image_task = Some(tokio::spawn(async move {
        let result = fetch_poster_protocol(api, item, picker, cached_data).await;
        (item_id, result)
    }));
    Ok(())
}

async fn jellyfin_episodes(api: &JellyfinApi, series: &MediaItem) -> Result<Vec<MediaItem>> {
    jellyfin_items(
        api,
        &[
            ("ParentId", series.id.as_str()),
            ("Recursive", "true"),
            ("IncludeItemTypes", "Episode"),
            ("Fields", "Path,ParentIndexNumber,IndexNumber"),
            ("SortBy", "ParentIndexNumber,IndexNumber"),
            ("Limit", "10000"),
        ],
    )
    .await
}

async fn fetch_episodes(api: JellyfinApi, series: MediaItem) -> Result<Vec<MediaItem>> {
    jellyfin_episodes(&api, &series).await
}

async fn jellyfin_poster(api: &JellyfinApi, item: &MediaItem) -> Result<Option<Vec<u8>>> {
    let response = api
        .client
        .get(format!(
            "{}/Items/{}/Images/Primary?maxWidth=480",
            api.base, item.id
        ))
        .header("Authorization", jellyfin_authorization(&api.token))
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .context("request Jellyfin poster")?;
    if response.status() == StatusCode::NOT_FOUND || response.status() == StatusCode::NO_CONTENT {
        return Ok(None);
    }
    Ok(Some(
        response
            .error_for_status()
            .context("Jellyfin poster request failed")?
            .bytes()
            .await
            .context("read Jellyfin poster")?
            .to_vec(),
    ))
}

fn poster_protocol(picker: &Picker, data: &[u8]) -> Result<StatefulProtocol> {
    let image = image::load_from_memory(data).context("decode Jellyfin poster")?;
    Ok(picker.new_resize_protocol(image))
}

/// Record ad-hoc library items as queued; a detached `jellysync fetch` worker downloads them.
fn queue_library_items(
    config: &Config,
    items: &[MediaItem],
    title: &str,
    movie: bool,
) -> Result<(usize, String, String)> {
    if items.is_empty() {
        bail!("select at least one item to download");
    }
    let job = format!("library:{title}");
    let mut folder = PathBuf::from(&config.local.root);
    folder.push(if movie { "Movies" } else { "TV Shows" });
    folder.push(safe_component(title));
    std::fs::create_dir_all(&folder)?;
    for item in items {
        let destination = item_destination(item, &folder, "Season $season_number", title, movie)?;
        mark_queued(&job, item, &destination)?;
    }
    update_job(
        &job,
        "queued",
        &format!("{} queued", plural(items.len(), "item")),
    )?;
    Ok((items.len(), job, items[0].id.clone()))
}

fn pending_transfers(job: &str) -> Result<Vec<(String, PathBuf)>> {
    let conn = db()?;
    let mut statement = conn.prepare(
        "SELECT item_id,path FROM transfers WHERE job=?1 AND status IN ('queued','downloading','interrupted') ORDER BY path",
    )?;
    let rows = statement.query_map(params![job], |row| {
        Ok((
            row.get::<_, String>(0)?,
            PathBuf::from(row.get::<_, String>(1)?),
        ))
    })?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

fn pending_library_jobs() -> Result<Vec<String>> {
    let conn = db()?;
    let mut statement = conn.prepare(
        "SELECT DISTINCT job FROM transfers WHERE job LIKE 'library:%' AND status IN ('queued','downloading','interrupted')",
    )?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

/// Download the pending transfers of an ad-hoc job. Only one worker runs per job (file
/// lock); a second one exits immediately. Returns once nothing new is pending.
async fn library_worker(config: &Config, job: &str) -> Result<()> {
    let lock_dir = state_db()?
        .parent()
        .context("state directory has no parent")?
        .join("locks");
    std::fs::create_dir_all(&lock_dir)?;
    let lock_path = lock_dir.join(format!("{}.lock", safe_component(job)));
    let credentials = config
        .jellyfin
        .as_ref()
        .context("ad-hoc downloads require Jellyfin credentials")?;
    let mut attempted = HashSet::new();
    loop {
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => return Ok(()),
            Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
        }
        // Items that already failed in this run stay pending; don't retry them in a loop.
        // (Paused items are skipped by status, so resuming one lets this loop pick it up.)
        let pending: Vec<_> = pending_transfers(job)?
            .into_iter()
            .filter(|(item_id, _)| !attempted.contains(item_id))
            .collect();
        if pending.is_empty() {
            return Ok(());
        }
        let api = jellyfin_login(credentials).await?;
        update_job(job, "running", &plural(pending.len(), "item"))?;
        let count = pending.len();
        let slots = Arc::new(Semaphore::new(config.parallelism.max(1)));
        let mut tasks = JoinSet::new();
        for (item_id, destination) in pending {
            let api = api.clone();
            let job = job.to_string();
            let permit = slots.clone().acquire_owned().await?;
            tasks.spawn(async move {
                let _permit = permit;
                let item = MediaItem {
                    id: item_id,
                    name: destination
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned(),
                    item_type: None,
                    production_year: None,
                    path: None,
                    parent_index_number: None,
                    index_number: None,
                    user_data: None,
                    series_name: None,
                    media_sources: None,
                };
                let id = item.id.clone();
                (
                    id,
                    jellyfin_download(&api, &job, &item, &destination, true).await,
                )
            });
        }
        let mut failures = Vec::new();
        while let Some(result) = tasks.join_next().await {
            let (item_id, result) = result.context("ad-hoc transfer worker failed")?;
            if let Err(error) = result {
                attempted.insert(item_id);
                failures.push(format!("{error:#}"));
            }
        }
        if failures.is_empty() {
            update_job(
                job,
                "success",
                &format!("{} downloaded", plural(count, "item")),
            )?;
        } else {
            update_job(job, "failed", &failures.join("; "))?;
        }
        // Release the lock before re-checking so a worker spawned for newly queued items
        // meanwhile either finds them itself or leaves them to this loop.
        drop(lock);
    }
}

/// Finish ad-hoc downloads inline during a full sync (e.g. from the systemd timer), in
/// case their detached worker died. Jobs with a live worker are skipped via its lock.
async fn finish_library_jobs(config: &Config) {
    let jobs = match pending_library_jobs() {
        Ok(jobs) => jobs,
        Err(error) => {
            eprintln!("Could not list pending ad-hoc downloads: {error:#}");
            return;
        }
    };
    for job in jobs {
        if let Err(error) = library_worker(config, &job).await {
            eprintln!("Ad-hoc download {job} failed: {error:#}");
        }
    }
}

/// The attached tmux client that most recently used our window, i.e. the one that
/// launched jellysync. None outside tmux or when only one client views the window.
fn tmux_active_client() -> Option<String> {
    let pane = env::var("TMUX_PANE").ok()?;
    env::var_os("TMUX")?;
    let tmux = |args: &[&str]| {
        std::process::Command::new("tmux")
            .args(args)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
    };
    let window = tmux(&["display-message", "-p", "-t", &pane, "#{window_id}"])?;
    let window = window.trim();
    let clients = tmux(&[
        "list-clients",
        "-F",
        "#{client_activity}\t#{window_id}\t#{client_name}",
    ])?;
    let viewers: Vec<(u64, String)> = clients
        .lines()
        .filter_map(|line| {
            let mut fields = line.splitn(3, '\t');
            let activity = fields.next()?.parse().ok()?;
            let window_id = fields.next()?;
            let name = fields.next()?;
            (window_id == window).then(|| (activity, name.to_string()))
        })
        .collect();
    // With a single viewer there is no other client to steal the window size.
    if viewers.len() < 2 {
        return None;
    }
    viewers.into_iter().max().map(|(_, name)| name)
}

/// How often to re-check the viewing tmux client's cell size.
const CELL_CHECK_INTERVAL: Duration = Duration::from_secs(5);

/// Inside tmux the cell-size query is answered by tmux with the size of some
/// client, which is wrong when clients with different fonts or DPI view the
/// session (e.g. a HiDPI laptop plus an SSH client): kitty then shows images
/// too large and crops them. Use the cell size of the client actually looking at
/// this pane instead.
fn with_viewer_cell_size(picker: Picker) -> Picker {
    let Some(cell) = tmux_viewer_cell_size() else {
        return picker;
    };
    if cell == picker.font_size() {
        return picker;
    }
    let protocol = picker.protocol_type();
    let mut updated = Picker::from_fontsize(cell);
    updated.set_protocol_type(protocol);
    updated
}

fn tmux_viewer_cell_size() -> Option<(u16, u16)> {
    let pane = env::var("TMUX_PANE").ok()?;
    env::var_os("TMUX")?;
    let tmux = |args: &[&str]| {
        std::process::Command::new("tmux")
            .args(args)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
    };
    let window = tmux(&["display-message", "-p", "-t", &pane, "#{window_id}"])?;
    let clients = tmux(&[
        "list-clients",
        "-F",
        "#{client_activity}\t#{window_id}\t#{client_cell_width}\t#{client_cell_height}",
    ])?;
    viewer_cell_size(&clients, window.trim())
}

/// Cell size of the most recently active client viewing `window`, from
/// `list-clients` output (activity, window id, cell width, cell height).
fn viewer_cell_size(clients: &str, window: &str) -> Option<(u16, u16)> {
    clients
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split('\t').collect();
            let [activity, window_id, width, height] = fields[..] else {
                return None;
            };
            let (width, height): (u16, u16) = (width.parse().ok()?, height.parse().ok()?);
            (window_id == window && width > 0 && height > 0)
                .then_some((activity.parse::<u64>().ok()?, (width, height)))
        })
        .max_by_key(|(activity, _)| *activity)
        .map(|(_, cell)| cell)
}

/// The image-capability query is passed through to every tmux client viewing this
/// window. Their terminals' replies count as input, so with `window-size latest` another
/// (smaller) client becomes the latest and the window shrinks until the user presses a
/// key. Make the launching client the latest again, now and once more for late replies.
fn reclaim_tmux_window(client: String) {
    let reclaim = move || {
        let Ok(pane) = env::var("TMUX_PANE") else {
            return;
        };
        let _ = std::process::Command::new("tmux")
            .args(["switch-client", "-c", &client, "-t", &pane])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    };
    reclaim();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(1500));
        reclaim();
    });
}

/// Start a detached worker that keeps downloading after the TUI exits.
fn spawn_library_worker(config_path: &Path, job: &str) -> Result<()> {
    use std::os::unix::process::CommandExt as _;
    let mut child = std::process::Command::new(env::current_exe()?)
        .arg("--config")
        .arg(config_path)
        .arg("worker")
        .arg(job)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .context("start ad-hoc download worker")?;
    // Reap the worker if it finishes while we are still running.
    std::thread::spawn(move || child.wait());
    Ok(())
}

/// Resume ad-hoc downloads left behind by a worker that died or never started.
fn resume_library_workers(config_path: &Path) -> Result<usize> {
    let jobs = pending_library_jobs()?;
    for job in &jobs {
        spawn_library_worker(config_path, job)?;
    }
    Ok(jobs.len())
}

fn safe_component(value: &str) -> String {
    let safe: String = value
        .chars()
        .map(|character| {
            if character.is_control() || matches!(character, '/' | '\\') {
                '_'
            } else {
                character
            }
        })
        .collect();
    let safe = safe.trim().trim_matches('.').to_string();
    if safe.is_empty() {
        "Untitled".into()
    } else {
        safe
    }
}

fn find_one(items: Vec<MediaItem>, name: &str) -> Result<Option<MediaItem>> {
    let exact: Vec<_> = items
        .iter()
        .filter(|i| i.name.eq_ignore_ascii_case(name))
        .cloned()
        .collect();
    if exact.len() == 1 {
        return Ok(exact.into_iter().next());
    }
    if exact.len() > 1 {
        bail!("multiple Jellyfin items exactly match '{name}'");
    }
    let matches: Vec<_> = items
        .into_iter()
        .filter(|i| i.name.to_lowercase().contains(&name.to_lowercase()))
        .collect();
    match matches.len() {
        0 => Ok(None),
        1 => Ok(matches.into_iter().next()),
        _ => bail!("multiple Jellyfin items match '{name}'; use a more specific job name"),
    }
}

/// Inclusive number ranges selected by a `seasons`/`episodes` filter.
#[derive(Debug, PartialEq)]
struct NumberFilter(Vec<(u32, u32)>);

impl NumberFilter {
    fn contains(&self, number: &u32) -> bool {
        self.0
            .iter()
            .any(|(first, last)| (*first..=*last).contains(number))
    }
}

/// Parse a filter against the numbers that actually exist. `latest` / `latest-N`
/// pick the N highest existing numbers, skipping 0 (specials) unless nothing else
/// exists. None means "no filter" (absent or `all`).
fn number_filter(
    spec: Option<&serde_yaml::Value>,
    available: &[u32],
) -> Result<Option<NumberFilter>> {
    let Some(spec) = spec else {
        return Ok(None);
    };
    let parts: Vec<String> = if let Some(values) = spec.as_sequence() {
        if values.is_empty() {
            bail!("filter list must not be empty");
        }
        values
            .iter()
            .map(|value| match value {
                // Unquoted `-5` is a negative YAML number: "up to 5".
                serde_yaml::Value::Number(number) => number
                    .as_i64()
                    .map(|number| number.to_string())
                    .context("filter numbers must be integers"),
                serde_yaml::Value::String(text) => Ok(text.clone()),
                _ => bail!("filter list entries must be numbers or strings"),
            })
            .collect::<Result<_>>()?
    } else if let Some(value) = spec.as_i64() {
        vec![value.to_string()]
    } else {
        let spec = spec
            .as_str()
            .context("filter must be a string or list of numbers")?;
        if spec.trim() == "all" {
            return Ok(None);
        }
        spec.split(',').map(str::to_string).collect()
    };
    let number = |text: &str| {
        text.trim()
            .parse::<u32>()
            .with_context(|| format!("invalid filter number '{}'", text.trim()))
    };
    let mut ranges = Vec::new();
    for part in &parts {
        let part = part.trim();
        if let Some(count) = part.strip_prefix("latest") {
            let count = match count {
                "" => 1,
                _ => count
                    .strip_prefix('-')
                    .map(number)
                    .transpose()?
                    .with_context(|| format!("invalid latest filter '{part}'"))?,
            };
            if count == 0 {
                bail!("latest filter count must be positive");
            }
            let mut existing: Vec<u32> = available.to_vec();
            existing.sort_unstable();
            existing.dedup();
            if existing.iter().any(|number| *number > 0) {
                existing.retain(|number| *number > 0);
            }
            if existing.is_empty() {
                bail!("latest filter has no available media");
            }
            let skip = existing.len().saturating_sub(count as usize);
            ranges.extend(existing[skip..].iter().map(|number| (*number, *number)));
        } else if let Some(first) = part.strip_suffix('+') {
            // `13+`: season 13 and every later one.
            ranges.push((number(first)?, u32::MAX));
        } else if let Some((first, last)) = part.split_once('-') {
            // `13-` is open-ended like `13+`; `-5` means up to 5.
            let first = if first.trim().is_empty() {
                0
            } else {
                number(first)?
            };
            let last = if last.trim().is_empty() {
                u32::MAX
            } else {
                number(last)?
            };
            if last < first {
                bail!("filter range end must be greater than or equal to start");
            }
            ranges.push((first, last));
        } else {
            let number = number(part)?;
            ranges.push((number, number));
        }
    }
    Ok(Some(NumberFilter(ranges)))
}

/// The series or movie a job name refers to. Both types are searched together so
/// an exact movie title wins over a series that merely contains it (and the
/// other way round); substring matches are only a fallback.
async fn resolve_job_item(api: &JellyfinApi, name: &str) -> Result<Option<MediaItem>> {
    let found = jellyfin_items(
        api,
        &[
            ("Recursive", "true"),
            ("IncludeItemTypes", "Series,Movie"),
            ("SearchTerm", name),
            ("Fields", "Path,UserData"),
            ("Limit", "100"),
        ],
    )
    .await?;
    find_one(found, name)
}

/// Episodes of a series, or the movie itself, plus whether it is a movie.
async fn resolve_job_media(api: &JellyfinApi, name: &str) -> Result<(bool, Vec<MediaItem>)> {
    match resolve_job_item(api, name).await? {
        Some(item) if item.item_type.as_deref() == Some("Series") => Ok((
            false,
            jellyfin_items(
                api,
                &[
                    ("ParentId", item.id.as_str()),
                    ("Recursive", "true"),
                    ("IncludeItemTypes", "Episode"),
                    ("Fields", "Path,UserData,ParentIndexNumber,IndexNumber"),
                    ("Limit", "10000"),
                ],
            )
            .await?,
        )),
        Some(item) => Ok((true, vec![item])),
        None => Ok((true, Vec::new())),
    }
}

async fn jellyfin_item_ids(api: &JellyfinApi, job: &Job) -> Result<HashSet<String>> {
    let jellyfin_name = job.jellyfin_name.as_deref().unwrap_or(&job.name);
    let (_, items) = resolve_job_media(api, jellyfin_name).await?;
    if items.is_empty() {
        bail!(
            "Jellyfin returned no media for '{}'; refusing to prune its downloads",
            job.name
        );
    }
    Ok(items.into_iter().map(|item| item.id).collect())
}

async fn jellyfin_job(
    config: &Config,
    job: &Job,
    slots: Arc<Semaphore>,
    quiet: bool,
) -> Result<()> {
    let pattern = config
        .library
        .as_ref()
        .map(|l| l.season_pattern.clone())
        .unwrap_or_else(default_season_pattern);
    let (api, planned) = if job.is_auto() {
        let (api, planned, rotated) = auto_job_plan(config, job, &pattern).await?;
        // Newer items pushed these out of the window; free their space first.
        if !rotated.is_empty() {
            let removed = clear_tracked(config, &rotated, false)?;
            if !quiet {
                say!(
                    "{} {}  {}",
                    "▸".with(TerminalColor::DarkGrey),
                    job.name.clone().with(TerminalColor::Grey).bold(),
                    format!(
                        "removed {} that newer ones replaced",
                        plural(removed, "item")
                    )
                    .with(TerminalColor::DarkGrey)
                );
            }
        }
        (api, planned)
    } else {
        let (api, movie_mode, items) = jellyfin_job_items(config, job).await?;
        let destination = PathBuf::from(resolved_path(config, job, false)?);
        let planned = items
            .into_iter()
            .filter_map(|item| {
                let output =
                    item_destination(&item, &destination, &pattern, &job.name, movie_mode).ok()?;
                Some((item, output))
            })
            .collect();
        (api, planned)
    };
    download_planned(&api, job, planned, slots, quiet).await
}

/// Download planned items (with their destinations) for a job and record its status.
async fn download_planned(
    api: &JellyfinApi,
    job: &Job,
    planned: Vec<(MediaItem, PathBuf)>,
    slots: Arc<Semaphore>,
    quiet: bool,
) -> Result<()> {
    if planned.is_empty() {
        let message = if job.is_auto() {
            "Nothing new within the limits".to_string()
        } else if job.unwatched == Some(true) {
            "No new episodes (unwatched-only)".to_string()
        } else {
            "No media matched current filters".to_string()
        };
        update_job(&job.name, "skipped", &message)?;
        if !quiet {
            say!(
                "{} {}  {}",
                "▸".with(TerminalColor::DarkGrey),
                job.name.clone().with(TerminalColor::Grey).bold(),
                message.with(TerminalColor::DarkGrey)
            );
        }
        return Ok(());
    }
    let count = planned.len();
    if !quiet {
        say!(
            "{} {}  {}",
            "▸".with(TerminalColor::Cyan),
            job.name.clone().with(TerminalColor::Cyan).bold(),
            plural(count, "item").with(TerminalColor::DarkGrey)
        );
    }
    update_job(&job.name, "running", &plural(count, "item"))?;
    // Record every pending item up front so ones waiting for a worker slot show as queued.
    for (item, output) in &planned {
        if let Some(parent) = output.parent() {
            std::fs::create_dir_all(parent)?;
        }
        mark_queued(&job.name, item, output)?;
    }
    let mut tasks = JoinSet::new();
    for (item, output) in planned {
        let api = api.clone();
        let name = job.name.clone();
        // Files already on disk (or paused/ignored) finish instantly; they must
        // not wait behind running downloads for a worker slot.
        let needs_slot = !output.exists()
            && !matches!(
                transfer_status(&item.id)?.as_deref(),
                Some("paused" | "ignored")
            );
        let permit = if needs_slot {
            Some(slots.clone().acquire_owned().await?)
        } else {
            None
        };
        tasks.spawn(async move {
            let _permit = permit;
            jellyfin_download(&api, &name, &item, &output, quiet).await
        });
    }
    let mut failures = Vec::new();
    while let Some(result) = tasks.join_next().await {
        match result.context("Jellyfin transfer worker failed")? {
            Ok(()) => {}
            Err(error) => failures.push(format!("{error:#}")),
        }
    }
    if !failures.is_empty() {
        update_job(&job.name, "failed", &failures.join("; "))?;
        bail!(
            "{} failed for '{}': {}",
            plural(failures.len(), "Jellyfin download"),
            job.name,
            failures.join("; ")
        );
    }
    update_job(
        &job.name,
        "success",
        &format!("{} downloaded", plural(count, "item")),
    )?;
    Ok(())
}

/// The id of the user's Jellyfin library (view) with this name.
async fn jellyfin_library_id(api: &JellyfinApi, name: &str) -> Result<String> {
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct View {
        id: String,
        name: String,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct Views {
        items: Vec<View>,
    }
    let views: Views = api
        .client
        .get(format!("{}/Users/{}/Views", api.base, api.user_id))
        .header("Authorization", jellyfin_authorization(&api.token))
        .send()
        .await
        .context("query Jellyfin libraries")?
        .error_for_status()
        .context("Jellyfin library query failed")?
        .json()
        .await
        .context("parse Jellyfin libraries")?;
    let names: Vec<String> = views.items.iter().map(|view| view.name.clone()).collect();
    views
        .items
        .into_iter()
        .find(|view| view.name.eq_ignore_ascii_case(name))
        .map(|view| view.id)
        .with_context(|| {
            format!(
                "no Jellyfin library named '{name}' (have: {})",
                names.join(", ")
            )
        })
}

/// Pick the newest items that fit: at most `max_items`, and a total size of at
/// most `max_bytes`. Candidates come newest first; one too big for the space
/// left is skipped so smaller, older ones can still fill it. Items of unknown
/// size count as zero bytes.
fn select_auto_items(
    candidates: &[(String, Option<u64>)],
    max_items: Option<usize>,
    max_bytes: Option<u64>,
) -> Vec<String> {
    let mut selected = Vec::new();
    let mut used = 0u64;
    for (id, size) in candidates {
        if max_items.is_some_and(|max| selected.len() >= max) {
            break;
        }
        let size = size.unwrap_or(0);
        if max_bytes.is_some_and(|max| used.saturating_add(size) > max) {
            continue;
        }
        used = used.saturating_add(size);
        selected.push(id.clone());
    }
    selected
}

/// Plan an auto job: the newest unwatched movies and episodes within its limits,
/// with destinations laid out like ad-hoc downloads, and the tracked items that
/// newer ones pushed out of the window (to delete). Watched items are left to
/// the watched cleanup and its grace period.
async fn auto_job_plan(
    config: &Config,
    job: &Job,
    pattern: &str,
) -> Result<(JellyfinApi, Vec<(MediaItem, PathBuf)>, Vec<DownloadEntry>)> {
    let credentials = config
        .jellyfin
        .as_ref()
        .context("auto jobs require Jellyfin credentials")?;
    let api = jellyfin_login(credentials).await?;
    let kind = job.auto.unwrap_or(AutoKind::All);
    let mut query = vec![
        ("Recursive", "true".to_string()),
        ("IncludeItemTypes", kind.item_types().to_string()),
        ("SortBy", "DateCreated,SortName".to_string()),
        ("SortOrder", "Descending".to_string()),
        (
            "Fields",
            "Path,UserData,MediaSources,ParentIndexNumber,IndexNumber,ProductionYear".to_string(),
        ),
        ("Limit", "500".to_string()),
    ];
    if let Some(library) = job.library.as_deref() {
        query.push(("ParentId", jellyfin_library_id(&api, library).await?));
    }
    let query: Vec<(&str, &str)> = query
        .iter()
        .map(|(key, value)| (*key, value.as_str()))
        .collect();
    let items = jellyfin_items(&api, &query).await?;
    let server_played: Vec<_> = items
        .iter()
        .filter_map(|item| {
            let data = item.user_data.as_ref()?;
            Some((
                item.id.clone(),
                data.is_played?,
                data.last_played_date.as_deref().and_then(jellyfin_datetime),
            ))
        })
        .collect();
    let conn = db()?;
    record_server_watched(&conn, &server_played)?;
    let local = watched_states(&conn)?;
    let played = |item: &MediaItem| match local.get(&item.id) {
        Some(state) if state.pending => state.played,
        _ => item.user_data.as_ref().and_then(|data| data.is_played) == Some(true),
    };
    let ignored = ignored_items()?;
    // Items another job already tracks are on disk already; claiming them would
    // let this job's rotation delete files that job wants (and it would download
    // them again).
    let tracked = tracked_downloads()?;
    let owned_elsewhere: HashSet<&str> = tracked
        .iter()
        .filter(|entry| entry.job != job.name && entry.status != "cleared")
        .map(|entry| entry.item_id.as_str())
        .collect();
    let unwatched_only = job.unwatched != Some(false);
    let eligible: Vec<&MediaItem> = items
        .iter()
        .filter(|item| item.path.is_some())
        .filter(|item| !ignored.contains(&item.id))
        .filter(|item| !owned_elsewhere.contains(item.id.as_str()))
        .filter(|item| !unwatched_only || !played(item))
        .collect();
    let candidates: Vec<(String, Option<u64>)> = eligible
        .iter()
        .map(|item| {
            let size = item
                .media_sources
                .as_ref()
                .and_then(|sources| sources.first())
                .and_then(|source| source.size);
            (item.id.clone(), size)
        })
        .collect();
    let max_bytes = job
        .max_size
        .map(|gib| (gib * 1024.0 * 1024.0 * 1024.0) as u64);
    let window: HashSet<String> = select_auto_items(&candidates, job.auto_max_items(), max_bytes)
        .into_iter()
        .collect();
    let root = PathBuf::from(&config.local.root);
    let mut planned = Vec::new();
    for item in eligible
        .into_iter()
        .filter(|item| window.contains(&item.id))
    {
        let movie = item.item_type.as_deref() == Some("Movie");
        let (folder, show) = if movie {
            (
                root.join("Movies").join(safe_component(&item.name)),
                item.name.clone(),
            )
        } else {
            let series = item
                .series_name
                .clone()
                .unwrap_or_else(|| item.name.clone());
            (root.join("TV Shows").join(safe_component(&series)), series)
        };
        if let Ok(output) = item_destination(item, &folder, pattern, &show, movie) {
            planned.push((item.clone(), output));
        }
    }
    let rotated = tracked
        .into_iter()
        .filter(|entry| entry.job == job.name)
        .filter(|entry| !matches!(entry.status.as_str(), "ignored" | "downloading"))
        .filter(|entry| !window.contains(&entry.item_id))
        .filter(|entry| !local.get(&entry.item_id).is_some_and(|state| state.played))
        .collect();
    Ok((api, planned, rotated))
}

async fn jellyfin_job_items(
    config: &Config,
    job: &Job,
) -> Result<(JellyfinApi, bool, Vec<MediaItem>)> {
    let credentials = config
        .jellyfin
        .as_ref()
        .context("Jellyfin mode requires jellyfin credentials")?;
    let api = jellyfin_login(credentials).await?;
    let jellyfin_name = job.jellyfin_name.as_deref().unwrap_or(&job.name);
    let (movie_mode, mut items) = resolve_job_media(&api, jellyfin_name).await?;
    let seasons: Vec<u32> = items
        .iter()
        .filter_map(|item| item.parent_index_number)
        .collect();
    if let Some(selected) = number_filter(job.seasons.as_ref(), &seasons)? {
        items.retain(|item| {
            item.parent_index_number
                .is_some_and(|n| selected.contains(&n))
        });
    }
    // Remember what Jellyfin says; a local mark made since (maybe offline) wins.
    let server_played: Vec<_> = items
        .iter()
        .filter_map(|item| {
            let data = item.user_data.as_ref()?;
            Some((
                item.id.clone(),
                data.is_played?,
                data.last_played_date.as_deref().and_then(jellyfin_datetime),
            ))
        })
        .collect();
    let conn = db()?;
    record_server_watched(&conn, &server_played)?;
    let local = watched_states(&conn)?;
    let played = |item: &MediaItem| match local.get(&item.id) {
        Some(state) if state.pending => state.played,
        _ => item.user_data.as_ref().and_then(|data| data.is_played) == Some(true),
    };
    // Unwatched-only jobs skip watched items; so do jobs that delete watched
    // files, which would otherwise download them again after the cleanup.
    if job.unwatched == Some(true) || job.delete_watched == Some(true) {
        items.retain(|item| !played(item));
    }
    // Items removed in the TUI stay away until restored there.
    let ignored = ignored_items()?;
    items.retain(|item| !ignored.contains(&item.id));
    if let Some(filter) = &job.episodes {
        let mut by_season: BTreeMap<Option<u32>, Vec<MediaItem>> = BTreeMap::new();
        for item in items {
            by_season
                .entry(item.parent_index_number)
                .or_default()
                .push(item);
        }
        items = Vec::new();
        for mut season in by_season.into_values() {
            let episodes: Vec<u32> = season.iter().filter_map(|item| item.index_number).collect();
            if let Some(selected) = number_filter(Some(filter), &episodes)? {
                season.retain(|item| item.index_number.is_some_and(|n| selected.contains(&n)));
            }
            items.extend(season);
        }
    }
    Ok((api, movie_mode, items))
}

fn item_destination(
    item: &MediaItem,
    root: &Path,
    pattern: &str,
    show: &str,
    movie: bool,
) -> Result<PathBuf> {
    let path = item
        .path
        .as_deref()
        .context("Jellyfin item omitted its media path")?;
    // The path is the server's, which may be a Windows path with backslashes.
    let filename = path
        .rsplit(['/', '\\'])
        .next()
        .filter(|name| !name.is_empty())
        .context("Jellyfin media path has no filename")?;
    let mut folder = root.to_path_buf();
    if !movie && let Some(season) = item.parent_index_number {
        let season = pattern
            .replace("$season_number", &season.to_string())
            .replace("$name", show);
        if season.is_empty()
            || season.contains('/')
            || season.contains('\\')
            || season == "."
            || season == ".."
        {
            bail!("season pattern must produce one directory name");
        }
        folder.push(season);
    }
    if filename == "." || filename == ".." || filename.contains('/') || filename.contains('\\') {
        bail!("Jellyfin returned an unsafe filename");
    }
    Ok(folder.join(filename))
}

async fn jellyfin_download(
    api: &JellyfinApi,
    job: &str,
    item: &MediaItem,
    destination: &Path,
    quiet: bool,
) -> Result<()> {
    match jellyfin_download_inner(api, job, item, destination, quiet).await {
        Ok(()) => Ok(()),
        Err(error) => {
            let partial = PathBuf::from(format!("{}.partial", destination.to_string_lossy()));
            let bytes = tokio::fs::metadata(&partial)
                .await
                .map(|metadata| metadata.len())
                .unwrap_or(0);
            // Best effort: a busy state DB must not replace the download error.
            let total = transfer_total(&item.id).unwrap_or(None);
            if let Err(db_error) =
                update_transfer(&item.id, job, destination, "interrupted", bytes, total)
            {
                eprintln!("jellysync: could not record interrupted transfer: {db_error:#}");
            }
            RUN_STATS
                .failed
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if !quiet {
                say!(
                    "  {} {}  {}",
                    "✗".with(TerminalColor::Red).bold(),
                    item.name.clone().with(TerminalColor::White),
                    format!("{error:#}").with(TerminalColor::Red)
                );
            }
            Err(error)
        }
    }
}

fn cli_print(line: String) {
    static COLOR: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let color = *COLOR.get_or_init(|| {
        std::io::IsTerminal::is_terminal(&stdout())
            && env::var_os("NO_COLOR").is_none_or(|value| value.is_empty())
    });
    if color {
        println!("{line}");
        return;
    }
    let mut plain = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            // Skip a CSI sequence: ESC [ parameters final-byte.
            if chars.next() == Some('[') {
                for next in chars.by_ref() {
                    if ('@'..='~').contains(&next) {
                        break;
                    }
                }
            }
        } else {
            plain.push(ch);
        }
    }
    println!("{plain}");
}

/// Totals for the CLI download summary.
struct RunStats {
    downloaded: std::sync::atomic::AtomicUsize,
    bytes: std::sync::atomic::AtomicU64,
    present: std::sync::atomic::AtomicUsize,
    failed: std::sync::atomic::AtomicUsize,
}
static RUN_STATS: RunStats = RunStats {
    downloaded: std::sync::atomic::AtomicUsize::new(0),
    bytes: std::sync::atomic::AtomicU64::new(0),
    present: std::sync::atomic::AtomicUsize::new(0),
    failed: std::sync::atomic::AtomicUsize::new(0),
};

fn format_elapsed(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    if seconds >= 3600 {
        format!("{}h{:02}m", seconds / 3600, seconds / 60 % 60)
    } else if seconds >= 60 {
        format!("{}m{:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{:.1}s", elapsed.as_secs_f64())
    }
}

fn transfer_status(item_id: &str) -> Result<Option<String>> {
    Ok(db()?
        .query_row(
            "SELECT status FROM transfers WHERE item_id=?1",
            params![item_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?)
}

/// Record progress; false when the transfer was paused, cleared or ignored
/// meanwhile, in which case the download stops (and must not come back to life).
/// `rate` is the current throughput in bytes per second.
fn update_progress(
    item_id: &str,
    bytes: u64,
    total: Option<u64>,
    rate: Option<u64>,
) -> Result<bool> {
    let updated = db()?.execute(
        "UPDATE transfers SET status='downloading',bytes=?2,total=?3,rate=?4,updated_at=datetime('now') WHERE item_id=?1 AND status NOT IN ('paused','cleared','ignored')",
        params![item_id, bytes as i64, total.map(|v| v as i64), rate.map(|v| v as i64)],
    )?;
    Ok(updated > 0)
}

/// Pause a queued/running transfer or resume a paused one; returns the new status.
fn toggle_pause(entry: &DownloadEntry) -> Result<&'static str> {
    let conn = db()?;
    if entry.status == "paused" {
        conn.execute(
            "UPDATE transfers SET status='queued',updated_at=datetime('now') WHERE item_id=?1 AND status='paused'",
            params![entry.item_id],
        )?;
        Ok("queued")
    } else if matches!(
        entry.status.as_str(),
        "queued" | "downloading" | "interrupted"
    ) {
        conn.execute(
            "UPDATE transfers SET status='paused',updated_at=datetime('now') WHERE item_id=?1",
            params![entry.item_id],
        )?;
        Ok("paused")
    } else {
        bail!("only queued, downloading or interrupted files can be paused")
    }
}

fn mark_queued(job: &str, item: &MediaItem, destination: &Path) -> Result<()> {
    if destination.exists()
        || matches!(
            transfer_status(&item.id)?.as_deref(),
            Some("paused" | "ignored")
        )
    {
        return Ok(());
    }
    let partial = PathBuf::from(format!("{}.partial", destination.to_string_lossy()));
    let bytes = std::fs::metadata(&partial)
        .map(|meta| meta.len())
        .unwrap_or(0);
    update_transfer(
        &item.id,
        job,
        destination,
        "queued",
        bytes,
        transfer_total(&item.id)?,
    )
}

/// Total size from a `Content-Range` header (`bytes 0-9/10` or `bytes */10`).
fn content_range_total(response: &reqwest::Response) -> Option<u64> {
    parse_content_range_total(response.headers().get("Content-Range")?.to_str().ok()?)
}

fn parse_content_range_total(value: &str) -> Option<u64> {
    value.rsplit('/').next()?.trim().parse().ok()
}

/// Non-blocking per-item lock; None when another process holds it.
fn try_item_lock(item_id: &str) -> Result<Option<std::fs::File>> {
    let lock_dir = state_db()?
        .parent()
        .context("state DB has no parent directory")?
        .join("locks");
    std::fs::create_dir_all(&lock_dir)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(lock_dir.join(format!("item-{}.lock", safe_component(item_id))))?;
    match lock.try_lock() {
        Ok(()) => Ok(Some(lock)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
    }
}

async fn jellyfin_download_inner(
    api: &JellyfinApi,
    job: &str,
    item: &MediaItem,
    destination: &Path,
    quiet: bool,
) -> Result<()> {
    if destination.exists() {
        let size = destination.metadata()?.len();
        update_transfer(&item.id, job, destination, "complete", size, Some(size))?;
        RUN_STATS
            .present
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return Ok(());
    }
    if matches!(
        transfer_status(&item.id)?.as_deref(),
        Some("paused" | "ignored")
    ) {
        return Ok(());
    }
    // The TUI, the timer and ad-hoc workers may all reach the same item; only one
    // may write its .partial file at a time.
    let Some(_item_lock) = try_item_lock(&item.id)? else {
        return Ok(());
    };
    if destination.exists() {
        // Another process may have finished it just before we took the lock.
        return Ok(());
    }
    let started = Instant::now();
    if let Some(parent) = destination.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let partial = PathBuf::from(format!("{}.partial", destination.to_string_lossy()));
    let offset = tokio::fs::metadata(&partial)
        .await
        .map(|meta| meta.len())
        .unwrap_or(0);
    update_transfer(&item.id, job, destination, "queued", offset, None)?;
    let mut request = api
        .client
        .get(format!("{}/Items/{}/Download", api.base, item.id))
        .header("Authorization", jellyfin_authorization(&api.token));
    if offset > 0 {
        request = request.header("Range", format!("bytes={offset}-"));
    }
    let response = request
        .send()
        .await
        .context("request Jellyfin media download")?;
    if response.status() == StatusCode::RANGE_NOT_SATISFIABLE
        && offset > 0
        && content_range_total(&response) == Some(offset)
    {
        // The partial file already holds the whole item; nothing left to fetch.
        tokio::fs::rename(&partial, destination).await?;
        update_transfer(&item.id, job, destination, "complete", offset, Some(offset))?;
        RUN_STATS
            .downloaded
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return Ok(());
    }
    let response = if response.status() == StatusCode::RANGE_NOT_SATISFIABLE && offset > 0 {
        tokio::fs::remove_file(&partial).await?;
        api.client
            .get(format!("{}/Items/{}/Download", api.base, item.id))
            .header("Authorization", jellyfin_authorization(&api.token))
            .send()
            .await
            .context("restart Jellyfin media download")?
    } else {
        response
    };
    if response.status() != StatusCode::OK && response.status() != StatusCode::PARTIAL_CONTENT {
        bail!(
            "Jellyfin download for '{}' returned {}",
            item.name,
            response.status()
        );
    }
    let append = offset > 0 && response.status() == StatusCode::PARTIAL_CONTENT;
    if append {
        let range_start = response
            .headers()
            .get("Content-Range")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split_once(' ').map(|(_, range)| range))
            .and_then(|range| range.split('-').next())
            .and_then(|value| value.parse::<u64>().ok());
        if range_start != Some(offset) {
            bail!("Jellyfin resumed the download at an unexpected byte offset");
        }
    }
    let start = if append { offset } else { 0 };
    let total = content_range_total(&response)
        .or_else(|| response.content_length().map(|length| start + length));
    let mut file = if append {
        tokio::fs::OpenOptions::new()
            .append(true)
            .open(&partial)
            .await?
    } else {
        tokio::fs::File::create(&partial).await?
    };
    if !update_progress(&item.id, start, total, None)? {
        return Ok(());
    }
    let mut stream = response.bytes_stream();
    let mut bytes = start;
    let mut checkpoint = start;
    let mut checkpoint_at = Instant::now();
    let mut rate: Option<f64> = None;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("read Jellyfin download stream")?;
        tokio::io::AsyncWriteExt::write_all(&mut file, &chunk).await?;
        bytes += chunk.len() as u64;
        let elapsed = checkpoint_at.elapsed();
        if elapsed >= PROGRESS_INTERVAL {
            // Smooth the per-interval throughput so the display does not jitter.
            let sample = (bytes - checkpoint) as f64 / elapsed.as_secs_f64();
            let smoothed = rate.map_or(sample, |previous| previous * 0.7 + sample * 0.3);
            rate = Some(smoothed);
            if !update_progress(&item.id, bytes, total, Some(smoothed as u64))? {
                // Paused: keep the .partial file and free this worker slot.
                tokio::io::AsyncWriteExt::flush(&mut file).await?;
                return Ok(());
            }
            checkpoint = bytes;
            checkpoint_at = Instant::now();
        }
    }
    tokio::io::AsyncWriteExt::flush(&mut file).await?;
    file.sync_all().await?;
    drop(file);
    // A stream that ends early (e.g. a proxy cutting a chunked body) must not be
    // promoted to the final name; keep the partial data for the next resume.
    if let Some(total) = total
        && bytes != total
    {
        if bytes > total {
            tokio::fs::remove_file(&partial).await?;
        }
        bail!(
            "download of '{}' ended at {} of {} bytes",
            item.name,
            bytes,
            total
        );
    }
    tokio::fs::rename(&partial, destination).await?;
    update_transfer(
        &item.id,
        job,
        destination,
        "complete",
        bytes,
        total.or(Some(bytes)),
    )?;
    let transferred = bytes.saturating_sub(start);
    RUN_STATS
        .downloaded
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    RUN_STATS
        .bytes
        .fetch_add(transferred, std::sync::atomic::Ordering::Relaxed);
    if !quiet {
        let rate = transferred as f64 / started.elapsed().as_secs_f64().max(0.001);
        say!(
            "  {} {}  {}  {}",
            "✓".with(TerminalColor::Green).bold(),
            destination
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string()
                .with(TerminalColor::White),
            format_bytes(bytes).with(TerminalColor::Grey),
            format!("{}/s", format_bytes(rate as u64)).with(TerminalColor::DarkGrey)
        );
    }
    Ok(())
}

async fn run_sync_mode(
    config: Config,
    target: Vec<String>,
    parallelism: usize,
    quiet: bool,
) -> Result<()> {
    let jobs = selected_jobs(&config, &target)?;
    let started = Instant::now();
    if !quiet {
        say!(
            "{} {}  {}",
            "◆".with(TerminalColor::Cyan),
            "jellysync download".bold(),
            format!(
                "{} · {} mode · {}",
                plural(jobs.len(), "job"),
                config.download.mode,
                plural(parallelism, "worker")
            )
            .with(TerminalColor::DarkGrey)
        );
        say!("{}", "─".repeat(48).with(TerminalColor::DarkGrey));
    }
    // Push marks made in the TUI (maybe offline) before the filters use
    // Jellyfin's played state, and refresh the local copy of it.
    if config.download.mode == "jellyfin"
        && config.jellyfin.is_some()
        && let Err(error) = sync_watched(&config).await
        && !quiet
    {
        say!(
            "{} {}",
            "!".with(TerminalColor::Yellow).bold(),
            format!("could not sync the watched state: {error:#}").with(TerminalColor::Yellow)
        );
    }
    let slots = Arc::new(Semaphore::new(parallelism));
    let mut tasks = JoinSet::new();
    for job in jobs.clone() {
        let config = config.clone();
        let slots = slots.clone();
        tasks.spawn(async move { sync_job(config, job, slots, quiet).await });
    }
    let mut failures = Vec::new();
    while let Some(result) = tasks.join_next().await {
        match result.context("sync worker failed")? {
            Ok(()) => {}
            Err(error) => failures.push(format!("{error:#}")),
        }
    }
    // Jobs with delete_watched drop watched files once their grace period is over.
    let due: Vec<DownloadEntry> = watched_cleanup(&config, &jobs, false)?
        .into_iter()
        .filter(|(_, remaining)| *remaining == 0)
        .map(|(entry, _)| entry)
        .collect();
    if !due.is_empty() {
        match clear_tracked(&config, &due, false) {
            Ok(removed) => {
                if !quiet {
                    say!(
                        "{} {}",
                        "✗".with(TerminalColor::DarkGrey),
                        format!("removed {} watched", plural(removed, "file"))
                            .with(TerminalColor::Grey)
                    );
                }
            }
            Err(error) => failures.push(format!("watched cleanup: {error:#}")),
        }
    }
    if !quiet {
        use std::sync::atomic::Ordering::Relaxed;
        let downloaded = RUN_STATS.downloaded.load(Relaxed);
        let present = RUN_STATS.present.load(Relaxed);
        let failed = RUN_STATS.failed.load(Relaxed);
        say!("{}", "─".repeat(48).with(TerminalColor::DarkGrey));
        let mut summary = vec![
            format!(
                "{} {downloaded} downloaded ({})",
                "✓".with(TerminalColor::Green).bold(),
                format_bytes(RUN_STATS.bytes.load(Relaxed))
            ),
            format!("{present} already present")
                .with(TerminalColor::Grey)
                .to_string(),
        ];
        if failed > 0 || !failures.is_empty() {
            summary.push(
                format!("✗ {} failed", failed.max(failures.len()))
                    .with(TerminalColor::Red)
                    .bold()
                    .to_string(),
            );
        }
        summary.push(
            format_elapsed(started.elapsed())
                .with(TerminalColor::DarkGrey)
                .to_string(),
        );
        say!(
            "{}",
            summary.join(&" · ".with(TerminalColor::DarkGrey).to_string())
        );
    }
    if !failures.is_empty() {
        bail!(
            "{} failed:\n{}",
            plural(failures.len(), "sync job"),
            failures.join("\n")
        );
    }
    Ok(())
}
/// Apply `download`'s auto options: a target named `auto` without a configured
/// job of that name gets an implicit auto job (all media), and `--max-items` /
/// `--max-size` override the limits of the auto jobs being synced.
fn with_auto_target(
    mut config: Config,
    targets: &[String],
    max_items: Option<usize>,
    max_size: Option<f64>,
) -> Result<Config> {
    if targets.iter().any(|target| target == "auto")
        && config.jobs.iter().all(|job| job.name != "auto")
    {
        config
            .jobs
            .push(serde_yaml::from_str("{name: auto, auto: true}")?);
    }
    if max_items.is_none() && max_size.is_none() {
        return Ok(config);
    }
    let mut applied = false;
    for job in &mut config.jobs {
        if job.is_auto() && (targets.is_empty() || targets.contains(&job.name)) {
            // A single CLI limit replaces both of the job's limits, so
            // `--max-size 20` is not still capped by a configured max_items.
            job.max_items = max_items;
            job.max_size = max_size;
            applied = true;
        }
    }
    if !applied {
        bail!("--max-items and --max-size only apply to auto jobs (try `jellysync download auto`)");
    }
    parse_config(serde_yaml::to_value(&config)?)
}

fn selected_jobs(config: &Config, targets: &[String]) -> Result<Vec<Job>> {
    let jobs: Vec<Job> = if targets.is_empty() {
        if config.jobs.is_empty() {
            bail!("no matching jobs configured");
        }
        // Disabled jobs only run when named explicitly; all disabled is not an error.
        return Ok(config
            .jobs
            .iter()
            .filter(|job| job.enabled())
            .cloned()
            .collect());
    } else {
        let found: Vec<Job> = config
            .jobs
            .iter()
            .filter(|job| targets.iter().any(|target| target == &job.name))
            .cloned()
            .collect();
        for target in targets {
            if !found.iter().any(|job| &job.name == target) {
                bail!("unknown job '{target}'");
            }
        }
        found
    };
    if jobs.is_empty() {
        bail!("no matching jobs configured");
    }
    Ok(jobs)
}
async fn sync_job(config: Config, job: Job, slots: Arc<Semaphore>, quiet: bool) -> Result<()> {
    if config.download.mode == "jellyfin" {
        return jellyfin_job(&config, &job, slots, quiet).await;
    }
    if job.seasons.is_some()
        || job.episodes.is_some()
        || job.wildcard == Some(true)
        || job.unwatched == Some(true)
    {
        bail!("season, episode, wildcard, and unwatched filters require Jellyfin download mode");
    }
    let _permit = slots.acquire_owned().await?;
    let source = resolved_path(&config, &job, true)?;
    let destination = resolved_path(&config, &job, false)?;
    std::fs::create_dir_all(&destination).with_context(|| format!("create {}", destination))?;
    update_job(&job.name, "running", &destination)?;
    let remote = format!(
        "{}@{}:{}",
        config.remote.username, config.remote.hostname, source
    );
    let mut command = Command::new("rsync");
    command
        .args(&config.rsync.flags)
        .arg("--partial")
        .arg("-e")
        .arg(format!("ssh -p {}", config.remote.port))
        .arg(format!("{remote}/"))
        .arg(format!("{destination}/"))
        .stdin(Stdio::null());
    let status = command.status().await.context("start rsync")?;
    if !status.success() {
        update_job(&job.name, "failed", &format!("rsync exited with {status}"))?;
        bail!("job '{}' failed: rsync exited with {status}", job.name);
    }
    update_job(&job.name, "success", &destination)?;
    if !quiet {
        say!(
            "{} {}  {} {}",
            "✓".with(TerminalColor::Green).bold(),
            job.name.clone().with(TerminalColor::Cyan).bold(),
            "rsync →".with(TerminalColor::DarkGrey),
            destination.clone().with(TerminalColor::Grey)
        );
    }
    Ok(())
}
/// A short, one-line summary of what a job syncs, e.g.
/// `tv_shows · seasons latest · unwatched only · deletes watched after 7d`.
fn job_summary(job: &Job, config: &Config) -> String {
    let mut parts = Vec::new();
    if let Some(kind) = job.auto {
        let kind = match kind {
            AutoKind::All => "all",
            AutoKind::Movies => "movies",
            AutoKind::Shows => "shows",
        };
        parts.push(format!("auto: {kind}"));
        match (job.auto_max_items(), job.max_size) {
            (Some(items), Some(size)) => parts.push(format!("max {items} items, {size} GiB")),
            (Some(items), None) => parts.push(format!("max {items} items")),
            (None, Some(size)) => parts.push(format!("max {size} GiB")),
            (None, None) => {}
        }
        if let Some(library) = &job.library {
            parts.push(format!("library {library}"));
        }
    } else {
        if let Some(directory) = job.directory.clone().or(job.local_dir.clone()) {
            parts.push(directory);
        }
        if let Some(seasons) = job_setting_text(job, "seasons") {
            parts.push(format!("seasons {seasons}"));
        }
        if let Some(episodes) = job_setting_text(job, "episodes") {
            parts.push(format!("episodes {episodes}"));
        }
        if job.unwatched == Some(true) {
            parts.push("unwatched only".into());
        }
        if job.wildcard == Some(true) {
            parts.push("wildcard".into());
        }
    }
    if let Some(grace) = job.watched_grace(config) {
        parts.push(if grace.is_zero() {
            "deletes watched".into()
        } else {
            format!(
                "deletes watched after {}",
                format_duration_short(grace.as_secs())
            )
        });
    }
    if !job.enabled() {
        parts.push("disabled".into());
    }
    parts.join(" · ")
}

/// Terminal width for `status` output, or None when stdout is not a terminal.
fn status_width() -> Option<usize> {
    if !std::io::IsTerminal::is_terminal(&stdout()) {
        return None;
    }
    crossterm::terminal::size()
        .ok()
        .map(|(columns, _)| usize::from(columns))
}

/// `config` is optional: status still works when the config cannot be loaded,
/// just without the per-job summaries.
fn status(json_output: bool, config: Option<&Config>) -> Result<()> {
    let conn = db()?;
    if json_output {
        return status_json(&conn, config);
    }
    println!("\n{} {}", "◆".with(TerminalColor::Cyan), "Jellysync".bold());
    println!("{}", "─".repeat(48).with(TerminalColor::DarkGrey));
    if let Ok(overlay) = state_config_path()
        && overlay.is_file()
    {
        println!(
            "{} {}",
            "settings overlay".with(TerminalColor::DarkGrey),
            tilde_path(&overlay).with(TerminalColor::Grey)
        );
    }
    println!("{}", "JOBS".with(TerminalColor::Magenta).bold());
    // Active downloads, shown under their job.
    type Active = (String, String, u64, Option<u64>, Option<u64>);
    let mut active: BTreeMap<String, Vec<Active>> = BTreeMap::new();
    let mut statement = conn.prepare(&format!("SELECT job,path,status,bytes,total,{LIVE_RATE_SQL} FROM transfers WHERE status IN ('queued','downloading','interrupted','paused') ORDER BY job,path"))?;
    for row in statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            (
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, u64>(3)?,
                row.get::<_, Option<u64>>(4)?,
                row.get::<_, Option<u64>>(5)?,
            ),
        ))
    })? {
        let (job, download) = row?;
        active.entry(job).or_default().push(download);
    }
    let file_states: HashMap<&str, Vec<&str>> = active
        .iter()
        .map(|(job, downloads)| {
            let states = downloads
                .iter()
                .map(|(_, state, ..)| state.as_str())
                .filter(|state| matches!(*state, "queued" | "downloading"))
                .collect();
            (job.as_str(), states)
        })
        .collect();
    let width = status_width();
    let summary_width = width.map_or(88, |width| width.saturating_sub(6).max(20));
    let configured_summary = |name: &str| {
        let config = config?;
        match config.jobs.iter().find(|job| job.name == name) {
            Some(job) => Some(job_summary(job, config)),
            None if name.starts_with("library:") => Some("ad-hoc download".into()),
            None => None,
        }
    };
    let mut total_rate: Option<u64> = None;
    let mut print_downloads = |job: &str| {
        for (path, state, bytes, total, rate) in active.get(job).into_iter().flatten() {
            let (filled, track, percent) = download_progress(*bytes, *total, 20);
            let filename = Path::new(path)
                .file_name()
                .unwrap_or_default()
                .to_string_lossy();
            let state_color = match state.as_str() {
                "downloading" => TerminalColor::Cyan,
                "queued" => TerminalColor::Blue,
                "paused" => TerminalColor::DarkGrey,
                "interrupted" => TerminalColor::Red,
                _ => TerminalColor::DarkGrey,
            };
            // Like the TUI: ellipsize near the end so the episode tag and
            // extension stay visible (full names when piped).
            let filename = match width {
                Some(width) => truncate_near_end(&filename, width.saturating_sub(8).max(20)),
                None => filename.into_owned(),
            };
            // The icon carries the state; queued files have no progress yet.
            println!("      {} {}", state_icon(state).with(state_color), filename);
            if state != "queued" {
                println!(
                    "        {}{} {}  {}",
                    filled.with(state_color),
                    track.with(TerminalColor::DarkGrey),
                    percent.with(state_color),
                    rate.map(|rate| format_throughput(rate, *bytes, *total))
                        .unwrap_or_default()
                        .with(TerminalColor::Grey)
                );
            }
            if let Some(rate) = rate {
                total_rate = Some(total_rate.unwrap_or(0) + rate);
            }
        }
    };
    let mut statement =
        conn.prepare("SELECT name,status,message,updated_at FROM jobs ORDER BY name")?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;
    let mut history: Vec<(String, String, Option<String>, String)> =
        rows.collect::<rusqlite::Result<_>>()?;
    // Configured jobs that never ran, and jobs known only from their downloads.
    for name in config
        .map_or(&[][..], |config| config.jobs.as_slice())
        .iter()
        .map(|job| job.name.clone())
        .chain(active.keys().cloned())
    {
        if !history.iter().any(|(known, ..)| *known == name) {
            history.push((name, "not run".into(), None, String::new()));
        }
    }
    for (name, state, message, time) in &history {
        let (state, summary) = job_display_state(
            state,
            file_states
                .get(name.as_str())
                .into_iter()
                .flatten()
                .copied(),
        );
        let message = summary.or(message.clone());
        let color = match state.to_ascii_lowercase().as_str() {
            "success" | "complete" | "completed" => TerminalColor::Green,
            "running" => TerminalColor::Yellow,
            "downloading" => TerminalColor::Cyan,
            "queued" => TerminalColor::Blue,
            "paused" => TerminalColor::DarkGrey,
            "failed" | "error" | "skipped" => TerminalColor::Red,
            "interrupted" | "cleared" => TerminalColor::Magenta,
            _ => TerminalColor::DarkGrey,
        };
        let icon = state_icon(state);
        // Pad before styling: styled text ignores the width.
        println!(
            "  {} {} {}",
            icon.with(color),
            format!("{:<28}", truncate_near_end(name, 28)).bold(),
            time.as_str().dim()
        );
        if let Some(summary) = configured_summary(name).filter(|summary| !summary.is_empty()) {
            println!("      {}", truncate(&summary, summary_width).dim());
        }
        if let Some(message) = message.filter(|message| !message.is_empty()) {
            println!(
                "      {}",
                truncate(&message, summary_width).with(TerminalColor::DarkGrey)
            );
        }
        print_downloads(name);
    }
    if history.is_empty() {
        println!("  {}", "No jobs have run yet".with(TerminalColor::DarkGrey));
    }
    if let Some(rate) = total_rate {
        println!(
            "\n{} {}",
            format!("{} DOWNLOADING", icon::DOWNLOAD)
                .with(TerminalColor::Magenta)
                .bold(),
            format!("{}/s in total", format_bytes(rate)).with(TerminalColor::Grey)
        );
    }
    if let Ok(output) = std::process::Command::new("systemctl")
        .args(["--user", "is-active", "jellysync.timer"])
        .output()
    {
        let timer = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let color = if timer == "active" {
            TerminalColor::Green
        } else {
            TerminalColor::DarkGrey
        };
        println!(
            "\n{} {}",
            "TIMER".with(TerminalColor::Magenta).bold(),
            timer.with(color)
        );
    }
    Ok(())
}
fn status_json(conn: &Connection, config: Option<&Config>) -> Result<()> {
    let summary = |name: &str| {
        config.and_then(
            |config| match config.jobs.iter().find(|job| job.name == name) {
                Some(job) => Some(job_summary(job, config)),
                None if name.starts_with("library:") => Some("ad-hoc download".into()),
                None => None,
            },
        )
    };
    let mut statement =
        conn.prepare("SELECT name,status,message,updated_at FROM jobs ORDER BY name")?;
    let jobs = statement
        .query_map([], |row| {
            let name = row.get::<_, String>(0)?;
            Ok(json!({
                "config": summary(&name),
                "name": name,
                "status": row.get::<_, String>(1)?,
                "message": row.get::<_, Option<String>>(2)?,
                "updated_at": row.get::<_, String>(3)?,
            }))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut statement = conn.prepare(&format!("SELECT job,path,status,bytes,total,{LIVE_RATE_SQL} FROM transfers WHERE status IN ('queued','downloading','interrupted','paused') ORDER BY job,path"))?;
    let downloads = statement
        .query_map([], |row| {
            Ok(json!({
                "job": row.get::<_, String>(0)?,
                "path": row.get::<_, String>(1)?,
                "status": row.get::<_, String>(2)?,
                "bytes": row.get::<_, u64>(3)?,
                "total": row.get::<_, Option<u64>>(4)?,
                "rate": row.get::<_, Option<u64>>(5)?,
            }))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let timer = std::process::Command::new("systemctl")
        .args(["--user", "is-active", "jellysync.timer"])
        .output()
        .ok()
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string());
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "jobs": jobs,
            "downloads": downloads,
            "timer": timer,
        }))?
    );
    Ok(())
}
/// The global grace period before watched files are deleted.
fn default_watched_grace(config: &Config) -> Duration {
    config
        .cleanup
        .as_ref()
        .and_then(|cleanup| cleanup.delete_watched_after.as_deref())
        .and_then(|spec| parse_duration(spec).ok())
        .unwrap_or(Duration::from_secs(7 * 86_400))
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}

/// Seconds until a file watched at `played_at` is due for deletion (0 = due).
fn cleanup_remaining(played_at: Option<&str>, grace: Duration, now: i64) -> u64 {
    // Unknown play time: count from now, so it is never deleted early.
    let played_at = played_at.and_then(sqlite_utc_seconds).unwrap_or(now);
    (played_at + grace.as_secs() as i64 - now).max(0) as u64
}

/// Downloaded, watched files of `jobs` with the seconds left until each is
/// deleted. Jobs without `delete_watched` only count when `force` is set
/// (`prune --watched`), with the global grace period.
fn watched_cleanup(
    config: &Config,
    jobs: &[Job],
    force: bool,
) -> Result<Vec<(DownloadEntry, u64)>> {
    let states = watched_states(&db()?)?;
    let now = unix_now();
    let mut candidates = Vec::new();
    for entry in tracked_downloads()? {
        let Some(job) = jobs.iter().find(|job| job.name == entry.job) else {
            continue;
        };
        let Some(grace) = job
            .watched_grace(config)
            .or_else(|| force.then(|| default_watched_grace(config)))
        else {
            continue;
        };
        let Some(state) = states.get(&entry.item_id).filter(|state| state.played) else {
            continue;
        };
        if download_complete(&entry.status) {
            let remaining = cleanup_remaining(state.played_at.as_deref(), grace, now);
            candidates.push((entry, remaining));
        }
    }
    Ok(candidates)
}

/// A file prune removes (or would remove) and why.
#[derive(Serialize)]
struct Pruned {
    path: String,
    reason: &'static str,
}

/// Remove tracked files whose media is gone from Jellyfin, and watched files
/// past their grace period (jobs with `delete_watched`, or all jobs with
/// `watched`). Watched state works offline; the check for removed media needs
/// Jellyfin and is skipped without it.
async fn prune(
    config: Config,
    target: Vec<String>,
    dry_run: bool,
    watched: bool,
    json_output: bool,
) -> Result<Vec<Pruned>> {
    let jobs = selected_jobs(&config, &target)?;
    if config.download.mode == "jellyfin" {
        let mut doomed: Vec<(DownloadEntry, &'static str)> = Vec::new();
        let online = match sync_watched(&config).await {
            Ok(_) => true,
            Err(error) => {
                if !json_output {
                    eprintln!(
                        "Jellyfin unavailable ({error:#}); using the local watched state and \
                         skipping the check for media removed from Jellyfin"
                    );
                }
                false
            }
        };
        if online {
            let api = jellyfin_login(
                config
                    .jellyfin
                    .as_ref()
                    .context("Jellyfin mode requires credentials")?,
            )
            .await?;
            let tracked = tracked_downloads()?;
            // Auto jobs rotate their own items out; they have no title to check.
            for job in jobs.iter().filter(|job| !job.is_auto()) {
                let current = jellyfin_item_ids(&api, job).await?;
                doomed.extend(
                    tracked
                        .iter()
                        .filter(|entry| entry.job == job.name && entry.status != "ignored")
                        .filter(|entry| !current.contains(&entry.item_id))
                        .map(|entry| (entry.clone(), "no longer in Jellyfin")),
                );
            }
        }
        for (entry, remaining) in watched_cleanup(&config, &jobs, watched)? {
            if doomed
                .iter()
                .any(|(other, _)| other.item_id == entry.item_id)
            {
                continue;
            }
            if remaining == 0 {
                doomed.push((entry, "watched"));
            } else if dry_run && !json_output {
                println!(
                    "keeping {} (watched, deleted in {})",
                    entry.path.display(),
                    format_duration_short(remaining)
                );
            }
        }
        for (entry, reason) in &doomed {
            if !json_output {
                let verb = if dry_run { "would delete" } else { "deleting" };
                println!("{verb} {} ({reason})", entry.path.display());
            }
        }
        if !dry_run && !doomed.is_empty() {
            let entries: Vec<_> = doomed.iter().map(|(entry, _)| entry.clone()).collect();
            clear_tracked(&config, &entries, false)?;
        }
        return Ok(doomed
            .into_iter()
            .map(|(entry, reason)| Pruned {
                path: entry.path.display().to_string(),
                reason,
            })
            .collect());
    }
    if watched {
        bail!("prune --watched needs Jellyfin download mode");
    }
    if jobs.iter().any(|job| {
        job.seasons.is_some()
            || job.episodes.is_some()
            || job.wildcard == Some(true)
            || job.unwatched == Some(true)
    }) {
        bail!(
            "refusing to prune filtered jobs until their filters are implemented in the Rust build"
        );
    }
    for job in jobs {
        let source = resolved_path(&config, &job, true)?;
        let destination = resolved_path(&config, &job, false)?;
        let remote = format!(
            "{}@{}:{}",
            config.remote.username, config.remote.hostname, source
        );
        let mut command = Command::new("rsync");
        command.args(&config.rsync.flags).arg("--delete");
        if dry_run {
            command.arg("--dry-run");
        }
        command
            .arg("-e")
            .arg(format!("ssh -p {}", config.remote.port))
            .arg(format!("{remote}/"))
            .arg(format!("{destination}/"));
        if json_output {
            command.arg("--quiet");
        }
        let status = command.status().await.context("start rsync prune")?;
        if !status.success() {
            bail!("prune for '{}' failed with {status}", job.name);
        }
    }
    Ok(Vec::new())
}
#[derive(Clone, Copy, PartialEq, Debug)]
enum ClearScope {
    File,
    Show,
}

/// A pending clear: the exact files it removes are fixed when it is requested,
/// so list refreshes or re-sorting before the confirmation cannot change them.
struct ClearRequest {
    scope: ClearScope,
    job: String,
    adhoc: bool,
    /// Also ignore the items so syncs do not download them again.
    ignore: bool,
    targets: Vec<DownloadEntry>,
    label: String,
}

/// Build a clear request for the selected job, or None when there is nothing to
/// clear. Ignoring only applies to configured jobs; ad-hoc downloads are never
/// synced again anyway.
fn clear_request(
    scope: ClearScope,
    ignore: bool,
    job: &TuiJob,
    downloads: &[DownloadEntry],
    selected: usize,
) -> Option<ClearRequest> {
    let files = |count: usize| format!("{count} {}", if count == 1 { "file" } else { "files" });
    let show = job.name.trim_start_matches("library:");
    let (targets, label) = match scope {
        ClearScope::File => {
            let entry = downloads.get(selected)?;
            (
                vec![entry.clone()],
                entry
                    .path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
            )
        }
        ClearScope::Show => {
            // An ad-hoc job without files can still be removed from the list.
            if downloads.is_empty() && !job.adhoc {
                return None;
            }
            (
                downloads.to_vec(),
                format!("{show} · all {}", files(downloads.len())),
            )
        }
    };
    if targets.is_empty() && scope != ClearScope::Show {
        return None;
    }
    Some(ClearRequest {
        scope,
        job: job.name.clone(),
        adhoc: job.adhoc,
        ignore: ignore && !job.adhoc,
        targets,
        label,
    })
}

/// Indices of the files in the same season group as the file at `selected`.
/// Without season headings, files sharing its folder form the group.
fn season_files(rows: &[FileRow], downloads: &[DownloadEntry], selected: usize) -> Vec<usize> {
    let Some(position) = rows
        .iter()
        .position(|row| matches!(row, FileRow::File(index) if *index == selected))
    else {
        return Vec::new();
    };
    let grouped = rows.iter().any(|row| matches!(row, FileRow::Season(_)));
    if !grouped {
        let folder = downloads
            .get(selected)
            .and_then(|entry| entry.path.parent());
        return (0..downloads.len())
            .filter(|index| downloads[*index].path.parent() == folder)
            .collect();
    }
    let start = rows[..position]
        .iter()
        .rposition(|row| matches!(row, FileRow::Season(_)))
        .map_or(0, |heading| heading + 1);
    rows[start..]
        .iter()
        .map_while(|row| match row {
            FileRow::File(index) => Some(*index),
            FileRow::Season(_) => None,
        })
        .collect()
}
#[derive(Clone)]
struct DownloadEntry {
    item_id: String,
    job: String,
    path: PathBuf,
    status: String,
    bytes: u64,
    total: Option<u64>,
    /// Live throughput in bytes per second while downloading.
    rate: Option<u64>,
}

#[derive(Default)]
struct DashboardSnapshot {
    history: BTreeMap<String, (String, String, String)>,
    downloads: Vec<DownloadEntry>,
    watched: HashMap<String, WatchedState>,
    timer: String,
}

#[derive(Clone)]
struct TuiJob {
    name: String,
    adhoc: bool,
}

/// Jobs and transfers from the state DB; the timer state (a `systemctl` call)
/// only when `check_timer` is set, otherwise left empty.
fn dashboard_snapshot(check_timer: bool) -> Result<DashboardSnapshot> {
    let conn = db()?;
    let mut history = BTreeMap::new();
    let mut statement = conn.prepare("SELECT name,status,message,updated_at FROM jobs")?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;
    for row in rows {
        let (name, state, message, time) = row?;
        history.insert(name, (state, message.unwrap_or_default(), time));
    }
    let downloads = tracked_downloads()?;
    let timer = if check_timer {
        std::process::Command::new("systemctl")
            .args(["--user", "is-active", "jellysync.timer"])
            .output()
            .ok()
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
            .filter(|state| !state.is_empty())
            .unwrap_or_else(|| "unknown".into())
    } else {
        String::new()
    };
    let watched = watched_states(&conn)?;
    Ok(DashboardSnapshot {
        history,
        downloads,
        watched,
        timer,
    })
}

fn tracked_downloads() -> Result<Vec<DownloadEntry>> {
    let conn = db()?;
    let mut statement = conn.prepare(&format!(
        "SELECT item_id,job,path,status,bytes,total,{LIVE_RATE_SQL} FROM transfers WHERE status != 'cleared' ORDER BY job,path"
    ))?;
    let rows = statement.query_map([], |row| {
        Ok(DownloadEntry {
            item_id: row.get(0)?,
            job: row.get(1)?,
            path: PathBuf::from(row.get::<_, String>(2)?),
            status: row.get(3)?,
            bytes: row.get(4)?,
            total: row.get(5)?,
            rate: row.get(6)?,
        })
    })?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

/// Delete the given tracked files (and their partial data) and mark them
/// cleared, or ignored so that syncs do not download them again.
fn clear_tracked(config: &Config, targets: &[DownloadEntry], ignore: bool) -> Result<usize> {
    let mut roots = vec![
        PathBuf::from(&config.local.root)
            .canonicalize()
            .context("local root must exist before clearing downloads")?,
    ];
    // Jobs may download outside local.root (absolute local_dir or directory).
    let job_names: HashSet<_> = targets.iter().map(|entry| entry.job.clone()).collect();
    for job in &job_names {
        if let Ok(dir) = job_directory(config, job).and_then(|dir| Ok(dir.canonicalize()?)) {
            roots.push(dir);
        }
    }
    // Check every path before deleting anything so a refusal never leaves a
    // half-cleared selection behind.
    let mut doomed = Vec::new();
    for entry in targets {
        let partial = PathBuf::from(format!("{}.partial", entry.path.to_string_lossy()));
        for path in [&entry.path, &partial] {
            if path.symlink_metadata().is_err() {
                continue;
            }
            if !path_within_roots(path, &roots)? {
                bail!(
                    "refusing to clear {} outside the configured download directories",
                    path.display()
                );
            }
            doomed.push(path.clone());
        }
    }
    for path in doomed {
        // Removes a symlink itself, never its target.
        std::fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
    }
    let conn = db()?;
    for entry in targets {
        conn.execute(
            "UPDATE transfers SET status=?2,updated_at=datetime('now') WHERE item_id=?1",
            params![entry.item_id, if ignore { "ignored" } else { "cleared" }],
        )?;
    }
    for job in job_names {
        if !job.starts_with("library:") {
            continue;
        }
        let remaining: i64 = conn.query_row(
            "SELECT COUNT(*) FROM transfers WHERE job=?1 AND status != 'cleared'",
            params![job],
            |row| row.get(0),
        )?;
        if remaining == 0 {
            conn.execute("DELETE FROM jobs WHERE name=?1", params![job])?;
        }
    }
    Ok(targets.len())
}

async fn reconcile_existing(config: &Config) -> Result<usize> {
    if config.download.mode != "jellyfin" {
        return Ok(0);
    }
    let pattern = config
        .library
        .as_ref()
        .map(|library| library.season_pattern.clone())
        .unwrap_or_else(default_season_pattern);
    let mut indexed = 0;
    // Auto jobs have no title to look up; their files are tracked as they download.
    for job in config.jobs.iter().filter(|job| !job.is_auto()) {
        let mut scan_job = job.clone();
        scan_job.seasons = None;
        scan_job.episodes = None;
        scan_job.unwatched = None;
        let (_, movie_mode, items) = jellyfin_job_items(config, &scan_job).await?;
        let destination = PathBuf::from(resolved_path(config, job, false)?);
        for item in items {
            let path = item_destination(&item, &destination, &pattern, &job.name, movie_mode)?;
            let Ok(metadata) = std::fs::metadata(&path) else {
                continue;
            };
            if metadata.is_file() {
                update_transfer(
                    &item.id,
                    &job.name,
                    &path,
                    "complete",
                    metadata.len(),
                    Some(metadata.len()),
                )?;
                indexed += 1;
            }
        }
    }
    Ok(indexed)
}

/// Open the editable config file in $EDITOR and reload the config afterwards.
fn edit_config_file(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    config_path: &Path,
    config: &mut Config,
) -> (String, bool) {
    let result = editable_config_file(config_path).and_then(|path| {
        run_editor(terminal, &path)?;
        Ok((path, load_config(config_path)?))
    });
    match result {
        Ok((path, reloaded)) => {
            *config = reloaded;
            (
                format!("Reloaded settings after editing {}", tilde_path(&path)),
                true,
            )
        }
        Err(error) => (format!("Settings not reloaded: {error:#}"), false),
    }
}

async fn tui(mut config: Config, config_path: PathBuf) -> Result<()> {
    let mut out = stdout();
    enable_raw_mode()?;
    execute!(out, EnterAlternateScreen, EnableMouseCapture)?;
    let result = async move {
        let backend = CrosstermBackend::new(out);
        let mut terminal = Terminal::new(backend)?;
        let tmux_client = tmux_active_client();
        let query_started = Instant::now();
        let picker = Picker::from_query_stdio().unwrap_or_else(|_| Picker::from_fontsize((8, 16)));
        let mut picker = with_viewer_cell_size(picker);
        let mut cell_checked_at = Instant::now();
        if let Some(client) = tmux_client {
            reclaim_tmux_window(client);
        }
        if query_started.elapsed() >= Duration::from_millis(900) {
            // ratatui-image gives up after 1s but leaves its query thread blocked on stdin,
            // where it swallows keystrokes until it reads a Device Status Report. Request one
            // so that thread exits before our event loop starts reading input.
            use std::io::Write as _;
            let mut out = stdout();
            let _ = out.write_all(b"\x1b[5n").and_then(|()| out.flush());
            std::thread::sleep(Duration::from_millis(200));
        }
        let mut poster_cache = HashMap::<String, Vec<u8>>::new();
        let mut poster_cache_order = VecDeque::<String>::new();
        let (resize_request_tx, resize_request_rx) = std::sync::mpsc::channel::<ResizeRequest>();
        let (resize_response_tx, resize_response_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            while let Ok(request) = resize_request_rx.recv() {
                if resize_response_tx.send(request.resize_encode()).is_err() {
                    break;
                }
            }
        });
        let mut poster_protocol = ThreadProtocol::new(resize_request_tx, None);
        let mut selected = 0usize;
        let mut selected_download = 0usize;
        let mut download_focus = false;
        let mut main_split = 50u16;
        let mut resizing_split = false;
        // Details panel height once dragged by the user (default: sized to fit).
        let mut details_height: Option<u16> = None;
        let mut resizing_details = false;
        let mut confirm_clear: Option<ClearRequest> = None;
        let mut show_help = false;
        let mut show_job_config = false;
        let mut editor = JobEditor::default();
        let mut settings: Option<SettingsScreen> = None;
        let mut file_info: Option<FileInfo> = None;
        let mut file_info_entry: Option<DownloadEntry> = None;
        // (index, job name) and (index, item id) selected when the last frame was drawn.
        let mut selection_anchor: (Option<Anchor>, Option<Anchor>) = (None, None);
        let mut file_info_task: Option<FileInfoTask> = None;
        let mut file_meta: Option<FileMeta> = None;
        // Job and item to select in the main view once Explore closes (after a download).
        let mut focus_request: Option<(String, String, Instant)> = None;
        let mut file_meta_task: Option<FileMetaTask> = None;
        let mut jellyfin_api: Option<JellyfinApi> = None;
        let mut explore: Option<ExploreState> = None;
        let mut explore_notice: Option<(String, bool)> = None;
        // None when Jellyfin is not configured; otherwise the latest ping result
        // (Some(None) while the first check is still running).
        let mut online: Option<Option<bool>> = config.jellyfin.as_ref().map(|_| None);
        let mut ping_task: Option<tokio::task::JoinHandle<bool>> = None;
        let mut pinged_at: Option<Instant> = None;
        let mut reconciled = false;
        let mut indexed: Option<usize> = None;
        let mut reconcile_task: Option<tokio::task::JoinHandle<Result<usize>>> = None;
        let mut catalog_task: Option<CatalogTask> = None;
        let mut library_cache: Option<(JellyfinApi, Vec<MediaItem>)> = None;
        let mut job_poster_catalog_task: Option<JobPosterCatalogTask> = None;
        let mut job_poster_tasks = HashMap::<String, PosterTask>::new();
        let mut job_poster_protocols = HashMap::<String, StatefulProtocol>::new();
        // Jellyfin item per job (from the poster catalog) and its detailed metadata.
        let mut job_items = HashMap::<String, MediaItem>::new();
        let mut job_details = HashMap::<String, JobDetails>::new();
        let mut job_details_tasks = HashMap::<String, JobDetailsTask>::new();
        let mut job_details_failed = HashSet::<String>::new();
        let mut probe_cache = HashMap::<PathBuf, std::result::Result<MediaProbe, String>>::new();
        let mut probe_tasks = HashMap::<PathBuf, ProbeTask>::new();
        let mut explore_poster_tasks = HashMap::<String, PosterTask>::new();
        let mut explore_poster_protocols = HashMap::<String, StatefulProtocol>::new();
        let mut explore_poster_failed = HashSet::<String>::new();
        let mut preview_episode_task: Option<PreviewEpisodeTask> = None;
        let mut poster_task: Option<PosterTask> = None;
        let mut sync_task: Option<tokio::task::JoinHandle<Result<()>>> = None;
        let mut sync_notice: Option<(String, bool)> = None;
        let mut reply_filter = ReplyFilter::default();
        // Watched state: pushed/pulled in the background while online.
        let mut watched_task: Option<tokio::task::JoinHandle<Result<usize>>> = None;
        let mut watched_synced_at: Option<Instant> = None;
        // Debugging aid for terminal quirks: log every input event to this file.
        let mut input_log = env::var_os("JELLYSYNC_INPUT_LOG")
            .and_then(|path| std::fs::File::create(path).ok());
        let mut notice_seen: Option<(String, Instant)> = None;
        let mut last_ctrl_c: Option<Instant> = None;
        let mut last_download_click: Option<(usize, Instant)> = None;
        let mut jobs_state = ListState::default();
        let mut downloads_state = ListState::default();
        let mut explore_state = ListState::default();
        let mut episode_state = ListState::default();
        let mut dashboard = DashboardSnapshot {
            timer: "unknown".into(),
            ..DashboardSnapshot::default()
        };
        let mut dashboard_task = Some(tokio::task::spawn_blocking(|| dashboard_snapshot(true)));
        let mut timer_checked_at = Instant::now();
        let mut dashboard_refreshed_at = Instant::now();
        loop {
            terminal.autoresize()?;
            if cell_checked_at.elapsed() >= CELL_CHECK_INTERVAL {
                cell_checked_at = Instant::now();
                let updated = with_viewer_cell_size(picker.clone());
                if updated.font_size() != picker.font_size() {
                    // Images are encoded for a cell size; re-render them all for the new one.
                    picker = updated;
                    for (_, task) in job_poster_tasks.drain() {
                        task.abort();
                    }
                    job_poster_protocols.clear();
                    for (_, task) in job_details_tasks.drain() {
                        task.abort();
                    }
                    job_details.clear();
                    for (_, task) in explore_poster_tasks.drain() {
                        task.abort();
                    }
                    explore_poster_protocols.clear();
                    if let Some(task) = poster_task.take() {
                        task.abort();
                    }
                    poster_protocol.empty_protocol();
                    if let Some(browser) = explore.as_mut() {
                        browser.poster_id = None;
                        browser.poster_loading = false;
                        browser.poster_loaded = false;
                    }
                    if online != Some(Some(false)) && config.jellyfin.is_some() {
                        if let Some(task) = job_poster_catalog_task.take() {
                            task.abort();
                        }
                        let config = config.clone();
                        job_poster_catalog_task = Some(tokio::spawn(async move {
                            jellyfin_job_poster_catalog(&config).await
                        }));
                    }
                }
            }
            if dashboard_task
                .as_ref()
                .is_some_and(|task| task.is_finished())
            {
                let task = dashboard_task
                    .take()
                    .expect("finished dashboard task exists");
                match task.await {
                    Ok(Ok(mut snapshot)) => {
                        if snapshot.timer.is_empty() {
                            snapshot.timer = std::mem::take(&mut dashboard.timer);
                        }
                        dashboard = snapshot;
                    }
                    Ok(Err(error)) => {
                        sync_notice = Some((format!("Could not refresh TUI status: {error:#}"), false));
                    }
                    Err(error) => {
                        sync_notice = Some((format!("Status refresh stopped: {error}"), false));
                    }
                }
                dashboard_refreshed_at = Instant::now();
            }
            if file_meta_task.as_ref().is_some_and(|task| task.is_finished()) {
                let task = file_meta_task.take().expect("finished file meta task exists");
                // Metadata is best effort; the ffprobe summary still shows without it.
                if let Ok(Ok(meta)) = task.await
                    && file_info.is_some()
                {
                    file_meta = Some(meta);
                }
            }
            if file_info_task.as_ref().is_some_and(|task| task.is_finished()) {
                let task = file_info_task.take().expect("finished file info task exists");
                let result = match task.await {
                    Ok(result) => result.map_err(|error| format!("{error:#}")),
                    Err(error) => Err(format!("probe stopped: {error}")),
                };
                if let Some((_, info)) = file_info.as_mut() {
                    *info = Some(result);
                }
            }
            if dashboard_task.is_none()
                && dashboard_refreshed_at.elapsed() >= Duration::from_millis(500)
            {
                // The DB is cheap to poll; spawning systemctl twice a second is not.
                let check_timer = timer_checked_at.elapsed() >= Duration::from_secs(10);
                if check_timer {
                    timer_checked_at = Instant::now();
                }
                dashboard_task =
                    Some(tokio::task::spawn_blocking(move || dashboard_snapshot(check_timer)));
            }
            if watched_task.as_ref().is_some_and(|task| task.is_finished()) {
                let task = watched_task.take().expect("finished watched task exists");
                match task.await {
                    Ok(Ok(pushed)) if pushed > 0 => {
                        sync_notice = Some((
                            format!("Synced {pushed} watched {} to Jellyfin", if pushed == 1 { "mark" } else { "marks" }),
                            true,
                        ));
                    }
                    Ok(Ok(_)) => {}
                    Ok(Err(error)) => {
                        sync_notice = Some((format!("Could not sync watched state: {error:#}"), false));
                    }
                    Err(error) => {
                        sync_notice = Some((format!("Watched sync stopped: {error}"), false));
                    }
                }
                watched_synced_at = Some(Instant::now());
            }
            if watched_task.is_none()
                && online == Some(Some(true))
                && config.download.mode == "jellyfin"
                && watched_synced_at.is_none_or(|at| at.elapsed() >= Duration::from_secs(60))
            {
                let config = config.clone();
                watched_task = Some(tokio::spawn(async move { sync_watched(&config).await }));
            }
            if ping_task.as_ref().is_some_and(|task| task.is_finished()) {
                let task = ping_task.take().expect("finished ping task exists");
                let reachable = task.await.unwrap_or(false);
                let was_online = online == Some(Some(true));
                let was_offline = online == Some(Some(false));
                online = Some(Some(reachable));
                pinged_at = Some(Instant::now());
                if reachable && !was_online {
                    // Retry what failed while the connection was flaky, and push
                    // watched marks made offline right away.
                    watched_synced_at = None;
                    job_details_failed.clear();
                    explore_poster_failed.clear();
                    if sync_notice.as_ref().is_some_and(|(message, _)| message.starts_with("Offline:")) {
                        sync_notice = None;
                    }
                    if let Err(error) = resume_library_workers(&config_path) {
                        sync_notice =
                            Some((format!("Could not resume ad-hoc downloads: {error:#}"), false));
                    }
                    if !reconciled && reconcile_task.is_none() {
                        let config = config.clone();
                        reconcile_task =
                            Some(tokio::spawn(async move { reconcile_existing(&config).await }));
                    }
                    if job_poster_catalog_task.is_none() && job_poster_protocols.is_empty() {
                        let config = config.clone();
                        job_poster_catalog_task = Some(tokio::spawn(async move {
                            jellyfin_job_poster_catalog(&config).await
                        }));
                    }
                } else if !reachable && !was_offline {
                    sync_notice = Some((
                        "Offline: Jellyfin is unreachable; local files, playback and clearing still work".into(),
                        false,
                    ));
                }
            }
            if let (Some(jellyfin), None) = (config.jellyfin.as_ref(), ping_task.as_ref()) {
                let interval = if online == Some(Some(true)) { 60 } else { 10 };
                if pinged_at.is_none_or(|at| at.elapsed() >= Duration::from_secs(interval)) {
                    let jellyfin = jellyfin.clone();
                    ping_task = Some(tokio::spawn(async move { jellyfin_reachable(&jellyfin).await }));
                }
            } else if online.is_none() && !reconciled && reconcile_task.is_none() {
                let config = config.clone();
                reconcile_task = Some(tokio::spawn(async move { reconcile_existing(&config).await }));
            }
            if reconcile_task
                .as_ref()
                .is_some_and(|task| task.is_finished())
            {
                let task = reconcile_task.take().expect("finished reconcile task exists");
                let result = task.await;
                reconciled = matches!(result, Ok(Ok(_)));
                // Success is shown in the header; only failures need the footer.
                match result {
                    Ok(Ok(count)) => indexed = Some(count),
                    Ok(Err(error)) => {
                        sync_notice = Some((format!("Could not index existing files: {error:#}"), false));
                    }
                    Err(error) => {
                        sync_notice = Some((format!("Indexing stopped: {error}"), false));
                    }
                }
            }
            if preview_episode_task
                .as_ref()
                .is_some_and(|task| task.is_finished())
            {
                let task = preview_episode_task
                    .take()
                    .expect("finished preview episode task exists");
                match task.await {
                    Ok((item_id, Ok(episodes))) => {
                        if let Some(browser) = explore.as_mut()
                            && browser.preview_item_id.as_deref() == Some(item_id.as_str())
                        {
                            browser.episodes = episodes;
                            browser.preview_loading = false;
                        }
                    }
                    Ok((item_id, Err(error))) => {
                        if let Some(browser) = explore.as_mut()
                            && browser.preview_item_id.as_deref() == Some(item_id.as_str())
                        {
                            browser.preview_loading = false;
                            explore_notice = Some((
                                format!("Could not load show contents: {error:#}"),
                                false,
                            ));
                        }
                    }
                    Err(error) if !error.is_cancelled() => {
                        explore_notice = Some((
                            format!("Show contents request stopped: {error}"),
                            false,
                        ));
                    }
                    Err(_) => {}
                }
            }
            while let Ok(result) = resize_response_rx.try_recv() {
                match result {
                    Ok(response) => {
                        poster_protocol.update_resized_protocol(response);
                    }
                    Err(error) => {
                        explore_notice = Some((format!("Poster rendering failed: {error}"), false));
                    }
                }
            }
            if catalog_task.as_ref().is_some_and(|task| task.is_finished()) {
                let task = catalog_task.take().expect("finished catalog task exists");
                match task.await {
                    Ok(Ok((api, items))) => {
                        library_cache = Some((api.clone(), items.clone()));
                        if let Some(browser) = explore.as_mut() {
                            browser.api = Some(api);
                            browser.items = items;
                            browser.catalog_loading = false;
                            if let Some(item) = browser.items.first().cloned()
                                && let Err(error) = request_poster(
                                    browser,
                                    item,
                                    &picker,
                                    &poster_cache,
                                    &mut poster_protocol,
                                    &mut poster_task,
                                )
                            {
                                browser.poster_loading = false;
                                explore_notice = Some((format!("Could not load poster: {error:#}"), false));
                            }
                        }
                    }
                    Ok(Err(error)) => {
                        explore = None;
                        sync_notice = Some((format!("Could not open Explore: {error:#}"), false));
                    }
                    Err(error) => {
                        explore = None;
                        explore_notice = Some((format!("Explore request stopped: {error}"), false));
                    }
                }
            }
            if job_poster_catalog_task
                .as_ref()
                .is_some_and(|task| task.is_finished())
            {
                let task = job_poster_catalog_task
                    .take()
                    .expect("finished job poster catalog task exists");
                if let Ok(Ok((api, posters))) = task.await {
                    jellyfin_api = Some(api.clone());
                    job_details_failed.clear();
                    for (job_name, item) in posters {
                        job_items.insert(job_name.clone(), item.clone());
                        if job_poster_protocols.contains_key(&job_name)
                            || job_poster_tasks.contains_key(&job_name)
                        {
                            continue;
                        }
                        let item_id = item.id.clone();
                        let cached_data = poster_cache.get(&item_id).cloned();
                        let picker = picker.clone();
                        let api = api.clone();
                        job_poster_tasks.insert(
                            job_name,
                            tokio::spawn(async move {
                                let result =
                                    fetch_poster_protocol(api, item, picker, cached_data).await;
                                (item_id, result)
                            }),
                        );
                    }
                }
            }
            let finished_job_posters: Vec<_> = job_poster_tasks
                .iter()
                .filter_map(|(job_name, task)| task.is_finished().then_some(job_name.clone()))
                .collect();
            for job_name in finished_job_posters {
                let task = job_poster_tasks
                    .remove(&job_name)
                    .expect("finished job poster task exists");
                if let Ok((item_id, Ok(Some((data, protocol))))) = task.await {
                    poster_cache.insert(item_id.clone(), data);
                    poster_cache_order.retain(|id| id != &item_id);
                    poster_cache_order.push_back(item_id.clone());
                    while poster_cache_order.len() > 32 {
                        if let Some(expired) = poster_cache_order.pop_front() {
                            poster_cache.remove(&expired);
                        }
                    }
                    job_poster_protocols.insert(job_name, protocol);
                }
            }
            let finished_details: Vec<_> = job_details_tasks
                .iter()
                .filter_map(|(job_name, task)| task.is_finished().then_some(job_name.clone()))
                .collect();
            for job_name in finished_details {
                let task = job_details_tasks
                    .remove(&job_name)
                    .expect("finished job details task exists");
                match task.await {
                    Ok(Ok(details)) => {
                        job_details.insert(job_name, details);
                    }
                    // Do not retry every frame; a new catalog or reconnect resets this.
                    _ => {
                        job_details_failed.insert(job_name);
                    }
                }
            }
            let finished_probes: Vec<_> = probe_tasks
                .iter()
                .filter_map(|(path, task)| task.is_finished().then_some(path.clone()))
                .collect();
            for path in finished_probes {
                let task = probe_tasks.remove(&path).expect("finished probe task exists");
                let result = match task.await {
                    Ok(result) => result.map_err(|error| format!("{error:#}")),
                    Err(error) => Err(format!("probe stopped: {error}")),
                };
                probe_cache.insert(path, result);
            }
            let finished_explore_posters: Vec<_> = explore_poster_tasks
                .iter()
                .filter_map(|(item_id, task)| task.is_finished().then_some(item_id.clone()))
                .collect();
            for item_id in finished_explore_posters {
                let task = explore_poster_tasks
                    .remove(&item_id)
                    .expect("finished Explore poster task exists");
                match task.await {
                    Ok((item_id, Ok(Some((data, protocol))))) => {
                        poster_cache.insert(item_id.clone(), data);
                        poster_cache_order.retain(|id| id != &item_id);
                        poster_cache_order.push_back(item_id.clone());
                        while poster_cache_order.len() > 32 {
                            if let Some(expired) = poster_cache_order.pop_front() {
                                poster_cache.remove(&expired);
                            }
                        }
                        explore_poster_protocols.insert(item_id, protocol);
                    }
                    // No image or a failed request: do not ask again every frame.
                    _ => {
                        explore_poster_failed.insert(item_id);
                    }
                }
            }
            if poster_task.as_ref().is_some_and(|task| task.is_finished()) {
                let task = poster_task.take().expect("finished poster task exists");
                match task.await {
                    Ok((item_id, result)) => {
                        if let Some(browser) = explore.as_mut()
                            && browser.poster_id.as_deref() == Some(item_id.as_str())
                        {
                            browser.poster_loading = false;
                            match result {
                                Ok(Some((data, protocol))) => {
                                    poster_cache.insert(item_id.clone(), data);
                                    poster_cache_order.retain(|id| id != &item_id);
                                    poster_cache_order.push_back(item_id.clone());
                                    while poster_cache_order.len() > 32 {
                                        if let Some(expired) = poster_cache_order.pop_front() {
                                            poster_cache.remove(&expired);
                                        }
                                    }
                                    poster_protocol.replace_protocol(protocol);
                                    browser.poster_loaded = true;
                                }
                                Ok(None) => {
                                    poster_protocol.empty_protocol();
                                    browser.poster_loaded = false;
                                    browser.poster_id = None;
                                }
                                Err(error) => {
                                    poster_protocol.empty_protocol();
                                    browser.poster_loaded = false;
                                    browser.poster_id = None;
                                    explore_notice = Some((format!("Poster unavailable: {error:#}"), false));
                                }
                            }
                        }
                    }
                    Err(error) if !error.is_cancelled() => {
                        if let Some(browser) = explore.as_mut() {
                            browser.poster_id = None;
                            browser.poster_loading = false;
                        }
                        explore_notice = Some((format!("Poster request stopped: {error}"), false));
                    }
                    Err(_) => {}
                }
            }
            if let Some(browser) = explore.as_ref()
                && !browser.catalog_loading
                && let Some(api) = browser.api.clone()
            {
                let mut items: Vec<_> = browser
                    .filtered_items()
                    .into_iter()
                    .take(40)
                    .cloned()
                    .collect();
                if let Some(item) = browser.filtered_items().get(browser.selected)
                    && !items.iter().any(|candidate| candidate.id == item.id)
                {
                    items.push((*item).clone());
                }
                for item in items {
                    if poster_cache.contains_key(&item.id)
                        || explore_poster_failed.contains(&item.id)
                        || explore_poster_protocols.contains_key(&item.id)
                        || explore_poster_tasks.contains_key(&item.id)
                        || browser.poster_id.as_deref() == Some(item.id.as_str())
                    {
                        continue;
                    }
                    let item_id = item.id.clone();
                    let picker = picker.clone();
                    let cached_data = poster_cache.get(&item_id).cloned();
                    let api = api.clone();
                    explore_poster_tasks.insert(
                        item_id.clone(),
                        tokio::spawn(async move {
                            let result = fetch_poster_protocol(api, item, picker, cached_data).await;
                            (item_id, result)
                        }),
                    );
                }
            }
            let preview_item = explore.as_ref().and_then(|browser| {
                if browser.catalog_loading {
                    return None;
                }
                browser
                    .filtered_items()
                    .get(browser.selected)
                    .cloned()
                    .cloned()
                    .zip(browser.api.clone())
            });
            if let Some((item, api)) = preview_item {
                let should_refresh = explore
                    .as_ref()
                    .is_some_and(|browser| browser.preview_item_id.as_deref() != Some(item.id.as_str()));
                if should_refresh {
                    if let Some(task) = preview_episode_task.take() {
                        task.abort();
                    }
                    if let Some(browser) = explore.as_mut() {
                        browser.preview_item_id = Some(item.id.clone());
                        browser.episodes.clear();
                        browser.episode_cursor = 0;
                        browser.selected_episodes.clear();
                        browser.preview_loading = item.item_type.as_deref() == Some("Series");
                    }
                    if item.item_type.as_deref() == Some("Series") {
                        let item_id = item.id.clone();
                        preview_episode_task = Some(tokio::spawn(async move {
                            let result = fetch_episodes(api, item).await;
                            (item_id, result)
                        }));
                    }
                }
            }
            if sync_task.as_ref().is_some_and(|task| task.is_finished()) {
                let task = sync_task.take().expect("finished sync task exists");
                sync_notice = Some(match task.await {
                    Ok(Ok(())) => ("Sync finished successfully".into(), true),
                    Ok(Err(error)) => (format!("Sync failed: {error:#}"), false),
                    Err(error) => (format!("Sync task stopped: {error}"), false),
                });
            }
            let history = &dashboard.history;
            let mut jobs: Vec<TuiJob> = config
                .jobs
                .iter()
                .map(|job| TuiJob {
                    name: job.name.clone(),
                    adhoc: false,
                })
                .collect();
            for name in history
                .keys()
                .filter(|name| name.starts_with("library:"))
            {
                if !jobs.iter().any(|job| job.name == *name) {
                    jobs.push(TuiJob {
                        name: name.clone(),
                        adhoc: true,
                    });
                }
            }
            for entry in &dashboard.downloads {
                if entry.job.starts_with("library:") && !jobs.iter().any(|job| job.name == entry.job)
                {
                    jobs.push(TuiJob {
                        name: entry.job.clone(),
                        adhoc: true,
                    });
                }
            }
            // Follow the selected job when jobs are added or removed around it, unless
            // input moved the selection since the last frame.
            if let (Some((index, name)), _) = &selection_anchor
                && *index == selected
                && let Some(position) = jobs.iter().position(|job| &job.name == name)
            {
                selected = position;
            }
            if selected >= jobs.len() && !jobs.is_empty() {
                selected = jobs.len() - 1;
            }
            // The new job shows up once the dashboard refreshes; give up after a while.
            if focus_request
                .as_ref()
                .is_some_and(|(_, _, at)| at.elapsed() > Duration::from_secs(10))
                && explore.is_none()
            {
                focus_request = None;
            }
            let mut focus_item = None;
            if explore.is_none()
                && let Some((job, item_id, _)) = &focus_request
                && let Some(index) = jobs.iter().position(|entry| &entry.name == job)
                && dashboard.downloads.iter().any(|entry| &entry.item_id == item_id)
            {
                selected = index;
                focus_item = Some(item_id.clone());
                focus_request = None;
            }
            let selected_job = jobs.get(selected);
            let mut downloads: Vec<_> = dashboard
                .downloads
                .iter()
                .filter(|entry| selected_job.is_some_and(|job| entry.job == job.name))
                .cloned()
                .collect();
            let selected_details = selected_job.and_then(|job| job_details.get(&job.name));
            let rows = file_rows(&mut downloads, selected_details);
            // Likewise keep the highlighted file when the list refreshes or re-sorts.
            if let (_, Some((index, item_id))) = &selection_anchor
                && *index == selected_download
                && let Some(position) = downloads.iter().position(|entry| &entry.item_id == item_id)
            {
                selected_download = position;
            }
            if let Some(job) = selected_job
                && online != Some(Some(false))
                && !job_details.contains_key(&job.name)
                && !job_details_tasks.contains_key(&job.name)
                && !job_details_failed.contains(&job.name)
                && let (Some(api), Some(item)) = (jellyfin_api.clone(), job_items.get(&job.name))
            {
                job_details_tasks.insert(
                    job.name.clone(),
                    tokio::spawn(jellyfin_job_details(
                        api,
                        item.clone(),
                        picker.clone(),
                        poster_cache.get(&item.id).cloned(),
                    )),
                );
            }
            if let Some(item_id) = focus_item
                && let Some(index) = downloads.iter().position(|entry| entry.item_id == item_id)
            {
                selected_download = index;
                download_focus = true;
            }
            if downloads.is_empty() {
                selected_download = 0;
            } else if selected_download >= downloads.len() {
                selected_download = downloads.len() - 1;
            }
            // Probe finished files for the list badges, the selected one first.
            for entry in downloads.get(selected_download).into_iter().chain(&downloads) {
                if !download_complete(&entry.status) {
                    // A file being (re)downloaded must be probed again afterwards.
                    probe_cache.remove(&entry.path);
                    continue;
                }
                if probe_tasks.len() >= PROBE_CONCURRENCY {
                    break;
                }
                if !probe_cache.contains_key(&entry.path) && !probe_tasks.contains_key(&entry.path) {
                    probe_tasks.insert(entry.path.clone(), tokio::spawn(probe_media(entry.path.clone())));
                }
            }
            let entries: Vec<ListItem> = jobs
                .iter()
                .map(|job| {
                    let (state, message) = history
                        .get(&job.name)
                        .map(|(state, message, _)| (state.as_str(), message.as_str()))
                        .unwrap_or(("not run", "Waiting for first sync"));
                    let (state, summary) = job_display_state(
                        state,
                        dashboard
                            .downloads
                            .iter()
                            .filter(|entry| entry.job == job.name)
                            .map(|entry| entry.status.as_str()),
                    );
                    let message = summary.as_deref().unwrap_or(message);
                    let disabled = config
                        .jobs
                        .iter()
                        .any(|configured| configured.name == job.name && !configured.enabled());
                    let (kind, kind_style) = if job.adhoc {
                        (format!("{} LIBRARY", icon::LIBRARY), Style::default().fg(Color::Magenta))
                    } else if disabled {
                        ("DISABLED".to_string(), Style::default().fg(Color::Yellow))
                    } else {
                        // Regular jobs need no label; only ad-hoc and disabled ones stand out.
                        (String::new(), Style::default())
                    };
                    let title_style = if job.adhoc {
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD)
                    };
                    let job_files: Vec<&DownloadEntry> = dashboard
                        .downloads
                        .iter()
                        .filter(|entry| entry.job == job.name && entry.status != "ignored")
                        .collect();
                    let total_size = job_files.iter().fold(0u64, |total, entry| {
                        total.saturating_add(entry.total.unwrap_or(entry.bytes))
                    });
                    let job_rate: u64 = job_files.iter().filter_map(|entry| entry.rate).sum();
                    let seen = job_files
                        .iter()
                        .filter(|entry| {
                            dashboard.watched.get(&entry.item_id).is_some_and(|state| state.played)
                        })
                        .count();
                    ListItem::new(vec![
                        Line::from(vec![
                            Span::raw(POSTER_INDENT),
                            {
                                let (marker, marker_style) = transfer_marker(state);
                                Span::styled(format!("{marker} "), marker_style)
                            },
                            Span::styled(job.name.clone(), title_style),
                            Span::styled(
                                if kind.is_empty() { String::new() } else { format!("  {kind}") },
                                kind_style,
                            ),
                        ]),
                        Line::from(vec![
                            Span::raw(POSTER_INDENT),
                            Span::styled(
                                format!("  {}", format_bytes(total_size)),
                                Style::default().fg(Color::Gray),
                            ),
                            Span::styled(
                                if job_rate > 0 {
                                    format!("  {} {}/s", icon::DOWNLOAD, format_bytes(job_rate))
                                } else {
                                    String::new()
                                },
                                Style::default().fg(Color::Cyan),
                            ),
                            Span::styled(
                                if seen > 0 {
                                    format!("  {seen}/{} watched", job_files.len())
                                } else {
                                    String::new()
                                },
                                Style::default().fg(Color::DarkGray),
                            ),
                        ]),
                        Line::from(vec![
                            Span::raw(POSTER_INDENT),
                            Span::styled(
                                format!("  {}", truncate(message, 64)),
                                message_style(state, message),
                            ),
                        ]),
                        Line::from(Span::raw(POSTER_INDENT)),
                    ])
                })
                .collect();
            let size = terminal.size()?;
            let area = Rect::new(0, 0, size.width, size.height);
            let explore_popup = centered_rect(100, 96, area);
            let [explore_list_area, explore_poster_area] = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
                .areas(Rect {
                    x: explore_popup.x + 1,
                    y: explore_popup.y + 2,
                    width: explore_popup.width.saturating_sub(2),
                    height: explore_popup.height.saturating_sub(4),
                });
            let explore_contents_area = Rect {
                x: explore_poster_area.x + 1,
                y: explore_poster_area.y + 5,
                width: explore_poster_area.width.saturating_sub(2),
                height: explore_poster_area.height.saturating_sub(7),
            };
            let [header, body, footer] = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(1),
                    Constraint::Min(4),
                    Constraint::Length(2),
                ])
                .areas(area);
            let (jobs_area, downloads_area) = if area.width >= 100 {
                let [jobs_area, downloads_area] = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([
                        Constraint::Percentage(main_split),
                        Constraint::Percentage(100 - main_split),
                    ])
                    .areas(body);
                (jobs_area, downloads_area)
            } else {
                let [jobs_area, downloads_area] = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Percentage(35), Constraint::Percentage(65)])
                    .areas(body);
                (jobs_area, downloads_area)
            };
            // Metadata for the selected job sits above its files when there is room.
            let (details_area, downloads_area) = if selected_job.is_some() && downloads_area.height >= 22 {
                let height = details_height
                    .unwrap_or_else(|| (downloads_area.height * 2 / 5).clamp(10, 16))
                    .clamp(6, downloads_area.height.saturating_sub(6));
                let [details_area, downloads_area] = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Length(height), Constraint::Min(6)])
                    .areas(downloads_area);
                (Some(details_area), downloads_area)
            } else {
                (None, downloads_area)
            };
            // Borders, the "▌ " highlight symbol and the "● " bullet.
            let download_text_width = usize::from(downloads_area.width.saturating_sub(6)).max(8);
            // Leading indent plus the " 100%" suffix.
            let download_bar_width = download_text_width.saturating_sub(7).clamp(8, 60);
            let watched = &dashboard.watched;
            let now = unix_now();
            let is_played =
                |entry: &DownloadEntry| watched.get(&entry.item_id).is_some_and(|state| state.played);
            let job_grace = selected_job
                .and_then(|job| config.jobs.iter().find(|configured| configured.name == job.name))
                .and_then(|job| job.watched_grace(&config));
            let season_stats = |row: usize| {
                // Files and size from this heading up to the next one.
                let files: Vec<&DownloadEntry> = rows[row + 1..]
                    .iter()
                    .map_while(|row| match row {
                        FileRow::File(index) => downloads.get(*index),
                        FileRow::Season(_) => None,
                    })
                    .collect();
                // Ignored files are not part of the season's downloads anymore.
                let files: Vec<&DownloadEntry> =
                    files.into_iter().filter(|entry| entry.status != "ignored").collect();
                let done = files.iter().filter(|entry| download_complete(&entry.status)).count();
                let seen = files.iter().filter(|entry| is_played(entry)).count();
                let size = files
                    .iter()
                    .fold(0u64, |total, entry| total.saturating_add(entry.total.unwrap_or(entry.bytes)));
                (files.len(), done, seen, size)
            };
            let download_rows: Vec<ListItem> = rows
                .iter()
                .enumerate()
                .map(|(row, file_row)| {
                    let entry = match file_row {
                        FileRow::Season(label) => {
                            let (count, done, seen, size) = season_stats(row);
                            let mut stats = format!(
                                "{count} {}",
                                if count == 1 { "file" } else { "files" }
                            );
                            if done < count {
                                stats.push_str(&format!(", {done} done"));
                            }
                            if seen > 0 {
                                stats.push_str(&format!(", {seen} watched"));
                            }
                            stats.push_str(&format!(" · {}", format_bytes(size)));
                            let used = label.chars().count() + stats.chars().count() + 6;
                            return ListItem::new(Line::from(vec![
                                Span::styled(
                                    format!("{label} "),
                                    Style::default()
                                        .fg(Color::Magenta)
                                        .add_modifier(Modifier::BOLD),
                                ),
                                Span::styled(
                                    "─".repeat(download_text_width.saturating_sub(used).max(2)),
                                    Style::default().fg(Color::DarkGray),
                                ),
                                Span::styled(format!(" {stats}"), Style::default().fg(Color::Gray)),
                            ]));
                        }
                        FileRow::File(index) => &downloads[*index],
                    };
                    let filename = entry.path.file_name().unwrap_or_default().to_string_lossy();
                    let episode = file_title(selected_details, entry, downloads.len() == 1);
                    let (filled, track, percent) =
                        download_progress(entry.bytes, entry.total, download_bar_width);
                    let completed = download_complete(&entry.status);
                    let size = if completed {
                        format_bytes(entry.bytes)
                    } else if entry.status == "queued" && entry.bytes == 0 {
                        entry.total.map(format_bytes).unwrap_or_default()
                    } else {
                        entry
                            .total
                            .map(|total| {
                                format!("{} / {}", format_bytes(entry.bytes), format_bytes(total))
                            })
                            .unwrap_or_else(|| format!("{} downloaded", format_bytes(entry.bytes)))
                    };
                    let ignored = entry.status == "ignored";
                    let mut subtitle = if ignored {
                        vec![
                            Span::styled(
                                if entry.path.exists() { "  file kept · I to sync again" } else { "  I to sync again" },
                                Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
                            ),
                        ]
                    } else if completed {
                        vec![
                            Span::styled(format!("  {size}"), Style::default().fg(Color::Gray)),
                        ]
                    } else {
                        let mut spans = vec![
                            Span::styled(format!("  {size}"), Style::default().fg(Color::Gray)),
                        ];
                        if let Some(rate) = entry.rate {
                            spans.push(Span::styled(
                                format!("  {}", format_throughput(rate, entry.bytes, entry.total)),
                                Style::default().fg(Color::Cyan),
                            ));
                        }
                        spans
                    };
                    if completed && let Some(Ok(probe)) = probe_cache.get(&entry.path) {
                        let used: usize = subtitle.iter().map(|span| span.content.chars().count()).sum();
                        // Resolution, then the codec in its own colour.
                        let mut room = download_text_width.saturating_sub(used);
                        for (index, badge) in probe.badges.iter().enumerate() {
                            let text = format!(" {}", truncate(badge, room.saturating_sub(1)));
                            room = room.saturating_sub(text.chars().count());
                            let color = if index == 0 { Color::Blue } else { codec_color(badge) };
                            subtitle.push(Span::styled(text, Style::default().fg(color)));
                            if room <= 1 {
                                break;
                            }
                        }
                    }
                    // Queued files have no progress yet: skip the bar and its "?".
                    let third_line = if !completed && !ignored && entry.status != "queued" {
                        Line::from(vec![
                            Span::styled("  ", Style::default()),
                            Span::styled(filled, Style::default().fg(Color::Cyan)),
                            Span::styled(track, Style::default().fg(Color::DarkGray)),
                            Span::styled(format!(" {percent}"), Style::default().fg(Color::Gray)),
                        ])
                    } else if episode.is_some() {
                        // The title replaced the file name on the first line.
                        Line::from(Span::styled(
                            format!("  {}", truncate_near_end(&filename, download_text_width.saturating_sub(2))),
                            Style::default().fg(Color::DarkGray),
                        ))
                    } else {
                        Line::from("")
                    };
                    let title_line = match episode {
                        Some((tag, name)) => {
                            let tag = if tag.is_empty() { tag } else { format!("{tag}  ") };
                            vec![
                                Span::styled(tag.clone(), Style::default().fg(Color::Cyan)),
                                Span::styled(
                                    truncate(&name, download_text_width.saturating_sub(tag.chars().count())),
                                    Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
                                ),
                            ]
                        }
                        None => vec![Span::styled(
                            truncate_near_end(&filename, download_text_width),
                            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
                        )],
                    };
                    let (marker, marker_style) = transfer_marker(&entry.status);
                    let mut first = vec![Span::styled(format!("{marker} "), marker_style)];
                    if ignored {
                        // Dim the whole title: the file is gone and stays gone.
                        first.extend(title_line.into_iter().map(|span| {
                            Span::styled(span.content, Style::default().fg(Color::DarkGray))
                        }));
                    } else {
                        first.extend(title_line);
                        if let Some((label, soon)) =
                            watched_label(watched.get(&entry.item_id), job_grace, now)
                        {
                            let used: usize = first.iter().map(|span| span.content.chars().count()).sum();
                            first.push(Span::styled(
                                format!("  {}", truncate(&label, download_text_width.saturating_sub(used))),
                                Style::default().fg(if soon { Color::Yellow } else { Color::DarkGray }),
                            ));
                        }
                    }
                    ListItem::new(vec![Line::from(first), Line::from(subtitle), third_line])
                })
                .collect();
            let selected_row = rows
                .iter()
                .position(|row| matches!(row, FileRow::File(index) if *index == selected_download));
            // The layout above was computed from an earlier size query; if the terminal
            // was resized since (tmux switching clients does this), drawing it would
            // index outside the resized buffer. Skip the frame and lay out again.
            let mut stale_layout = false;
            terminal.draw(|frame| {
                if frame.area() != area {
                    stale_layout = true;
                    return;
                }
                // Several panels use fixed offsets; below this size they would not fit.
                if area.width < 40 || area.height < 12 {
                    frame.render_widget(
                        Paragraph::new(vec![
                            Line::from(Span::styled(
                                "◆ jellysync",
                                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                            )),
                            Line::from(Span::styled(
                                "terminal too small (min 40x12)",
                                Style::default().fg(Color::Gray),
                            )),
                        ])
                        .alignment(ratatui::layout::Alignment::Center)
                        .wrap(Wrap { trim: true }),
                        Rect {
                            x: 0,
                            y: area.height.saturating_sub(2) / 2,
                            width: area.width,
                            height: area.height.min(2),
                        },
                    );
                    return;
                }
                // One combined state: offline wins, then a running sync, then
                // the reachability check; otherwise the server is up and idle.
                let (connection, connection_state) = match online {
                    Some(Some(false)) => (format!("{} OFFLINE", icon::OFFLINE), "failed"),
                    _ if sync_task.is_some() => (format!("{} SYNCING", icon::SYNC), "running"),
                    Some(None) => ("CHECKING".to_string(), "stopped"),
                    Some(Some(true)) => (format!("{} ONLINE", icon::ONLINE), "success"),
                    // No reachability check (rsync mode): just idle.
                    None => (format!("{} READY", icon::CHECK), "success"),
                };
                let mut header_segments: Vec<Vec<Span>> = vec![
                    vec![Span::styled(
                        "  ◆ JELLYSYNC",
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    )],
                    vec![Span::styled(
                        connection,
                        state_style(connection_state).add_modifier(Modifier::BOLD),
                    )],
                ];
                if reconcile_task.is_some() {
                    header_segments.push(vec![Span::styled(
                        "indexing…",
                        Style::default().fg(Color::Gray),
                    )]);
                } else if let Some(count) = indexed.filter(|count| *count > 0) {
                    header_segments.push(vec![Span::styled(
                        format!("{count} indexed"),
                        Style::default().fg(Color::Gray),
                    )]);
                }
                // What tracked files (and partial downloads) use on disk now.
                let disk_size = dashboard
                    .downloads
                    .iter()
                    .filter(|entry| entry.status != "ignored")
                    .fold(0u64, |total, entry| total.saturating_add(entry.bytes));
                if disk_size > 0 {
                    header_segments.push(vec![Span::styled(
                        format!("{} {}", icon::DISK, format_bytes(disk_size)),
                        Style::default().fg(Color::Gray),
                    )]);
                }
                let total_rate: u64 = dashboard.downloads.iter().filter_map(|entry| entry.rate).sum();
                if total_rate > 0 {
                    header_segments.push(vec![Span::styled(
                        format!("{} {}/s", downloading_icon(), format_bytes(total_rate)),
                        Style::default().fg(Color::Cyan),
                    )]);
                }
                // The timer only deserves attention when it is not running.
                if !dashboard.timer.is_empty() && dashboard.timer != "active" {
                    header_segments.push(vec![Span::styled(
                        format!("{} timer {}", icon::TIMER_OFF, dashboard.timer),
                        Style::default().fg(Color::Yellow),
                    )]);
                }
                // Segments are separated by whitespace only.
                let mut header_spans = Vec::new();
                for (index, segment) in header_segments.into_iter().enumerate() {
                    if index > 0 {
                        header_spans.push(Span::raw("   "));
                    }
                    header_spans.extend(segment);
                }
                let header_text = Line::from(header_spans);
                frame.render_widget(Paragraph::new(header_text), header);

                let jobs_title = format!("Jobs · {}", jobs.len());
                let seasons = rows.iter().filter(|row| matches!(row, FileRow::Season(_))).count();
                let ignored_count = downloads.iter().filter(|entry| entry.status == "ignored").count();
                let file_count = downloads.len() - ignored_count;
                let files_size = downloads
                    .iter()
                    .filter(|entry| entry.status != "ignored")
                    .fold(0u64, |total, entry| total.saturating_add(entry.total.unwrap_or(entry.bytes)));
                let downloads_title = match selected_job {
                    None => "Files · no job selected".to_string(),
                    Some(job) => {
                        let mut title = format!(
                            "Files: {} · {} {}",
                            job.name.trim_start_matches("library:"),
                            file_count,
                            if file_count == 1 { "file" } else { "files" }
                        );
                        if seasons > 1 {
                            title.push_str(&format!(" · {seasons} seasons"));
                        }
                        if file_count > 0 {
                            title.push_str(&format!(" · {}", format_bytes(files_size)));
                        }
                        if ignored_count > 0 {
                            title.push_str(&format!(" · {ignored_count} ignored"));
                        }
                        title
                    }
                };
                let jobs_block = panel_block(&jobs_title, !download_focus);
                let downloads_block = panel_block(&downloads_title, download_focus);
                if !jobs.is_empty() {
                    jobs_state.select(Some(selected));
                }
                downloads_state.select(selected_row);
                // Keep a file's season heading visible when scrolling up to it.
                if let Some(row) = selected_row
                    && row > 0
                    && downloads_state.offset() == row
                    && matches!(rows[row - 1], FileRow::Season(_))
                {
                    *downloads_state.offset_mut() = row - 1;
                }
                let list_style = Style::default()
                    .fg(Color::White)
                    .bg(Color::Rgb(38, 44, 66))
                    .add_modifier(Modifier::BOLD);
                // The unfocused list keeps its selection visible, but only as a
                // faint gray band so the focused list stays obvious.
                let unfocused_style = Style::default().bg(Color::Rgb(46, 50, 58));
                let job_list = List::new(entries)
                    .block(jobs_block)
                    .highlight_style(if !download_focus {
                        list_style
                    } else {
                        unfocused_style
                    })
                    .highlight_symbol("▌ ");
                let download_list = List::new(download_rows)
                    .block(downloads_block)
                    .highlight_style(if download_focus {
                        list_style
                    } else {
                        unfocused_style
                    })
                    .highlight_symbol("▌ ");
                frame.render_stateful_widget(job_list, jobs_area, &mut jobs_state);
                if jobs_area.width >= 20 {
                    let offset = jobs_state.offset();
                    for (visible_index, job) in jobs.iter().skip(offset).enumerate() {
                        let y = jobs_area.y + 1 + (visible_index as u16 * 4);
                        if y + 4 > jobs_area.y + jobs_area.height.saturating_sub(1) {
                            break;
                        }
                        if let Some(protocol) = job_poster_protocols.get_mut(&job.name) {
                            frame.render_stateful_widget(
                                StatefulImage::default()
                                    .resize(Resize::Fit(Some(FilterType::Nearest))),
                                Rect {
                                    x: jobs_area.x + 3,
                                    y,
                                    width: POSTER_WIDTH,
                                    height: 4,
                                },
                                protocol,
                            );
                        }
                    }
                }
                if let (Some(details_area), Some(job)) = (details_area, selected_job) {
                    let selected_entry = downloads.get(selected_download);
                    let file_label = selected_entry.map(|entry| {
                        match file_title(job_details.get(&job.name), entry, downloads.len() == 1) {
                            Some((tag, name)) if !tag.is_empty() => format!("{tag} · {name}"),
                            // A movie's title is already the heading; name the file instead.
                            _ => entry.path.file_name().unwrap_or_default().to_string_lossy().into_owned(),
                        }
                    });
                    let probe = selected_entry.and_then(|entry| {
                        if download_complete(&entry.status) {
                            probe_cache.get(&entry.path)
                        } else {
                            None
                        }
                    });
                    let still_downloading = selected_entry.is_some_and(|entry| !download_complete(&entry.status));
                    let loading = job_details_tasks.contains_key(&job.name)
                        || job_poster_catalog_task.is_some();
                    // The DB stores UTC; show how long ago instead.
                    let last_sync = history.get(&job.name).and_then(|(state, _, time)| {
                        let age = status_age(time)?;
                        Some((state.clone(), age))
                    });
                    let file_watched = selected_entry
                        .filter(|entry| download_complete(&entry.status))
                        .map(|entry| {
                            watched_label(dashboard.watched.get(&entry.item_id), job_grace, now)
                                .map(|(label, _)| {
                                    label
                                        .trim_start_matches(icon::WATCHED)
                                        .trim_start()
                                        .replacen("watched", "yes", 1)
                                })
                                .unwrap_or_else(|| "no · w marks it watched".into())
                        });
                    render_job_details(
                        frame,
                        details_area,
                        &job.name,
                        job_details.get_mut(&job.name),
                        loading,
                        last_sync,
                        file_label.filter(|_| !still_downloading),
                        file_watched,
                        probe,
                    );
                }
                frame.render_stateful_widget(download_list, downloads_area, &mut downloads_state);
                let visible_lines = usize::from(downloads_area.height.saturating_sub(2));
                let total_lines: usize = rows.iter().map(file_row_height).sum();
                if total_lines > visible_lines {
                    // Measured in lines: rows differ in height (headings are one line).
                    let scrolled: usize = rows
                        .iter()
                        .take(downloads_state.offset())
                        .map(file_row_height)
                        .sum();
                    let mut scrollbar_state =
                        ScrollbarState::new(total_lines.saturating_sub(visible_lines))
                            .position(scrolled)
                            .viewport_content_length(visible_lines);
                    frame.render_stateful_widget(
                        Scrollbar::new(ScrollbarOrientation::VerticalRight)
                            .begin_symbol(None)
                            .end_symbol(None),
                        Rect {
                            x: downloads_area.x,
                            y: downloads_area.y + 1,
                            width: downloads_area.width,
                            height: downloads_area.height.saturating_sub(2),
                        },
                        &mut scrollbar_state,
                    );
                }

                if let Some(screen) = settings.as_mut() {
                    let popup = centered_rect(96, area.height, area);
                    let rows = settings_rows(&config.jobs);
                    let inner = Rect {
                        x: popup.x + 1,
                        y: popup.y + 1,
                        width: popup.width.saturating_sub(2),
                        height: popup.height.saturating_sub(2),
                    };
                    // Three lines at the bottom for help/errors, keys and the target file.
                    let visible = usize::from(inner.height.saturating_sub(4)).max(1);
                    if screen.cursor < screen.offset {
                        screen.offset = screen.cursor;
                    }
                    // Keep a section heading above its first row visible.
                    if screen.offset > 0
                        && screen.offset == screen.cursor
                        && !rows[screen.cursor - 1].selectable()
                    {
                        screen.offset -= 1;
                    }
                    if screen.cursor >= screen.offset + visible {
                        screen.offset = screen.cursor + 1 - visible;
                    }
                    let value_width = usize::from(inner.width.saturating_sub(40));
                    let muted = Style::default().fg(Color::DarkGray);
                    let mut lines: Vec<Line> = Vec::new();
                    for (index, row) in rows.iter().enumerate().skip(screen.offset).take(visible) {
                        let focused = index == screen.cursor;
                        let pointer = Span::styled(if focused { "▸ " } else { "  " }, Style::default().fg(Color::Cyan));
                        let label_style = Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD);
                        let value_style = if focused {
                            Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(Color::White)
                        };
                        lines.push(match row {
                            SettingsRow::Header(title) => Line::from(Span::styled(
                                title.clone(),
                                Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),
                            )),
                            SettingsRow::Field(field_index) => {
                                let field = &GLOBAL_FIELDS[*field_index];
                                let target = SettingTarget::Global(field.path);
                                let mut spans = vec![pointer, Span::styled(format!("{:<20}", field.label), label_style)];
                                match (&screen.input, focused) {
                                    (Some(input), true) => spans.push(Span::styled(
                                        format!("{input}▏"),
                                        Style::default().fg(Color::Black).bg(Color::Cyan),
                                    )),
                                    _ => match field_display(setting_value(&screen.merged, target).as_ref()) {
                                        Some(value) => spans.push(Span::styled(truncate(&value, value_width), value_style)),
                                        None if field.default.is_empty() => spans.push(Span::styled("not set", muted)),
                                        None => spans.push(Span::styled(format!("{} (default)", field.default), muted)),
                                    },
                                }
                                if let FieldKind::Choice(_) = field.kind {
                                    spans.push(Span::styled("  ⇄", muted));
                                }
                                if setting_value(&screen.overlay, target).is_some() {
                                    spans.push(Span::styled("  overridden", Style::default().fg(Color::Yellow)));
                                }
                                Line::from(spans)
                            }
                            SettingsRow::Job(name) => {
                                let mut spans = vec![pointer];
                                if let Some(job) = config.jobs.iter().find(|job| &job.name == name) {
                                    spans.push(Span::styled(
                                        format!("{:<20}", truncate(name, 19)),
                                        if job.enabled() { label_style } else { muted },
                                    ));
                                    // "disabled" gets its own highlighted marker below.
                                    let summary = job_summary(job, &config);
                                    let summary = summary.trim_end_matches(" · disabled").trim_end_matches("disabled");
                                    spans.push(Span::styled(
                                        truncate(summary, value_width),
                                        if job.enabled() { value_style } else { muted },
                                    ));
                                    if !job.enabled() {
                                        spans.push(Span::styled("  disabled", Style::default().fg(Color::Yellow)));
                                    }
                                }
                                Line::from(spans)
                            }
                            SettingsRow::AddJob => match &screen.new_job {
                                Some(new_job) => {
                                    let directories = library_directories(&config);
                                    Line::from(vec![
                                        pointer,
                                        Span::styled(format!("{:<20}", "New job"), label_style),
                                        Span::styled(
                                            format!("{}▏", new_job.name),
                                            Style::default().fg(Color::Black).bg(Color::Cyan),
                                        ),
                                        Span::styled("   in ", muted),
                                        Span::styled(
                                            format!("◂ {} ▸", directories[new_job.directory.min(directories.len() - 1)]),
                                            Style::default().fg(Color::Magenta),
                                        ),
                                    ])
                                }
                                None => Line::from(vec![pointer, Span::styled("+ Add a job…", value_style)]),
                            },
                            SettingsRow::OpenEditor => Line::from(vec![
                                pointer,
                                Span::styled("Open the config in $EDITOR…", value_style),
                                Span::styled("  directories, rsync details, anything else", muted),
                            ]),
                        });
                    }
                    while lines.len() < visible {
                        lines.push(Line::from(""));
                    }
                    lines.push(Line::from(""));
                    let focused_row = rows.get(screen.cursor);
                    lines.push(if let Some(error) = &screen.error {
                        Line::from(Span::styled(truncate(error, usize::from(inner.width)), Style::default().fg(Color::Red)))
                    } else if let Some(name) = &screen.confirm_delete {
                        Line::from(vec![
                            Span::styled(format!("Delete job {name}? "), Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
                            Span::styled("Downloaded files stay on disk.  y delete · n / Esc cancel", Style::default().fg(Color::Gray)),
                        ])
                    } else {
                        let help = match focused_row {
                            Some(SettingsRow::Field(index)) => GLOBAL_FIELDS[*index].help,
                            Some(SettingsRow::Job(_)) => "Enter edit sync settings · Space enable/disable · d delete",
                            Some(SettingsRow::AddJob) if screen.new_job.is_some() => "type the show or movie title · Tab/←/→ library folder · Enter add · Esc cancel",
                            Some(SettingsRow::AddJob) => "track another show or movie; set its filters afterwards",
                            Some(SettingsRow::OpenEditor) => "for options without a field here, e.g. the directories maps",
                            _ => "",
                        };
                        Line::from(Span::styled(help, Style::default().fg(Color::Gray).add_modifier(Modifier::ITALIC)))
                    });
                    lines.push(Line::from(Span::styled(
                        if config_is_writable(&config_path) {
                            format!("Edits are saved to {}", tilde_path(&config_path))
                        } else {
                            format!(
                                "Config is read-only; edits go to {}",
                                state_config_path().map(|path| tilde_path(&path)).unwrap_or_default()
                            )
                        },
                        muted,
                    )));
                    let title = if screen.input.is_some() {
                        "Settings · Enter save · Esc cancel · empty clears"
                    } else {
                        "Settings · ↑/↓ · Enter edit · Del clear · r reset · a add job · E editor · Esc"
                    };
                    frame.render_widget(Clear, popup);
                    frame.render_widget(
                        Paragraph::new(lines)
                            .block(panel_block(title, true))
                            .style(Style::default().fg(Color::White)),
                        popup,
                    );
                }
                if show_job_config {
                    let mut lines = Vec::new();
                    let mut title = "Job configuration · i / Esc close".to_string();
                    if let Some(job) = jobs
                        .get(selected)
                        .and_then(|selected| config.jobs.iter().find(|job| job.name == selected.name))
                    {
                        lines.extend([
                            config_line("Name", job.name.clone()),
                            config_line("Directory", job.directory.clone().unwrap_or_else(|| "—".into())),
                            config_line("Remote dir", job.remote_dir.clone().unwrap_or_else(|| "—".into())),
                            config_line("Local dir", job.local_dir.clone().unwrap_or_else(|| "—".into())),
                            Line::from(""),
                            Line::from(Span::styled(
                                "Sync settings",
                                Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),
                            )),
                        ]);
                        let settings = job_settings(&config.download.mode, job);
                        let cursor = editor.cursor.min(settings.len() - 1);
                        let value_width = usize::from(area.width.min(84).saturating_sub(34));
                        for (index, setting) in settings.iter().enumerate() {
                            let focused = index == cursor;
                            let value = match (focused, &editor.input) {
                                (true, Some(input)) => format!("{input}▏"),
                                _ if setting.kind == SettingKind::Toggle => {
                                    if job_setting_bool(job, setting.key).unwrap_or(false) { "yes" } else { "no" }.to_string()
                                }
                                _ => job_setting_text(job, setting.key).unwrap_or_else(|| match setting.key {
                                    "jellyfin_name" => format!("{} (job name)", job.name),
                                    "auto" => "off".into(),
                                    "max_items" if job.max_size.is_none() => {
                                        format!("{DEFAULT_AUTO_MAX_ITEMS} (default)")
                                    }
                                    "max_items" | "max_size" => "no limit".into(),
                                    "library" => "all libraries".into(),
                                    "delete_watched_after" => "cleanup setting".into(),
                                    _ => "all".into(),
                                }),
                            };
                            let mut spans = vec![
                                Span::styled(
                                    if focused { "▸ " } else { "  " },
                                    Style::default().fg(Color::Cyan),
                                ),
                                Span::styled(
                                    format!("{:<18}", setting.label),
                                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                                ),
                                Span::styled(
                                    truncate(&value, value_width),
                                    if focused && editor.input.is_some() {
                                        Style::default().fg(Color::Black).bg(Color::Cyan)
                                    } else if focused {
                                        Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
                                    } else {
                                        Style::default().fg(Color::White)
                                    },
                                ),
                            ];
                            if editor.overrides.contains(setting.key) {
                                spans.push(Span::styled("  overridden", Style::default().fg(Color::Yellow)));
                            }
                            lines.push(Line::from(spans));
                        }
                        lines.push(Line::from(""));
                        if let Some(error) = &editor.error {
                            lines.push(Line::from(Span::styled(error.clone(), Style::default().fg(Color::Red))));
                        } else if let Some(setting) = settings.get(cursor) {
                            lines.push(Line::from(Span::styled(
                                setting.help,
                                Style::default().fg(Color::Gray).add_modifier(Modifier::ITALIC),
                            )));
                        }
                        let target = if config_is_writable(&config_path) {
                            format!("Edits are saved to {}", tilde_path(&config_path))
                        } else {
                            format!(
                                "Config is read-only; edits go to {}",
                                state_config_path().map(|path| tilde_path(&path)).unwrap_or_default()
                            )
                        };
                        lines.push(Line::from(Span::styled(target, Style::default().fg(Color::DarkGray))));
                        title = if editor.input.is_some() {
                            "Job configuration · Enter save · Esc cancel · empty clears".into()
                        } else {
                            "Job configuration · ↑/↓ · Enter edit/toggle · Del clear · r reset · Esc".into()
                        };
                    } else if let Some(job) = jobs.get(selected) {
                        lines.extend([
                            config_line("Name", job.name.clone()),
                            config_line("Type", "Ad-hoc library download"),
                            config_line("Configured", "No static job configuration"),
                        ]);
                    } else {
                        lines.push(Line::from("No job selected"));
                    }
                    let popup = centered_rect(84, lines.len() as u16 + 2, area);
                    frame.render_widget(Clear, popup);
                    frame.render_widget(
                        Paragraph::new(lines)
                            .block(panel_block(&title, true))
                            .style(Style::default().fg(Color::White)),
                        popup,
                    );
                }
                if let Some((name, info)) = &file_info {
                    let (meta_rows, overview, still) = match file_meta.as_mut() {
                        Some((rows, overview, still)) => (rows.as_slice(), overview.as_str(), still.as_mut()),
                        None => (&[][..], "", None),
                    };
                    let probe_rows: &[(String, String)] = match info {
                        Some(Ok(rows)) => rows,
                        _ => &[],
                    };
                    let row_count = (meta_rows.len() + probe_rows.len()).max(1) as u16;
                    // Overview lines after wrapping (capped), plus one blank separator line.
                    let text_width = usize::from(area.width.saturating_sub(4).min(108)).max(1);
                    let overview_height = if overview.is_empty() {
                        0
                    } else {
                        (overview.chars().count().div_ceil(text_width) as u16).min(6) + 1
                    };
                    let height = (row_count.max(if still.is_some() { 9 } else { 0 })
                        + overview_height
                        + 2)
                    .min(area.height.saturating_sub(2));
                    let popup = centered_rect(110, height, area);
                    let inner = Rect {
                        x: popup.x + 1,
                        y: popup.y + 1,
                        width: popup.width.saturating_sub(2),
                        height: popup.height.saturating_sub(2),
                    };
                    let title = truncate_near_end(name, usize::from(popup.width.saturating_sub(24)));
                    frame.render_widget(Clear, popup);
                    frame.render_widget(
                        panel_block(&format!("{title} · p play · i / Esc close"), true),
                        popup,
                    );
                    // A 16:9 still at 9 rows is ~32 cells wide with 1:2 cells.
                    let still_width = if still.is_some() && inner.width >= 80 { 33 } else { 0 };
                    let rows_area = Rect {
                        x: inner.x + still_width,
                        y: inner.y,
                        width: inner.width.saturating_sub(still_width),
                        height: inner.height.saturating_sub(overview_height),
                    }
                    .intersection(inner);
                    if let Some(still) = still
                        && still_width > 0
                    {
                        frame.render_stateful_widget(
                            StatefulImage::default().resize(Resize::Fit(Some(FilterType::Triangle))),
                            Rect { x: inner.x, y: inner.y, width: still_width - 1, height: rows_area.height.min(9) }
                                .intersection(inner),
                            still,
                        );
                    }
                    let value_width = usize::from(rows_area.width.saturating_sub(14));
                    let mut lines: Vec<Line> = meta_rows
                        .iter()
                        .chain(probe_rows)
                        .map(|(label, value)| config_line(label, truncate(value, value_width)))
                        .collect();
                    match info {
                        None => lines.push(Line::from(Span::styled(
                            "Probing…",
                            Style::default().fg(Color::Gray),
                        ))),
                        Some(Err(error)) => lines.push(Line::from(Span::styled(
                            error.clone(),
                            Style::default().fg(Color::Red),
                        ))),
                        Some(Ok(_)) => {}
                    }
                    frame.render_widget(
                        Paragraph::new(lines).style(Style::default().fg(Color::White)),
                        rows_area,
                    );
                    if overview_height > 0 {
                        frame.render_widget(
                            Paragraph::new(overview.to_string())
                                .wrap(Wrap { trim: true })
                                .style(Style::default().fg(Color::Gray)),
                            Rect {
                                x: inner.x,
                                y: inner.y + inner.height.saturating_sub(overview_height) + 1,
                                width: inner.width,
                                height: overview_height - 1,
                            }
                            .intersection(inner),
                        );
                    }
                }

                let footer_lines = if confirm_clear.is_some() {
                    vec![Line::from(vec![
                        Span::styled(
                            "  CLEAR DOWNLOADS?  ",
                            Style::default()
                                .fg(Color::Yellow)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled("y confirm", Style::default().fg(Color::White)),
                        Span::styled("  ·  ", Style::default().fg(Color::DarkGray)),
                        Span::styled("n / Esc cancel", Style::default().fg(Color::Gray)),
                    ])]
                } else {
                    let mut shortcut_spans = Vec::new();
                    // Hints follow the focused panel so every key shown does something.
                    let mut shortcuts = vec![("Tab", "focus")];
                    if area.width >= 110 {
                        shortcuts.push(("↑/↓", "move"));
                    }
                    if download_focus && !downloads.is_empty() {
                        shortcuts.extend([("p", "play"), ("w", "watched"), ("i", "info"), ("x", "clear")]);
                    } else {
                        shortcuts.extend([("s", "sync"), ("i", "config")]);
                    }
                    shortcuts.extend([("b", "browse"), (",", "settings"), ("?", "more"), ("q", "quit")]);
                    for (index, (key, label)) in shortcuts.into_iter().enumerate() {
                        if index > 0 {
                            shortcut_spans.push(key_sep());
                        }
                        shortcut_spans.extend(key_hint(key, label));
                    }
                    vec![
                        Line::from(shortcut_spans),
                        Line::from(Span::styled(
                            sync_notice
                                .as_ref()
                                .map(|(message, _)| {
                                    format!(
                                        "  {}",
                                        truncate(message, area.width.saturating_sub(2) as usize)
                                    )
                                })
                                // Empty without a notice; the line stays reserved for them.
                                .unwrap_or_default(),
                            Style::default()
                                .fg(sync_notice
                                    .as_ref()
                                    .map(
                                        |(_, success)| {
                                            if *success { Color::Green } else { Color::Red }
                                        },
                                    )
                                    .unwrap_or(Color::DarkGray))
                                .add_modifier(Modifier::ITALIC),
                        )),
                    ]
                };
                frame.render_widget(Paragraph::new(footer_lines), footer);
                if let Some(request) = confirm_clear.as_ref() {
                    let popup = centered_rect(72, 6, area);
                    frame.render_widget(Clear, popup);
                    frame.render_widget(
                        Block::default()
                            .title(" Confirm clear ")
                            .title_style(
                                Style::default()
                                    .fg(Color::Yellow)
                                    .add_modifier(Modifier::BOLD),
                            )
                            .borders(Borders::ALL)
                            .border_type(BorderType::Rounded)
                            .border_style(Style::default().fg(Color::Yellow)),
                        popup,
                    );
                    frame.render_widget(
                        Paragraph::new(vec![
                            Line::from(Span::styled(
                                truncate_near_end(&request.label, usize::from(popup.width.saturating_sub(4))),
                                Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
                            )),
                            Line::from(Span::styled(
                                if request.targets.is_empty() {
                                    "Removes this ad-hoc job from the list."
                                } else if !request.ignore {
                                    "Deletes these files from disk.  y confirm · n / Esc cancel"
                                } else {
                                    "Deletes them and syncs skip them (I undoes).  y confirm · n / Esc"
                                },
                                Style::default().fg(Color::Gray),
                            )),
                        ])
                        .alignment(ratatui::layout::Alignment::Center),
                        Rect {
                            x: popup.x + 1,
                            y: popup.y + 2,
                            width: popup.width.saturating_sub(2),
                            height: 2,
                        }
                        .intersection(popup),
                    );
                }
                if let Some(explore) = explore.as_mut() {
                    let filtered = explore.filtered_items();
                    frame.render_widget(Clear, explore_popup);
                    frame.render_widget(
                        Block::default()
                            .title(Span::styled(
                                " Explore Jellyfin ",
                                Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),
                            ))
                            .borders(Borders::ALL)
                            .border_type(BorderType::Rounded)
                            .border_style(Style::default().fg(Color::Magenta)),
                        explore_popup,
                    );
                    if explore.catalog_loading {
                        frame.render_widget(
                            Paragraph::new("Loading Jellyfin library…")
                                .style(Style::default().fg(Color::Gray))
                                .alignment(ratatui::layout::Alignment::Center)
                                .block(panel_block("Library", true)),
                            explore_list_area,
                        );
                    } else {
                        let rows: Vec<_> = filtered
                            .iter()
                            .map(|item| {
                                let kind = item.item_type.as_deref().unwrap_or("Media");
                                let year = item.production_year.map(|year| format!(" · {year}")).unwrap_or_default();
                                ListItem::new(vec![
                                    Line::from(vec![
                                        Span::raw(POSTER_INDENT),
                                        Span::styled(item.name.clone(), Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
                                    ]),
                                    Line::from(vec![
                                        Span::raw(POSTER_INDENT),
                                        Span::styled(format!("{kind}{year}"), Style::default().fg(Color::Gray)),
                                    ]),
                                    Line::from(vec![
                                        Span::raw(POSTER_INDENT),
                                        Span::styled(
                                            if item.item_type.as_deref() == Some("Series") { "Show contents · Enter open" } else { "Movie · Enter download" },
                                            Style::default().fg(Color::Cyan),
                                        ),
                                    ]),
                                    Line::from(""),
                                ])
                            })
                            .collect();
                        explore_state.select((!rows.is_empty()).then_some(explore.selected.min(rows.len().saturating_sub(1))));
                        frame.render_stateful_widget(
                            List::new(rows)
                                .block(panel_block(
                                    &format!(
                                        "Library · {} · {} · type to search · ^F type",
                                        filtered.len(),
                                        explore.type_filter.label()
                                    ),
                                    !explore.details_focus,
                                ))
                                .highlight_style(Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD))
                                .highlight_symbol("▌ "),
                            explore_list_area,
                            &mut explore_state,
                        );
                        for (visible_index, item) in filtered.iter().skip(explore_state.offset()).enumerate() {
                            let y = explore_list_area.y + 1 + (visible_index as u16 * 4);
                            if y + 4 > explore_list_area.y + explore_list_area.height.saturating_sub(1) {
                                break;
                            }
                            if let Some(protocol) = explore_poster_protocols.get_mut(&item.id) {
                                frame.render_stateful_widget(
                                    StatefulImage::default()
                                        .resize(Resize::Fit(Some(FilterType::Nearest))),
                                    Rect {
                                        x: explore_list_area.x + 3,
                                        y,
                                        width: POSTER_WIDTH,
                                        height: 4,
                                    },
                                    protocol,
                                );
                            } else if explore.poster_id.as_deref() == Some(item.id.as_str())
                                && explore.poster_loaded
                            {
                                frame.render_stateful_widget(
                                    StatefulImage::default()
                                        .resize(Resize::Fit(Some(FilterType::Nearest))),
                                    Rect {
                                        x: explore_list_area.x + 3,
                                        y,
                                        width: POSTER_WIDTH,
                                        height: 4,
                                    },
                                    &mut poster_protocol,
                                );
                            }
                        }
                        if let Some(item) = filtered.get(explore.selected) {
                            let details_area = Rect { x: explore_poster_area.x + 1, y: explore_poster_area.y + 1, width: explore_poster_area.width.saturating_sub(2), height: 3 };
                            frame.render_widget(
                                Paragraph::new(vec![
                                    Line::from(Span::styled(item.name.clone(), Style::default().fg(Color::White).add_modifier(Modifier::BOLD))),
                                    Line::from(Span::styled(format!("{}{}", item.item_type.as_deref().unwrap_or("Media"), item.production_year.map(|year| format!(" · {year}")).unwrap_or_default()), Style::default().fg(Color::Gray))),
                                    Line::from(Span::styled(
                                        if explore.details_focus {
                                            "Space select · a select all · d download · Tab/←/Esc back"
                                        } else {
                                            "Tab/→/Enter select episodes · Enter download movie"
                                        },
                                        Style::default().fg(Color::Cyan),
                                    )),
                                ]),
                                details_area,
                            );
                            let content_area = explore_contents_area;
                            if item.item_type.as_deref() == Some("Series") {
                                if explore.preview_loading && explore.episodes.is_empty() {
                                    frame.render_widget(
                                        Paragraph::new("Loading show contents…")
                                            .style(Style::default().fg(Color::DarkGray))
                                            .alignment(ratatui::layout::Alignment::Center)
                                            .block(panel_block("Contents", false)),
                                        content_area,
                                    );
                                } else {
                                    let rows: Vec<_> = explore
                                        .episodes
                                        .iter()
                                        .enumerate()
                                        .map(|(index, episode)| {
                                            let season = episode.parent_index_number.unwrap_or(0);
                                            let number = episode.index_number.unwrap_or(0);
                                            let mark = if explore.selected_episodes.contains(&index) { format!("{} ", icon::CHECK) } else { "  ".into() };
                                            ListItem::new(format!(
                                                "{mark}S{season:02}E{number:02}  {}",
                                                episode.name
                                            ))
                                        })
                                        .collect();
                                    episode_state.select(
                                        (explore.details_focus && !rows.is_empty())
                                            .then_some(explore.episode_cursor),
                                    );
                                    frame.render_stateful_widget(
                                        List::new(rows)
                                            .block(panel_block(
                                                &format!(
                                                    "Contents · {} episodes · {} selected",
                                                    explore.episodes.len(),
                                                    explore.selected_episodes.len()
                                                ),
                                                explore.details_focus,
                                            ))
                                            .highlight_style(Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD))
                                            .highlight_symbol("▌ "),
                                        content_area,
                                        &mut episode_state,
                                    );
                                }
                            } else {
                                frame.render_widget(
                                    Paragraph::new(vec![
                                        Line::from(Span::styled(
                                            "Movie",
                                            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                                        )),
                                        Line::from(Span::styled(
                                            "This title has no episodic contents.",
                                            Style::default().fg(Color::Gray),
                                        )),
                                        Line::from(Span::styled(
                                            "Press Enter to download it.",
                                            Style::default().fg(Color::Gray),
                                        )),
                                    ])
                                    .block(panel_block("Contents", false)),
                                    content_area,
                                );
                            }
                        }
                    }
                    let footer = Rect { x: explore_popup.x + 1, y: explore_popup.y + explore_popup.height.saturating_sub(2), width: explore_popup.width.saturating_sub(2), height: 1 };
                    let footer_line = if let Some((notice, success)) = &explore_notice {
                        Line::from(vec![
                            Span::styled(
                                format!("{}  ·  ", explore.search),
                                Style::default().fg(Color::Gray),
                            ),
                            Span::styled(
                                truncate(notice, explore_popup.width.saturating_sub(24) as usize),
                                if *success {
                                    Style::default().fg(Color::Green)
                                } else {
                                    Style::default()
                                        .fg(Color::Red)
                                        .add_modifier(Modifier::ITALIC)
                                },
                            ),
                            Span::styled("  ·  Esc close", Style::default().fg(Color::Gray)),
                        ])
                    } else {
                        Line::from(format!(
                            "{}  ·  ↑/↓ navigate  ·  Tab switch pane  ·  Enter download  ·  Esc {}",
                            if explore.details_focus {
                                "episode selection".to_string()
                            } else {
                                format!("Search: {} · Filter: {}", explore.search, explore.type_filter.label())
                            },
                            if explore.details_focus { "back" } else { "close" }
                        ))
                    };
                    frame.render_widget(
                        Paragraph::new(footer_line).style(Style::default().fg(Color::Gray)),
                        footer,
                    );
                }
                if show_help {
                    let help_popup = centered_rect(84, 34, area);
                    let help_lines = vec![
                        Line::from(Span::styled(
                            "Main view",
                            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                        )),
                        Line::from("  ↑/↓ or click     select a job or download"),
                        Line::from("  PgUp/PgDn Home/End  jump through the list"),
                        Line::from("  Tab / ← / →       switch focus"),
                        Line::from("  Mouse drag        resize Jobs/Files split (wide terminals)"),
                        Line::from("                    and the Details/Files split"),
                        Line::from("  s / S             sync selected / all jobs"),
                        Line::from("  b                 browse the Jellyfin library"),
                        Line::from("  i                 job configuration and sync settings (Jobs focused)"),
                        Line::from("  ,                 settings: all options, add / disable / delete jobs"),
                        Line::from("  E                 open the config (or its overlay) in $EDITOR"),
                        Line::from("  i / Enter         file media info (Files focused)"),
                        Line::from("  p / double-click  play selected file"),
                        Line::from("  Space             pause / resume selected download"),
                        Line::from("  o                 open the file's or show's directory"),
                        Line::from("  x / X             delete file / delete and ignore it in syncs"),
                        Line::from("  c                 delete and ignore the whole show"),
                        Line::from("  I                 ignore the file in syncs / sync it again"),
                        Line::from("  w / W             toggle watched: file (job in Jobs) / season"),
                        Line::from("  q or Ctrl-C twice quit the TUI (Esc closes dialogs)"),
                        Line::from(""),
                        Line::from(Span::styled(
                            "Explore",
                            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                        )),
                        Line::from("  ↑/↓               move through library or episodes"),
                        Line::from("  /                 clear the search"),
                        Line::from("  type / Backspace  search the library"),
                        Line::from("  Ctrl-F            cycle type filter"),
                        Line::from("  Tab / ← / → / Enter  switch library / details pane"),
                        Line::from("  Space / a         select episode / select all"),
                        Line::from("  d                 download selected media"),
                        Line::from("  Ctrl-C or Esc     close Explore (q too in the details pane)"),
                        Line::from(""),
                        Line::from(Span::styled(
                            "Ad-hoc downloads continue after Explore closes.",
                            Style::default().fg(Color::Gray),
                        )),
                    ];
                    frame.render_widget(Clear, help_popup);
                    frame.render_widget(
                        Paragraph::new(help_lines)
                            .block(panel_block("Keyboard shortcuts · ? / Esc close", true))
                            .style(Style::default().fg(Color::White)),
                        help_popup,
                    );
                }
            })?;
            if stale_layout {
                continue;
            }
            // Footer notices fade after a while; failures stay a little longer.
            match (&sync_notice, &notice_seen) {
                (None, _) => notice_seen = None,
                (Some((message, _)), Some((seen, _))) if message == seen => {}
                (Some((message, _)), _) => notice_seen = Some((message.clone(), Instant::now())),
            }
            if let (Some((message, success)), Some((_, at))) = (&sync_notice, &notice_seen) {
                let ttl = if message == QUIT_HINT {
                    // Only meaningful within the double-press window.
                    Duration::from_secs(1)
                } else if *success {
                    NOTICE_TTL
                } else {
                    NOTICE_TTL * 2
                };
                if at.elapsed() >= ttl {
                    sync_notice = None;
                    notice_seen = None;
                }
            }
            // Remember what is selected so the next frame can follow it by identity.
            selection_anchor = (
                jobs.get(selected).map(|job| (selected, job.name.clone())),
                downloads
                    .get(selected_download)
                    .map(|entry| (selected_download, entry.item_id.clone())),
            );
            if event::poll(Duration::from_millis(250))? {
                let input = event::read()?;
                let swallowed = matches!(&input, Event::Key(key) if reply_filter.swallow(key));
                if let Some(log) = input_log.as_mut() {
                    use std::io::Write as _;
                    let _ = writeln!(log, "{:?} swallowed={swallowed} {input:?}", Instant::now());
                }
                if swallowed {
                    continue;
                }
                if let Event::Resize(_, _) = input {
                    terminal.autoresize()?;
                    terminal.clear()?;
                    // Switching tmux clients resizes the window; the new viewer may
                    // have a different cell size.
                    cell_checked_at = Instant::now()
                        .checked_sub(CELL_CHECK_INTERVAL)
                        .unwrap_or(cell_checked_at);
                    continue;
                }
                if let Event::Mouse(mouse) = &input {
                    if let Some(browser) = explore.as_mut() {
                        let in_library = mouse.column >= explore_list_area.x
                            && mouse.column < explore_list_area.x + explore_list_area.width
                            && mouse.row >= explore_list_area.y
                            && mouse.row < explore_list_area.y + explore_list_area.height;
                        let in_details = mouse.column >= explore_poster_area.x
                            && mouse.column < explore_poster_area.x + explore_poster_area.width
                            && mouse.row >= explore_poster_area.y
                            && mouse.row < explore_poster_area.y + explore_poster_area.height;
                        let has_episodes = !browser.episodes.is_empty();
                        match mouse.kind {
                            MouseEventKind::Down(MouseButton::Left) if in_details && has_episodes => {
                                browser.details_focus = true;
                                if let Some(index) = list_index_at(
                                    explore_contents_area,
                                    mouse.column,
                                    mouse.row,
                                    episode_state.offset(),
                                    browser.episodes.len(),
                                    1,
                                ) {
                                    browser.episode_cursor = index;
                                    if !browser.selected_episodes.remove(&index) {
                                        browser.selected_episodes.insert(index);
                                    }
                                }
                            }
                            MouseEventKind::Down(MouseButton::Left) if in_library => {
                                browser.details_focus = false;
                                if let Some(index) = list_index_at(
                                    explore_list_area,
                                    mouse.column,
                                    mouse.row,
                                    explore_state.offset(),
                                    browser.filtered_items().len(),
                                    4,
                                ) {
                                    browser.selected = index;
                                }
                            }
                            MouseEventKind::ScrollUp if in_details && has_episodes => {
                                browser.details_focus = true;
                                browser.episode_cursor = browser.episode_cursor.saturating_sub(1);
                            }
                            MouseEventKind::ScrollDown if in_details && has_episodes => {
                                browser.details_focus = true;
                                if browser.episode_cursor + 1 < browser.episodes.len() {
                                    browser.episode_cursor += 1;
                                }
                            }
                            MouseEventKind::ScrollUp if !browser.details_focus => {
                                browser.selected = browser.selected.saturating_sub(1);
                            }
                            MouseEventKind::ScrollDown if !browser.details_focus => {
                                let len = browser.filtered_items().len();
                                if browser.selected + 1 < len {
                                    browser.selected += 1;
                                }
                            }
                            _ => {}
                        }
                        if let Some(item) = browser
                                .filtered_items()
                                .get(browser.selected)
                                .map(|item| (*item).clone())
                            && let Err(error) = request_poster(
                                browser,
                                item,
                                &picker,
                                &poster_cache,
                                &mut poster_protocol,
                                &mut poster_task,
                            ) {
                            explore_notice = Some((format!("Could not load poster: {error:#}"), false));
                        }
                    } else if confirm_clear.is_none()
                        && !show_help
                        && !show_job_config
                        && settings.is_none()
                        && file_info.is_none()
                    {
                        match mouse.kind {
                            MouseEventKind::Down(MouseButton::Left)
                                if area.width >= 100
                                    && mouse.column.abs_diff(downloads_area.x) <= 1
                                    && mouse.row >= body.y
                                    && mouse.row < body.y + body.height =>
                            {
                                resizing_split = true;
                            }
                            // The border between the Details panel and the Files list.
                            MouseEventKind::Down(MouseButton::Left)
                                if details_area.is_some()
                                    && (mouse.row == downloads_area.y
                                        || mouse.row + 1 == downloads_area.y)
                                    && mouse.column >= downloads_area.x
                                    && mouse.column < downloads_area.x + downloads_area.width =>
                            {
                                resizing_details = true;
                            }
                            MouseEventKind::Drag(MouseButton::Left) if resizing_split => {
                                let relative = mouse.column.saturating_sub(body.x);
                                let ratio = relative.saturating_mul(100) / body.width.max(1);
                                main_split = ratio.clamp(20, 80);
                            }
                            MouseEventKind::Drag(MouseButton::Left) if resizing_details => {
                                if let Some(details) = details_area {
                                    // Clamped to the available room when laid out.
                                    details_height = Some(mouse.row.saturating_sub(details.y) + 1);
                                }
                            }
                            MouseEventKind::Up(MouseButton::Left) => {
                                resizing_split = false;
                                resizing_details = false;
                            }
                            MouseEventKind::Down(MouseButton::Left) => {
                                if let Some(index) = list_index_at(
                                    jobs_area,
                                    mouse.column,
                                    mouse.row,
                                    jobs_state.offset(),
                                    jobs.len(),
                                    4,
                                ) {
                                    selected = index;
                                    selected_download = 0;
                                    download_focus = false;
                                } else if let Some(index) = file_row_at(
                                    downloads_area,
                                    mouse.column,
                                    mouse.row,
                                    downloads_state.offset(),
                                    &rows,
                                ) {
                                    selected_download = index;
                                    download_focus = true;
                                    let now = Instant::now();
                                    let double_click = last_download_click.is_some_and(|(last, at)| {
                                        last == index
                                            && now.duration_since(at) <= Duration::from_millis(400)
                                    });
                                    if double_click {
                                        last_download_click = None;
                                        if let Some(entry) = downloads.get(index) {
                                            sync_notice = match play_download(&config, entry) {
                                                Ok(()) => Some((
                                                    format!("Playing {} with {}", entry.path.display(), config.player),
                                                    true,
                                                )),
                                                Err(error) => Some((format!("Could not start player: {error:#}"), false)),
                                            };
                                        }
                                    } else {
                                        last_download_click = Some((index, now));
                                    }
                                }
                            }
                            MouseEventKind::ScrollUp
                                if mouse.column >= jobs_area.x
                                    && mouse.column < jobs_area.x + jobs_area.width
                                    && mouse.row >= jobs_area.y
                                    && mouse.row < jobs_area.y + jobs_area.height =>
                            {
                                selected = selected.saturating_sub(1);
                                selected_download = 0;
                                download_focus = false;
                            }
                            MouseEventKind::ScrollDown
                                if mouse.column >= jobs_area.x
                                    && mouse.column < jobs_area.x + jobs_area.width
                                    && mouse.row >= jobs_area.y
                                    && mouse.row < jobs_area.y + jobs_area.height
                                    && selected + 1 < jobs.len() =>
                            {
                                selected += 1;
                                selected_download = 0;
                                download_focus = false;
                            }
                            MouseEventKind::ScrollUp
                                if mouse.column >= downloads_area.x
                                    && mouse.column < downloads_area.x + downloads_area.width
                                    && mouse.row >= downloads_area.y
                                    && mouse.row < downloads_area.y + downloads_area.height =>
                            {
                                selected_download = selected_download.saturating_sub(1);
                                download_focus = true;
                            }
                            MouseEventKind::ScrollDown
                                if mouse.column >= downloads_area.x
                                    && mouse.column < downloads_area.x + downloads_area.width
                                    && mouse.row >= downloads_area.y
                                    && mouse.row < downloads_area.y + downloads_area.height
                                    && selected_download + 1 < downloads.len() =>
                            {
                                selected_download += 1;
                                download_focus = true;
                            }
                            _ => {}
                        }
                    }
                } else if let Event::Key(key) = input {
                // In the Explore library pane every printable key belongs to the search.
                if key.code == KeyCode::Char('?')
                    && explore.as_ref().is_none_or(|browser| browser.details_focus)
                {
                    show_help = !show_help;
                    show_job_config = false;
                } else if show_help && matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) {
                    show_help = false;
                } else if file_info.is_some() && key.code == KeyCode::Char('p') {
                    // Play the file the dialog describes, even if the list moved since.
                    if let Some(entry) = file_info_entry.as_ref() {
                        sync_notice = match play_download(&config, entry) {
                            Ok(()) => Some((
                                format!("Playing {} with {}", entry.path.display(), config.player),
                                true,
                            )),
                            Err(error) => Some((format!("Could not start player: {error:#}"), false)),
                        };
                    }
                } else if file_info.is_some()
                    && matches!(key.code, KeyCode::Esc | KeyCode::Char('i') | KeyCode::Char('q'))
                {
                    if let Some(task) = file_info_task.take() {
                        task.abort();
                    }
                    if let Some(task) = file_meta_task.take() {
                        task.abort();
                    }
                    file_info = None;
                    file_meta = None;
                } else if settings.is_some() && !key.modifiers.contains(KeyModifiers::CONTROL) {
                    let rows = settings_rows(&config.jobs);
                    let overlay_path = state_config_path();
                    // A save to perform, with the label for its notice.
                    let mut saved: Option<(Result<PathBuf>, String)> = None;
                    let mut close = false;
                    let mut open_job: Option<String> = None;
                    let mut launch_editor = false;
                    let screen = settings.as_mut().expect("settings screen is open");
                    let row = rows.get(screen.cursor).cloned();
                    if let Some(name) = screen.confirm_delete.clone() {
                        match key.code {
                            KeyCode::Char('y') => {
                                screen.confirm_delete = None;
                                saved = Some((
                                    overlay_path.and_then(|overlay| delete_job(&config_path, &overlay, &name)),
                                    format!("Deleted job {name}"),
                                ));
                            }
                            KeyCode::Char('n') | KeyCode::Esc => screen.confirm_delete = None,
                            _ => {}
                        }
                    } else if let Some(new_job) = screen.new_job.as_mut() {
                        let directories = library_directories(&config);
                        match key.code {
                            KeyCode::Char(character) => new_job.name.push(character),
                            KeyCode::Backspace => {
                                new_job.name.pop();
                            }
                            KeyCode::Tab | KeyCode::Right => {
                                new_job.directory = (new_job.directory + 1) % directories.len();
                            }
                            KeyCode::BackTab | KeyCode::Left => {
                                new_job.directory =
                                    new_job.directory.checked_sub(1).unwrap_or(directories.len() - 1);
                            }
                            KeyCode::Enter => {
                                let name = new_job.name.trim().to_string();
                                let directory = directories[new_job.directory.min(directories.len() - 1)].clone();
                                saved = Some((
                                    overlay_path.and_then(|overlay| add_job(&config_path, &overlay, &name, &directory)),
                                    format!("Added job {name}"),
                                ));
                            }
                            KeyCode::Esc => {
                                screen.new_job = None;
                                screen.error = None;
                            }
                            _ => {}
                        }
                    } else if let Some(input) = screen.input.as_mut() {
                        match key.code {
                            KeyCode::Char(character) => input.push(character),
                            KeyCode::Backspace => {
                                input.pop();
                            }
                            KeyCode::Esc => {
                                screen.input = None;
                                screen.error = None;
                            }
                            KeyCode::Enter => {
                                if let Some(SettingsRow::Field(index)) = &row {
                                    let field = &GLOBAL_FIELDS[*index];
                                    match parse_field_input(field.kind, input) {
                                        Ok(change) => {
                                            saved = Some((
                                                overlay_path.and_then(|overlay| {
                                                    save_setting(&config_path, &overlay, SettingTarget::Global(field.path), change)
                                                }),
                                                format!("Saved {}", field.label),
                                            ));
                                        }
                                        Err(error) => screen.error = Some(format!("{error:#}")),
                                    }
                                }
                            }
                            _ => {}
                        }
                    } else {
                        let selectable: Vec<usize> = (0..rows.len()).filter(|index| rows[*index].selectable()).collect();
                        let position = selectable.iter().position(|index| *index == screen.cursor).unwrap_or(0);
                        match key.code {
                            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char(',') => close = true,
                            KeyCode::Up => {
                                screen.cursor = selectable[position.checked_sub(1).unwrap_or(selectable.len() - 1)];
                                screen.error = None;
                            }
                            KeyCode::Down => {
                                screen.cursor = selectable[(position + 1) % selectable.len()];
                                screen.error = None;
                            }
                            KeyCode::PageUp | KeyCode::PageDown | KeyCode::Home | KeyCode::End => {
                                screen.cursor = selectable[list_jump(key.code, position, selectable.len(), 10)];
                            }
                            KeyCode::Char('E') => launch_editor = true,
                            KeyCode::Char('a') => {
                                screen.new_job = Some(NewJob { name: String::new(), directory: 0 });
                            }
                            KeyCode::Enter | KeyCode::Char('e') => match &row {
                                Some(SettingsRow::Field(index)) => {
                                    let field = &GLOBAL_FIELDS[*index];
                                    let current = field_display(
                                        setting_value(&screen.merged, SettingTarget::Global(field.path)).as_ref(),
                                    );
                                    if let FieldKind::Choice(choices) = field.kind {
                                        let current = current.unwrap_or_else(|| field.default.to_string());
                                        let next = choices
                                            .iter()
                                            .position(|choice| *choice == current)
                                            .map_or(0, |index| (index + 1) % choices.len());
                                        saved = Some((
                                            overlay_path.and_then(|overlay| {
                                                save_setting(
                                                    &config_path,
                                                    &overlay,
                                                    SettingTarget::Global(field.path),
                                                    SettingChange::Set(choices[next].into()),
                                                )
                                            }),
                                            format!("Set {} to {}", field.label, choices[next]),
                                        ));
                                    } else {
                                        screen.input = Some(current.unwrap_or_default());
                                        screen.error = None;
                                    }
                                }
                                Some(SettingsRow::Job(name)) => {
                                    open_job = Some(name.clone());
                                    close = true;
                                }
                                Some(SettingsRow::AddJob) => {
                                    screen.new_job = Some(NewJob { name: String::new(), directory: 0 });
                                }
                                Some(SettingsRow::OpenEditor) => launch_editor = true,
                                _ => {}
                            },
                            KeyCode::Char(' ') => {
                                if let Some(SettingsRow::Job(name)) = &row
                                    && let Some(job) = config.jobs.iter().find(|job| &job.name == name)
                                {
                                    let enable = !job.enabled();
                                    saved = Some((
                                        overlay_path.and_then(|overlay| {
                                            save_job_setting(
                                                &config_path,
                                                &overlay,
                                                name,
                                                "enabled",
                                                SettingChange::Set(enable.into()),
                                            )
                                        }),
                                        format!("{} job {name}", if enable { "Enabled" } else { "Disabled" }),
                                    ));
                                }
                            }
                            KeyCode::Char('d') | KeyCode::Delete | KeyCode::Backspace => match &row {
                                Some(SettingsRow::Job(name)) => screen.confirm_delete = Some(name.clone()),
                                Some(SettingsRow::Field(index)) => {
                                    let field = &GLOBAL_FIELDS[*index];
                                    saved = Some((
                                        overlay_path.and_then(|overlay| {
                                            save_setting(&config_path, &overlay, SettingTarget::Global(field.path), SettingChange::Clear)
                                        }),
                                        format!("Cleared {}", field.label),
                                    ));
                                }
                                _ => {}
                            },
                            KeyCode::Char('r') => {
                                if let Some(SettingsRow::Field(index)) = &row {
                                    let field = &GLOBAL_FIELDS[*index];
                                    saved = Some((
                                        overlay_path.and_then(|overlay| {
                                            save_setting(&config_path, &overlay, SettingTarget::Global(field.path), SettingChange::Reset)
                                        }),
                                        format!("Reset {}", field.label),
                                    ));
                                }
                            }
                            _ => {}
                        }
                    }
                    if let Some((result, label)) = saved {
                        match result.and_then(|path| Ok((path, load_config(&config_path)?))) {
                            Ok((path, reloaded)) => {
                                config = reloaded;
                                screen.reload(&config_path);
                                screen.input = None;
                                screen.new_job = None;
                                screen.error = None;
                                sync_notice = Some((format!("{label} in {}", tilde_path(&path)), true));
                            }
                            Err(error) => screen.error = Some(format!("{error:#}")),
                        }
                    }
                    if launch_editor {
                        sync_notice = Some(edit_config_file(&mut terminal, &config_path, &mut config));
                        if let Some(screen) = settings.as_mut() {
                            screen.reload(&config_path);
                        }
                    }
                    if close {
                        settings = None;
                    }
                    if let Some(name) = open_job
                        && let Some(index) = jobs.iter().position(|job| job.name == name)
                    {
                        selected = index;
                        download_focus = false;
                        show_job_config = true;
                        editor = JobEditor {
                            overrides: state_config_path()
                                .map(|overlay| overlay_job_keys(&overlay, &name))
                                .unwrap_or_default(),
                            ..JobEditor::default()
                        };
                    }
                } else if show_job_config && !key.modifiers.contains(KeyModifiers::CONTROL) {
                    let job = jobs
                        .get(selected)
                        .and_then(|selected| config.jobs.iter().find(|job| job.name == selected.name))
                        .cloned();
                    let settings = job.as_ref().map_or_else(Vec::new, |job| job_settings(&config.download.mode, job));
                    let setting = settings.get(editor.cursor.min(settings.len().saturating_sub(1))).copied();
                    // The change to save, if this key produced one.
                    let mut change = None;
                    match (&job, editor.input.as_mut()) {
                        (Some(_), Some(input)) => match key.code {
                            KeyCode::Char(character) => input.push(character),
                            KeyCode::Backspace => {
                                input.pop();
                            }
                            KeyCode::Esc => {
                                editor.input = None;
                                editor.error = None;
                            }
                            KeyCode::Enter => {
                                if let Some(setting) = setting {
                                    match parse_setting_input(setting.kind, input) {
                                        Ok(parsed) => change = Some(parsed),
                                        Err(error) => editor.error = Some(format!("{error:#}")),
                                    }
                                }
                            }
                            _ => {}
                        },
                        (Some(job), None) => match key.code {
                            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('i') => show_job_config = false,
                            KeyCode::Up => {
                                editor.cursor = editor.cursor.checked_sub(1).unwrap_or(settings.len() - 1);
                                editor.error = None;
                            }
                            KeyCode::Down => {
                                editor.cursor = (editor.cursor + 1) % settings.len();
                                editor.error = None;
                            }
                            KeyCode::Enter | KeyCode::Char('e') | KeyCode::Char(' ') => {
                                if let Some(setting) = setting {
                                    if setting.kind == SettingKind::Toggle {
                                        let current = job_setting_bool(job, setting.key).unwrap_or(false);
                                        change = Some(SettingChange::Set(serde_yaml::Value::Bool(!current)));
                                    } else if let SettingKind::Choice(choices) = setting.kind {
                                        let current = job_setting_text(job, setting.key).unwrap_or_default();
                                        let next = choices
                                            .iter()
                                            .position(|choice| *choice == current)
                                            .map_or(1, |index| (index + 1) % choices.len());
                                        change = Some(if next == 0 {
                                            SettingChange::Clear
                                        } else {
                                            SettingChange::Set(choices[next].into())
                                        });
                                    } else if key.code != KeyCode::Char(' ') {
                                        editor.input = Some(job_setting_text(job, setting.key).unwrap_or_default());
                                        editor.error = None;
                                    }
                                }
                            }
                            KeyCode::Delete | KeyCode::Backspace => change = Some(SettingChange::Clear),
                            KeyCode::Char('r') => change = Some(SettingChange::Reset),
                            _ => {}
                        },
                        (None, _) => {
                            if matches!(key.code, KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('i')) {
                                show_job_config = false;
                            }
                        }
                    }
                    if let (Some(change), Some(job), Some(setting)) = (change, &job, setting) {
                        let saved = state_config_path().and_then(|overlay| {
                            let path = save_job_setting(&config_path, &overlay, &job.name, setting.key, change)?;
                            Ok((path, load_config(&config_path)?, overlay))
                        });
                        match saved {
                            Ok((path, reloaded, overlay)) => {
                                config = reloaded;
                                editor.overrides = overlay_job_keys(&overlay, &job.name);
                                editor.input = None;
                                editor.error = None;
                                sync_notice = Some((
                                    format!("Saved {} for {} to {}; the next sync uses it", setting.label, job.name, tilde_path(&path)),
                                    true,
                                ));
                                if setting.key == "jellyfin_name" {
                                    // The job may now point at another show: look it up again.
                                    job_details.remove(&job.name);
                                    job_items.remove(&job.name);
                                    job_details_failed.remove(&job.name);
                                    job_poster_protocols.remove(&job.name);
                                    if online != Some(Some(false)) && config.jellyfin.is_some() {
                                        if let Some(task) = job_poster_catalog_task.take() {
                                            task.abort();
                                        }
                                        let config = config.clone();
                                        job_poster_catalog_task = Some(tokio::spawn(async move {
                                            jellyfin_job_poster_catalog(&config).await
                                        }));
                                    }
                                }
                            }
                            Err(error) => editor.error = Some(format!("{error:#}")),
                        }
                    }
                } else if key.code == KeyCode::Char('c')
                    && key.modifiers.contains(KeyModifiers::CONTROL)
                {
                    // Ctrl-C closes the topmost dialog first. Closing one does not count
                    // towards quitting: the next Ctrl-C is the first of the quitting pair.
                    if show_help
                        || show_job_config
                        || settings.is_some()
                        || confirm_clear.is_some()
                        || file_info.is_some()
                    {
                        settings = None;
                        show_help = false;
                        show_job_config = false;
                        confirm_clear = None;
                        if let Some(task) = file_info_task.take() {
                            task.abort();
                        }
                        if let Some(task) = file_meta_task.take() {
                            task.abort();
                        }
                        file_info = None;
                        file_meta = None;
                        last_ctrl_c = None;
                    } else if explore.is_some() {
                        last_ctrl_c = None;
                        if let Some(task) = catalog_task.take() {
                            task.abort();
                        }
                        if let Some(task) = poster_task.take() {
                            task.abort();
                        }
                        for (_, task) in explore_poster_tasks.drain() {
                            task.abort();
                        }
                        if let Some(task) = preview_episode_task.take() {
                            task.abort();
                        }
                        poster_protocol.empty_protocol();
                        explore = None;
                        explore_notice = None;
                    } else if last_ctrl_c.is_some_and(|time| time.elapsed() < Duration::from_secs(1)) {
                        break;
                    } else {
                        last_ctrl_c = Some(Instant::now());
                        sync_notice = Some((QUIT_HINT.into(), false));
                    }
                } else if show_help || show_job_config || settings.is_some() || file_info.is_some() {
                    // Dialogs are modal: other keys must not act on the view behind them.
                } else if let Some(browser) = explore.as_mut() {
                    last_ctrl_c = None;
                    match key.code {
                        KeyCode::Esc | KeyCode::Tab | KeyCode::Left if browser.details_focus => {
                            browser.details_focus = false;
                        }
                        KeyCode::Tab | KeyCode::Right | KeyCode::Enter
                            if !browser.details_focus && !browser.episodes.is_empty() =>
                        {
                            browser.details_focus = true;
                            explore_notice = None;
                        }
                        // Esc in the details pane goes back (handled above); q there closes
                        // Explore, while in the library pane it is typed into the search.
                        KeyCode::Esc | KeyCode::Char('q')
                            if key.code == KeyCode::Esc || browser.details_focus =>
                        {
                            if let Some(task) = catalog_task.take() {
                                task.abort();
                            }
                            if let Some(task) = poster_task.take() {
                                task.abort();
                            }
                            for (_, task) in explore_poster_tasks.drain() {
                                task.abort();
                            }
                            if let Some(task) = preview_episode_task.take() {
                                task.abort();
                            }
                            poster_protocol.empty_protocol();
                            explore = None;
                            explore_notice = None;
                        }
                        KeyCode::Up if browser.details_focus => {
                            browser.episode_cursor = browser.episode_cursor.saturating_sub(1);
                        }
                        KeyCode::Down if browser.details_focus => {
                            if browser.episode_cursor + 1 < browser.episodes.len() {
                                browser.episode_cursor += 1;
                            }
                        }
                        KeyCode::Up => {
                            browser.selected = browser.selected.saturating_sub(1);
                        }
                        KeyCode::Down => {
                            let len = browser.filtered_items().len();
                            if browser.selected + 1 < len {
                                browser.selected += 1;
                            }
                        }
                        KeyCode::Char(' ') if browser.details_focus && !browser.episodes.is_empty() => {
                            if !browser.selected_episodes.remove(&browser.episode_cursor) {
                                browser.selected_episodes.insert(browser.episode_cursor);
                            }
                        }
                        KeyCode::Char('a') if browser.details_focus => {
                            browser.selected_episodes = (0..browser.episodes.len()).collect();
                        }
                        KeyCode::Char('/') if !browser.details_focus => {
                            browser.search.clear();
                            browser.selected = 0;
                        }
                        KeyCode::Backspace if !browser.details_focus => {
                            browser.search.pop();
                            browser.selected = 0;
                        }
                        // Ctrl-F, so that a plain f can be typed into the search.
                        KeyCode::Char('f')
                            if !browser.details_focus
                                && key.modifiers.contains(KeyModifiers::CONTROL) =>
                        {
                            browser.type_filter = browser.type_filter.next();
                            browser.selected = 0;
                            explore_notice = None;
                        }
                        // In the library pane letters go to the search, so only Enter downloads.
                        KeyCode::Char('d') | KeyCode::Enter
                            if browser.details_focus || key.code == KeyCode::Enter =>
                        {
                            let queued = if browser.details_focus
                                && let Some(title) = browser
                                    .filtered_items()
                                    .get(browser.selected)
                                    .map(|item| item.name.clone())
                            {
                                let mut indices: Vec<_> = browser.selected_episodes.iter().copied().collect();
                                indices.sort_unstable();
                                let items: Vec<_> = indices
                                    .into_iter()
                                    .filter_map(|index| browser.episodes.get(index).cloned())
                                    .collect();
                                if items.is_empty() {
                                    explore_notice = Some(("Select episodes with Space or a first, then press d or Enter".into(), false));
                                    None
                                } else {
                                    Some(queue_library_items(&config, &items, &title, false))
                                }
                            } else if let Some(item) = browser.filtered_items().get(browser.selected).map(|item| (*item).clone())
                                && item.item_type.as_deref() == Some("Movie")
                            {
                                Some(queue_library_items(&config, std::slice::from_ref(&item), &item.name, true))
                            } else {
                                None
                            };
                            match queued {
                                Some(Ok((count, job, first_item))) => {
                                    focus_request = Some((job.clone(), first_item, Instant::now()));
                                    explore_notice = Some(match spawn_library_worker(&config_path, &job) {
                                        Ok(()) => (format!("Queued {}; they continue after the TUI exits", plural(count, "download")), true),
                                        Err(error) => (format!("Queued {} but could not start worker: {error:#}", plural(count, "download")), false),
                                    });
                                    if let Some(task) = job_poster_catalog_task.take() {
                                        task.abort();
                                    }
                                    let config = config.clone();
                                    job_poster_catalog_task = Some(tokio::spawn(async move {
                                        jellyfin_job_poster_catalog(&config).await
                                    }));
                                }
                                Some(Err(error)) => {
                                    explore_notice = Some((format!("Could not queue download: {error:#}"), false));
                                }
                                None => {}
                            }
                        }
                        KeyCode::Char(character)
                            if !browser.details_focus
                                && !character.is_control()
                                && !key.modifiers.contains(KeyModifiers::CONTROL) =>
                        {
                            browser.search.push(character);
                            browser.selected = 0;
                        }
                        _ => {}
                    }
                    if let Some(browser) = explore.as_mut()
                        && let Some(item) = browser
                            .filtered_items()
                            .get(browser.selected)
                            .map(|item| (*item).clone())
                        && let Err(error) = request_poster(
                            browser,
                            item,
                            &picker,
                            &poster_cache,
                            &mut poster_protocol,
                            &mut poster_task,
                        ) {
                        explore_notice = Some((format!("Could not load poster: {error:#}"), false));
                    }
                } else if let Some(request) = confirm_clear.as_ref() {
                    last_ctrl_c = None;
                    match key.code {
                        KeyCode::Char('y') => {
                            let result = if request.targets.is_empty() {
                                // Only ad-hoc jobs are cleared without files.
                                forget_job(&request.job).map(|()| 0)
                            } else {
                                // Removing items of a configured job must stick:
                                // ignore them so the next sync does not bring them back.
                                clear_tracked(&config, &request.targets, request.ignore).and_then(|removed| {
                                    // Ad-hoc jobs are dropped by clear_tracked once empty;
                                    // recording a status here would resurrect them.
                                    if request.scope == ClearScope::Show && !request.adhoc {
                                        update_job(
                                            &request.job,
                                            "cleared",
                                            &format!("cleared {removed} tracked files"),
                                        )?;
                                    }
                                    Ok(removed)
                                })
                            };
                            sync_notice = Some(match result {
                                Ok(0) => (format!("Removed {}", request.job.trim_start_matches("library:")), true),
                                Ok(_) => (format!("Cleared {}", request.label), true),
                                Err(error) => (format!("Could not clear: {error:#}"), false),
                            });
                            confirm_clear = None;
                        }
                        KeyCode::Char('n') | KeyCode::Esc => confirm_clear = None,
                        _ => {}
                    }
                } else {
                    last_ctrl_c = None;
                    match key.code {
                        // Esc closes dialogs; quitting from the main view takes q or Ctrl-C twice.
                        KeyCode::Char('q') => break,
                        KeyCode::Char('i') | KeyCode::Enter if download_focus && !downloads.is_empty() => {
                            if let Some(entry) = downloads.get(selected_download) {
                                let path = entry.path.clone();
                                let cached = probe_cache
                                    .get(&path)
                                    .and_then(|probe| probe.as_ref().ok())
                                    .map(|probe| Ok(probe.rows.clone()));
                                file_info_entry = Some(entry.clone());
                                file_info = Some((
                                    path.file_name()
                                        .unwrap_or_default()
                                        .to_string_lossy()
                                        .into_owned(),
                                    cached.clone(),
                                ));
                                if let Some(task) = file_info_task.take() {
                                    task.abort();
                                }
                                if cached.is_none() {
                                    file_info_task = Some(tokio::spawn(async move {
                                        probe_media(path).await.map(|probe| probe.rows)
                                    }));
                                }
                                file_meta = None;
                                if let Some(task) = file_meta_task.take() {
                                    task.abort();
                                }
                                if online != Some(Some(false))
                                    && let Some(api) = jellyfin_api.clone()
                                {
                                    file_meta_task = Some(tokio::spawn(jellyfin_file_meta(
                                        api,
                                        entry.item_id.clone(),
                                        picker.clone(),
                                    )));
                                }
                            }
                        }
                        KeyCode::Char(',') => settings = Some(SettingsScreen::open(&config_path)),
                        KeyCode::Char('E') => {
                            sync_notice = Some(edit_config_file(&mut terminal, &config_path, &mut config));
                        }
                        KeyCode::Char('i') if !jobs.is_empty() => {
                            show_job_config = true;
                            editor = JobEditor {
                                overrides: state_config_path()
                                    .map(|overlay| overlay_job_keys(&overlay, &jobs[selected].name))
                                    .unwrap_or_default(),
                                ..JobEditor::default()
                            };
                        }
                        KeyCode::Char('b') if online == Some(Some(false)) => {
                            sync_notice = Some((
                                "Offline: Jellyfin is unreachable, so browsing is unavailable".into(),
                                false,
                            ));
                        }
                        KeyCode::Char('s' | 'S')
                            if online == Some(Some(false)) && config.download.mode == "jellyfin" =>
                        {
                            sync_notice = Some((
                                "Offline: Jellyfin is unreachable, so syncing is unavailable".into(),
                                false,
                            ));
                        }
                        KeyCode::Char('b') => {
                            if let Some((api, items)) = library_cache.clone() {
                                explore = Some(ExploreState {
                                    api: Some(api),
                                    items,
                                    episodes: Vec::new(),
                                    selected: 0,
                                    episode_cursor: 0,
                                    selected_episodes: HashSet::new(),
                                    search: String::new(),
                                    type_filter: ExploreTypeFilter::All,
                                    details_focus: false,
                                    poster_id: None,
                                    poster_loading: false,
                                    poster_loaded: false,
                                    preview_item_id: None,
                                    preview_loading: false,
                                    catalog_loading: false,
                                });
                                explore_notice = None;
                            } else {
                                explore = Some(ExploreState {
                                    api: None,
                                    items: Vec::new(),
                                    episodes: Vec::new(),
                                    selected: 0,
                                    episode_cursor: 0,
                                    selected_episodes: HashSet::new(),
                                    search: String::new(),
                                    type_filter: ExploreTypeFilter::All,
                                    details_focus: false,
                                    poster_id: None,
                                    poster_loading: false,
                                    poster_loaded: false,
                                    preview_item_id: None,
                                    preview_loading: false,
                                    catalog_loading: true,
                                });
                                let config = config.clone();
                                catalog_task = Some(tokio::spawn(async move {
                                    jellyfin_catalog(&config).await
                                }));
                                explore_notice = None;
                            }
                        }
                        KeyCode::PageUp | KeyCode::PageDown | KeyCode::Home | KeyCode::End => {
                            if download_focus {
                                let page = usize::from(downloads_area.height.saturating_sub(2)) / 3;
                                selected_download = list_jump(key.code, selected_download, downloads.len(), page);
                            } else {
                                let page = usize::from(jobs_area.height.saturating_sub(2)) / 4;
                                let next = list_jump(key.code, selected, jobs.len(), page);
                                if next != selected {
                                    selected = next;
                                    selected_download = 0;
                                }
                            }
                        }
                        KeyCode::Tab => download_focus = !download_focus,
                        KeyCode::Left => download_focus = false,
                        KeyCode::Right => download_focus = !downloads.is_empty(),
                        // Wrap within the Files list instead of moving the job selection.
                        KeyCode::Up if download_focus => {
                            selected_download = selected_download
                                .checked_sub(1)
                                .unwrap_or(downloads.len().saturating_sub(1))
                        }
                        KeyCode::Down if download_focus => {
                            selected_download = if selected_download + 1 < downloads.len() {
                                selected_download + 1
                            } else {
                                0
                            }
                        }
                        KeyCode::Up => {
                            let next = selected.saturating_sub(1);
                            if next != selected {
                                selected = next;
                                selected_download = 0;
                            }
                        }
                        KeyCode::Down if selected + 1 < jobs.len() => {
                            selected += 1;
                            selected_download = 0;
                        }
                        KeyCode::Char('s')
                            if jobs.get(selected).is_some_and(|job| job.adhoc) =>
                        {
                            sync_notice = Some((
                                "Ad-hoc downloads are already running in the background".into(),
                                false,
                            ));
                        }
                        KeyCode::Char('s')
                            if jobs.get(selected).is_some_and(|job| !job.adhoc) =>
                        {
                            if sync_task.is_some() {
                                sync_notice = Some(("A sync is already running".into(), false));
                            } else {
                                let target = vec![jobs[selected].name.clone()];
                                let target_name = target[0].clone();
                                let config = config.clone();
                                let parallelism = config.parallelism;
                                sync_notice =
                                    Some((format!("Starting sync for {target_name}"), true));
                                sync_task = Some(tokio::spawn(async move {
                                    run_sync_mode(config, target, parallelism, true).await
                                }));
                            }
                        }
                        KeyCode::Char('S') => {
                            if sync_task.is_some() {
                                sync_notice = Some(("A sync is already running".into(), false));
                            } else {
                                let config = config.clone();
                                let parallelism = config.parallelism;
                                sync_notice = Some(("Starting sync for all jobs".into(), true));
                                sync_task = Some(tokio::spawn(async move {
                                    run_sync_mode(config, Vec::new(), parallelism, true).await
                                }));
                            }
                        }
                        KeyCode::Char(' ') if download_focus && !downloads.is_empty() => {
                            if let Some(entry) = downloads.get(selected_download) {
                                sync_notice = Some(match toggle_pause(entry) {
                                    Ok("paused") => (format!("Paused {}", entry.path.file_name().unwrap_or_default().to_string_lossy()), true),
                                    Ok(_) => match spawn_library_worker(&config_path, &entry.job) {
                                        Ok(()) => (format!("Resumed {}", entry.path.file_name().unwrap_or_default().to_string_lossy()), true),
                                        Err(error) => (format!("Queued again but could not start worker: {error:#}"), false),
                                    },
                                    Err(error) => (format!("Could not pause: {error:#}"), false),
                                });
                            }
                        }
                        // x deletes the file (a later sync may bring it back); X also ignores it.
                        KeyCode::Char('x' | 'X') if download_focus && !downloads.is_empty() => {
                            let ignore = key.code == KeyCode::Char('X');
                            confirm_clear = selected_job.and_then(|job| {
                                clear_request(ClearScope::File, ignore, job, &downloads, selected_download)
                            });
                        }
                        KeyCode::Char('x')
                            if !download_focus
                                && jobs.get(selected).is_some_and(|job| job.adhoc) =>
                        {
                            confirm_clear = selected_job.and_then(|job| {
                                clear_request(ClearScope::Show, false, job, &downloads, selected_download)
                            });
                        }
                        KeyCode::Char('p') if download_focus && !downloads.is_empty() => {
                            if let Some(entry) = downloads.get(selected_download) {
                                sync_notice = match play_download(&config, entry) {
                                    Ok(()) => Some((
                                        format!("Playing {} with {}", entry.path.display(), config.player),
                                        true,
                                    )),
                                    Err(error) => Some((format!("Could not start player: {error:#}"), false)),
                                };
                            }
                        }
                        KeyCode::Char('o') if !jobs.is_empty() => {
                            let job_name = &jobs[selected].name;
                            // With a file focused, open the directory containing it.
                            let file_dir = download_focus
                                .then(|| downloads.get(selected_download))
                                .flatten()
                                .and_then(|entry| entry.path.parent().map(Path::to_path_buf))
                                .filter(|dir| dir.is_dir());
                            sync_notice = Some(
                                match file_dir
                                    .map_or_else(|| job_directory(&config, job_name), Ok)
                                    .and_then(|path| Ok((open_directory(&config, &path)?, path)))
                                {
                                    Ok((command, path)) => (
                                        format!("Opened {} with {command}", path.display()),
                                        true,
                                    ),
                                    Err(error) => {
                                        (format!("Could not open directory: {error:#}"), false)
                                    }
                                },
                            );
                        }
                        // w: the focused file (or the whole job from the Jobs list);
                        // W: the file's season. Marks all watched unless all already are.
                        KeyCode::Char('w' | 'W') if !downloads.is_empty() => {
                            let indices: Vec<usize> = if !download_focus {
                                (0..downloads.len()).collect()
                            } else if key.code == KeyCode::Char('W') {
                                season_files(&rows, &downloads, selected_download)
                            } else {
                                vec![selected_download]
                            };
                            let ids: Vec<String> = indices
                                .iter()
                                .filter_map(|index| downloads.get(*index))
                                .filter(|entry| entry.status != "ignored")
                                .map(|entry| entry.item_id.clone())
                                .collect();
                            let played = !ids.iter().all(|id| {
                                dashboard.watched.get(id).is_some_and(|state| state.played)
                            });
                            sync_notice = Some(match db().and_then(|conn| mark_watched(&conn, &ids, played)) {
                                Ok(()) => {
                                    // Show it right away instead of at the next refresh.
                                    for id in &ids {
                                        dashboard.watched.insert(
                                            id.clone(),
                                            WatchedState { played, pending: true, played_at: None },
                                        );
                                    }
                                    watched_synced_at = None;
                                    (
                                        format!(
                                            "Marked {} {} {}{}",
                                            ids.len(),
                                            if ids.len() == 1 { "file" } else { "files" },
                                            if played { "watched" } else { "unwatched" },
                                            if online == Some(Some(true)) {
                                                ""
                                            } else {
                                                "; syncs to Jellyfin when online"
                                            }
                                        ),
                                        true,
                                    )
                                }
                                Err(error) => (format!("Could not mark watched: {error:#}"), false),
                            });
                        }
                        // I: ignore the focused file (syncs skip it; a downloaded file
                        // is kept) or, when it is ignored, allow it again.
                        KeyCode::Char('I') if download_focus && !downloads.is_empty() => {
                            if let Some(entry) = downloads.get(selected_download) {
                                let ignore = entry.status != "ignored";
                                let name = entry.path.file_name().unwrap_or_default().to_string_lossy().into_owned();
                                sync_notice = Some(match set_ignored(entry, ignore) {
                                    Ok(()) if ignore => (format!("Ignoring {name}; syncs skip it (I again to undo)"), true),
                                    Ok(()) => (format!("{name} is synced again"), true),
                                    Err(error) => (format!("Could not change ignore: {error:#}"), false),
                                });
                            }
                        }
                        KeyCode::Char('c') if !jobs.is_empty() => {
                            confirm_clear = selected_job.and_then(|job| {
                                clear_request(ClearScope::Show, true, job, &downloads, selected_download)
                            });
                            if confirm_clear.is_none() {
                                sync_notice = Some(("Nothing to clear for this job".into(), false));
                            }
                        }
                        _ => {}
                    }
                }
            }
            }
        }
        Ok(())
    }
    .await;
    disable_raw_mode()?;
    execute!(stdout(), DisableMouseCapture, LeaveAlternateScreen)?;
    result
}

fn state_style(state: &str) -> Style {
    let color = match state.to_ascii_lowercase().as_str() {
        "success" | "complete" | "completed" => Color::Green,
        "running" => Color::Yellow,
        "downloading" => Color::Cyan,
        "queued" => Color::Blue,
        "paused" => Color::DarkGray,
        "failed" | "error" | "skipped" => Color::Red,
        "interrupted" | "cleared" => Color::Magenta,
        "not run" | "stopped" => Color::DarkGray,
        _ => Color::Cyan,
    };
    Style::default().fg(color)
}

/// The state to show for a job: a running job is only "downloading" while
/// one of its files actually transfers; otherwise its files wait for a worker
/// slot. Returns the state and, when derived from the files, a summary message.
fn job_display_state<'a>(
    state: &'a str,
    file_states: impl IntoIterator<Item = &'a str>,
) -> (&'a str, Option<String>) {
    if !matches!(state, "running" | "queued") {
        return (state, None);
    }
    let (mut downloading, mut queued) = (0, 0);
    for file_state in file_states {
        match file_state {
            "downloading" => downloading += 1,
            "queued" => queued += 1,
            _ => {}
        }
    }
    let summary = match (downloading, queued) {
        (0, 0) => return (state, None),
        (0, queued) => format!("{queued} queued"),
        (downloading, 0) => format!("{downloading} downloading"),
        (downloading, queued) => format!("{downloading} downloading · {queued} queued"),
    };
    (
        if downloading > 0 {
            "downloading"
        } else {
            "queued"
        },
        Some(summary),
    )
}

/// Nerd Font (Material Design) glyphs; each takes a single terminal cell.
mod icon {
    pub const ALERT: &str = "\u{f0026}"; // md-alert
    pub const AUDIO: &str = "\u{f057e}"; // md-volume_high
    pub const CANCEL: &str = "\u{f073a}"; // md-cancel
    pub const CHECK: &str = "\u{f012c}"; // md-check
    pub const CLOSE: &str = "\u{f0156}"; // md-close
    pub const DELETE: &str = "\u{f01b4}"; // md-delete
    pub const DISK: &str = "\u{f02ca}"; // md-harddisk
    pub const DOWNLOAD: &str = "\u{f01da}"; // md-download
    pub const DOWNLOAD_OUTLINE: &str = "\u{f0b8f}"; // md-download_outline
    pub const IDLE: &str = "\u{f0766}"; // md-circle_outline
    pub const LIBRARY: &str = "\u{f0331}"; // md-library
    pub const OFFLINE: &str = "\u{f0164}"; // md-cloud_off_outline
    pub const ONLINE: &str = "\u{f0160}"; // md-cloud_check
    pub const PAUSE: &str = "\u{f03e4}"; // md-pause
    pub const QUEUED: &str = "\u{f051f}"; // md-timer_sand
    pub const SKIP: &str = "\u{f04ad}"; // md-skip_next
    pub const STAR: &str = "\u{f04ce}"; // md-star
    pub const SUBTITLES: &str = "\u{f0a16}"; // md-subtitles
    pub const SYNC: &str = "\u{f04e6}"; // md-sync
    pub const TIMER_OFF: &str = "\u{f13ac}"; // md-timer_off
    pub const VIDEO: &str = "\u{f0567}"; // md-video
    pub const WATCHED: &str = "\u{f0208}"; // md-eye
}

/// The glyph for a job or file state, shared by `status` and the TUI.
fn state_icon(state: &str) -> &'static str {
    match state.to_ascii_lowercase().as_str() {
        "success" | "complete" | "completed" => icon::CHECK,
        "running" => icon::SYNC,
        "downloading" => icon::DOWNLOAD,
        "queued" => icon::QUEUED,
        "paused" => icon::PAUSE,
        "failed" | "error" => icon::CLOSE,
        "skipped" => icon::SKIP,
        "interrupted" => icon::ALERT,
        "cleared" => icon::DELETE,
        "ignored" => icon::CANCEL,
        _ => icon::IDLE,
    }
}

/// The download glyph, pulsing between filled and outlined twice a second
/// while the TUI redraws.
fn downloading_icon() -> &'static str {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis());
    if (millis / 500).is_multiple_of(2) {
        icon::DOWNLOAD
    } else {
        icon::DOWNLOAD_OUTLINE
    }
}

/// A distinct glyph and colour per job or file state, so downloading, queued,
/// paused and ignored entries tell apart at a glance; downloads are animated.
fn transfer_marker(state: &str) -> (&'static str, Style) {
    let style = match state {
        "complete" | "completed" | "success" => Style::default().fg(Color::Green),
        "downloading" => Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
        "queued" => Style::default().fg(Color::Blue),
        "paused" => Style::default().fg(Color::Yellow),
        "interrupted" => Style::default().fg(Color::Magenta),
        "failed" | "error" => Style::default().fg(Color::Red),
        "ignored" | "skipped" => Style::default().fg(Color::DarkGray),
        _ => state_style(state),
    };
    let glyph = if state == "downloading" {
        downloading_icon()
    } else {
        state_icon(state)
    };
    (glyph, style)
}

fn message_style(state: &str, message: &str) -> Style {
    let lower = message.to_ascii_lowercase();
    if matches!(
        state.to_ascii_lowercase().as_str(),
        "failed" | "error" | "skipped"
    ) || lower.contains("no jellyfin media")
        || lower.contains("jellyfin returned no media")
    {
        Style::default()
            .fg(Color::Red)
            .add_modifier(Modifier::ITALIC)
    } else {
        Style::default().fg(Color::Gray)
    }
}

async fn jellyfin_file_meta(api: JellyfinApi, item_id: String, picker: Picker) -> Result<FileMeta> {
    let item: serde_json::Value = api
        .client
        .get(format!(
            "{}/Users/{}/Items/{}",
            api.base, api.user_id, item_id
        ))
        .header("Authorization", jellyfin_authorization(&api.token))
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .context("query Jellyfin item")?
        .error_for_status()
        .context("Jellyfin item query failed")?
        .json()
        .await
        .context("parse Jellyfin item")?;
    let text = |key: &str| item.get(key).and_then(|v| v.as_str()).map(str::to_string);
    let number = |key: &str| item.get(key).and_then(|v| v.as_f64());
    let mut rows = Vec::new();
    let mut title = Vec::new();
    if let Some(series) = text("SeriesName") {
        title.push(series);
    }
    if let (Some(season), Some(episode)) = (number("ParentIndexNumber"), number("IndexNumber")) {
        title.push(format!("S{:02}E{:02}", season as u32, episode as u32));
    }
    if let Some(name) = text("Name") {
        title.push(name);
    }
    if !title.is_empty() {
        rows.push(("Title".to_string(), title.join(" · ")));
    }
    if let Some(date) = text("PremiereDate") {
        rows.push(("Aired".to_string(), date.chars().take(10).collect()));
    }
    let mut rating = Vec::new();
    if let Some(score) = number("CommunityRating") {
        rating.push(format!("{} {score:.1}", icon::STAR));
    }
    if let Some(official) = text("OfficialRating") {
        rating.push(official);
    }
    if !rating.is_empty() {
        rows.push(("Rating".to_string(), rating.join("  ")));
    }
    if let Some(ticks) = number("RunTimeTicks") {
        rows.push((
            "Runtime".to_string(),
            format!("{} min", (ticks / 600_000_000.0).round()),
        ));
    }
    if let Some(played) = item
        .get("UserData")
        .and_then(|data| data.get("Played"))
        .and_then(|v| v.as_bool())
    {
        rows.push((
            "Watched".to_string(),
            if played { "yes" } else { "no" }.to_string(),
        ));
    }
    let overview = text("Overview").unwrap_or_default();
    let still = match serde_json::from_value::<MediaItem>(item) {
        Ok(media) => fetch_poster_protocol(api, media, picker, None)
            .await
            .ok()
            .flatten()
            .map(|(_, protocol)| protocol),
        Err(_) => None,
    };
    Ok((rows, overview, still))
}

async fn jellyfin_job_details(
    api: JellyfinApi,
    item: MediaItem,
    picker: Picker,
    cached_poster: Option<Vec<u8>>,
) -> Result<JobDetails> {
    let details: serde_json::Value = api
        .client
        .get(format!(
            "{}/Users/{}/Items/{}",
            api.base, api.user_id, item.id
        ))
        .header("Authorization", jellyfin_authorization(&api.token))
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .context("query Jellyfin item")?
        .error_for_status()
        .context("Jellyfin item query failed")?
        .json()
        .await
        .context("parse Jellyfin item")?;
    let text = |key: &str| {
        details
            .get(key)
            .and_then(|v| v.as_str())
            .map(str::to_string)
    };
    let number = |key: &str| details.get(key).and_then(|v| v.as_f64());
    let series = text("Type").as_deref() == Some("Series");
    let years = number("ProductionYear").map(|year| {
        let year = year as u32;
        if !series {
            return year.to_string();
        }
        match (text("Status").as_deref(), text("EndDate")) {
            (Some("Continuing"), _) => format!("{year}–"),
            (_, Some(end)) if !end.starts_with(&year.to_string()) => {
                format!("{year}–{}", end.chars().take(4).collect::<String>())
            }
            _ => year.to_string(),
        }
    });
    let mut facts = Vec::new();
    if let Some(score) = number("CommunityRating") {
        facts.push(format!("{} {score:.1}", icon::STAR));
    }
    if let Some(rating) = text("OfficialRating") {
        facts.push(rating);
    }
    if let Some(minutes) = number("RunTimeTicks")
        .map(|ticks| (ticks / 600_000_000.0).round() as u64)
        .filter(|minutes| *minutes > 0)
    {
        facts.push(if series {
            format!("{minutes} min/ep")
        } else if minutes >= 60 {
            format!("{}h {:02}m", minutes / 60, minutes % 60)
        } else {
            format!("{minutes} min")
        });
    }
    if series && let Some(seasons) = number("ChildCount") {
        let seasons = seasons as u64;
        facts.push(format!(
            "{seasons} {}",
            if seasons == 1 { "season" } else { "seasons" }
        ));
    }
    let genres: Vec<&str> = details
        .get("Genres")
        .and_then(|v| v.as_array())
        .map(|genres| genres.iter().filter_map(|g| g.as_str()).take(3).collect())
        .unwrap_or_default();
    if !genres.is_empty() {
        facts.push(genres.join(", "));
    }
    let tagline = details
        .get("Taglines")
        .and_then(|v| v.as_array())
        .and_then(|taglines| taglines.first())
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let mut episodes = HashMap::new();
    if series {
        // Episode names are a nicety; the Files list falls back to file names.
        for episode in jellyfin_episodes(&api, &item).await.unwrap_or_default() {
            episodes.insert(
                episode.id,
                EpisodeInfo {
                    season: episode.parent_index_number,
                    episode: episode.index_number,
                    name: episode.name,
                },
            );
        }
    }
    let poster = fetch_poster_protocol(api, item.clone(), picker, cached_poster)
        .await
        .ok()
        .flatten()
        .map(|(_, protocol)| protocol);
    Ok(JobDetails {
        series,
        title: text("Name").unwrap_or(item.name),
        years,
        facts,
        tagline,
        overview: text("Overview").unwrap_or_default(),
        episodes,
        poster,
    })
}

/// Season number from a "Season 3"-style folder or an "S03E07" file name tag.
fn parse_season(path: &Path) -> Option<u32> {
    let folder = path.parent()?.file_name()?.to_string_lossy().to_lowercase();
    if folder == "specials" {
        return Some(0);
    }
    if let Some(number) = folder.strip_prefix("season") {
        return number.trim().parse().ok();
    }
    let name = path.file_name()?.to_string_lossy().to_uppercase();
    let bytes = name.as_bytes();
    (0..bytes.len()).find_map(|start| {
        if bytes[start] != b'S' || (start > 0 && bytes[start - 1].is_ascii_alphanumeric()) {
            return None;
        }
        let digits: String = name[start + 1..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        let rest = &name[start + 1 + digits.len()..];
        (!digits.is_empty() && digits.len() <= 3 && rest.starts_with('E'))
            .then(|| digits.parse().ok())
            .flatten()
    })
}

/// Display title for a file: an episode tag and name, or a lone movie's title.
fn file_title(
    details: Option<&JobDetails>,
    entry: &DownloadEntry,
    single: bool,
) -> Option<(String, String)> {
    let details = details?;
    if let Some(info) = details.episodes.get(&entry.item_id) {
        let tag = match (info.season, info.episode) {
            (Some(season), Some(number)) => format!("S{season:02}E{number:02}"),
            (None, Some(number)) => format!("E{number:02}"),
            _ => String::new(),
        };
        return Some((tag, info.name.clone()));
    }
    (!details.series && single).then(|| {
        let title = match &details.years {
            Some(years) => format!("{} ({years})", details.title),
            None => details.title.clone(),
        };
        (String::new(), title)
    })
}

fn season_label(season: Option<u32>, folder: &str) -> String {
    match season {
        Some(0) => "Specials".into(),
        Some(number) => format!("Season {number}"),
        None if folder.is_empty() => "Other files".into(),
        None => folder.to_string(),
    }
}

/// Order files by season and episode and interleave season headings. Headings
/// are only added when the files look like a show (seasons or several folders).
fn file_rows(downloads: &mut [DownloadEntry], details: Option<&JobDetails>) -> Vec<FileRow> {
    let season_of = |entry: &DownloadEntry| {
        details
            .and_then(|details| details.episodes.get(&entry.item_id))
            .and_then(|info| info.season)
            .or_else(|| parse_season(&entry.path))
    };
    let episode_of = |entry: &DownloadEntry| {
        details
            .and_then(|details| details.episodes.get(&entry.item_id))
            .and_then(|info| info.episode)
    };
    let folders: HashSet<_> = downloads.iter().map(|entry| entry.path.parent()).collect();
    let grouped = details.is_some_and(|details| details.series)
        || folders.len() > 1
        || downloads.iter().any(|entry| season_of(entry).is_some());
    if !grouped {
        return (0..downloads.len()).map(FileRow::File).collect();
    }
    // The show (or movie) folder: above the season folder for episodes. Auto
    // jobs mix several; then headings name the show and shows sort together.
    let show_of = |entry: &DownloadEntry| {
        let folder = entry.path.parent()?;
        let show = if season_of(entry).is_some() {
            folder.parent()?
        } else {
            folder
        };
        Some(show.file_name()?.to_string_lossy().into_owned())
    };
    let shows: HashSet<_> = downloads.iter().map(show_of).collect();
    let several_shows = shows.len() > 1;
    downloads.sort_by_cached_key(|entry| {
        (
            several_shows.then(|| show_of(entry)).flatten(),
            season_of(entry).unwrap_or(u32::MAX),
            entry.path.parent().map(Path::to_path_buf),
            episode_of(entry).unwrap_or(u32::MAX),
            entry.path.clone(),
        )
    });
    let mut rows = Vec::new();
    let mut current = None;
    for (index, entry) in downloads.iter().enumerate() {
        let key = (season_of(entry), entry.path.parent().map(Path::to_path_buf));
        if current.as_ref() != Some(&key) {
            let folder = entry
                .path
                .parent()
                .and_then(Path::file_name)
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            let label = match (several_shows, key.0, show_of(entry)) {
                (true, Some(_), Some(show)) => format!("{show} · {}", season_label(key.0, &folder)),
                _ => season_label(key.0, &folder),
            };
            rows.push(FileRow::Season(label));
            current = Some(key);
        }
        rows.push(FileRow::File(index));
    }
    rows
}

/// Poster, title, facts, the selected file's media info and the overview.
#[allow(clippy::too_many_arguments)]
fn render_job_details(
    frame: &mut ratatui::Frame,
    area: Rect,
    job_name: &str,
    details: Option<&mut JobDetails>,
    loading: bool,
    last_sync: Option<(String, String)>,
    file_label: Option<String>,
    file_watched: Option<String>,
    probe: Option<&std::result::Result<MediaProbe, String>>,
) {
    let block = panel_block("Details", false);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width < 10 || inner.height < 2 {
        return;
    }
    let (title, years, facts, tagline, overview, series, poster) = match details {
        Some(details) => (
            details.title.clone(),
            details.years.clone(),
            details.facts.clone(),
            details.tagline.clone(),
            details.overview.clone(),
            Some(details.series),
            details.poster.as_mut(),
        ),
        None => (
            job_name.trim_start_matches("library:").to_string(),
            None,
            Vec::new(),
            None,
            String::new(),
            None,
            None,
        ),
    };
    // A 2:3 poster at 1:2 cells is about 4/3 as wide as it is tall.
    let poster_width = if inner.width >= 56 && poster.is_some() {
        (inner.height * 4 / 3 + 1).min(inner.width / 3)
    } else {
        0
    };
    if let Some(poster) = poster
        && poster_width > 0
    {
        frame.render_stateful_widget(
            StatefulImage::default().resize(Resize::Fit(Some(FilterType::Triangle))),
            Rect {
                width: poster_width.saturating_sub(1),
                ..inner
            },
            poster,
        );
    }
    let text_area = Rect {
        x: inner.x + poster_width + u16::from(poster_width > 0),
        width: inner
            .width
            .saturating_sub(poster_width + u16::from(poster_width > 0)),
        ..inner
    };
    let width = usize::from(text_area.width);
    let mut lines = Vec::new();
    let mut title_spans = vec![Span::styled(
        title,
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )];
    if let Some(years) = years {
        title_spans.push(Span::styled(
            format!("  {years}"),
            Style::default().fg(Color::Gray),
        ));
    }
    lines.push(Line::from(title_spans));
    let mut fact_spans = Vec::new();
    match series {
        Some(true) => fact_spans.push(Span::styled("SERIES", Style::default().fg(Color::Magenta))),
        Some(false) => fact_spans.push(Span::styled("MOVIE", Style::default().fg(Color::Magenta))),
        None if loading => fact_spans.push(Span::styled(
            "loading details…",
            Style::default().fg(Color::DarkGray),
        )),
        None => {}
    }
    for fact in facts {
        if !fact_spans.is_empty() {
            fact_spans.push(Span::styled(" · ", Style::default().fg(Color::DarkGray)));
        }
        fact_spans.push(Span::styled(
            fact.clone(),
            if fact.starts_with(icon::STAR) {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default().fg(Color::White)
            },
        ));
    }
    if !fact_spans.is_empty() {
        lines.push(Line::from(fact_spans));
    }
    if let Some(tagline) = tagline {
        lines.push(Line::from(Span::styled(
            truncate(&tagline, width),
            Style::default()
                .fg(Color::Gray)
                .add_modifier(Modifier::ITALIC),
        )));
    }
    if let Some((state, age)) = last_sync {
        lines.push(Line::from(vec![
            Span::styled("Last sync  ", Style::default().fg(Color::Cyan)),
            Span::styled(age, Style::default().fg(Color::White)),
            Span::styled(" · ", Style::default().fg(Color::DarkGray)),
            Span::styled(state.to_lowercase(), state_style(&state)),
        ]));
    }
    if let Some(label) = file_label {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("▸ ", Style::default().fg(Color::Cyan)),
            Span::styled(
                truncate_near_end(&label, width.saturating_sub(2)),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
        if let Some(watched) = file_watched {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("  {:<11}", "Watched"),
                    Style::default().fg(Color::Cyan),
                ),
                Span::raw(truncate(&watched, width.saturating_sub(13))),
            ]));
        }
        match probe {
            Some(Ok(probe)) => {
                // Streams first; the container line matters least.
                let order = ["Video", "Audio", "Subtitles", "File"];
                for label in order {
                    for (name, value) in probe.rows.iter().filter(|(name, _)| name == label) {
                        lines.push(Line::from(vec![
                            Span::styled(format!("  {name:<11}"), Style::default().fg(Color::Cyan)),
                            Span::raw(truncate(value, width.saturating_sub(13))),
                        ]));
                    }
                }
            }
            Some(Err(error)) => lines.push(Line::from(Span::styled(
                format!("  {}", truncate(error, width.saturating_sub(2))),
                Style::default().fg(Color::DarkGray),
            ))),
            None => lines.push(Line::from(Span::styled(
                "  probing…",
                Style::default().fg(Color::DarkGray),
            ))),
        }
    }
    let used = lines.len() as u16;
    frame.render_widget(Paragraph::new(lines), text_area);
    // The overview fills whatever space is left below.
    if !overview.is_empty() && text_area.height > used + 1 {
        frame.render_widget(
            Paragraph::new(overview)
                .wrap(Wrap { trim: true })
                .style(Style::default().fg(Color::Gray)),
            Rect {
                y: text_area.y + used + 1,
                height: text_area.height - used - 1,
                ..text_area
            },
        );
    }
}

async fn probe_media(path: PathBuf) -> Result<MediaProbe> {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_format",
            "-show_streams",
            "-of",
            "json",
        ])
        .arg(&path)
        .stdin(Stdio::null())
        .output()
        .await
        .context("run ffprobe")?;
    if !output.status.success() {
        bail!(
            "ffprobe failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let probe: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("parse ffprobe output")?;
    Ok(MediaProbe {
        rows: media_summary(&probe),
        badges: media_badges(&probe),
    })
}

/// Common name for a video resolution, e.g. "1080p" or "4K".
fn resolution_label(width: u64, height: u64) -> String {
    // Widescreen encodes crop the height, so also look at the width.
    if width >= 3800 || height >= 2100 {
        "4K".into()
    } else if width >= 2500 || height >= 1400 {
        "1440p".into()
    } else if width >= 1900 || height >= 1000 {
        "1080p".into()
    } else if width >= 1200 || height >= 700 {
        "720p".into()
    } else if height > 0 {
        format!("{height}p")
    } else {
        "SD".into()
    }
}

fn channel_label(channels: Option<u64>) -> String {
    match channels {
        Some(1) => "mono".into(),
        Some(2) => "2.0".into(),
        Some(6) => "5.1".into(),
        Some(8) => "7.1".into(),
        Some(n) => format!("{n}ch"),
        None => "?".into(),
    }
}

/// Short tags for a Files row: resolution, video codec, HDR, main audio track.
fn media_badges(probe: &serde_json::Value) -> Vec<String> {
    let streams = probe
        .get("streams")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let text = |value: &serde_json::Value, key: &str| {
        value.get(key).and_then(|v| v.as_str()).map(str::to_string)
    };
    let number = |value: &serde_json::Value, key: &str| value.get(key).and_then(|v| v.as_u64());
    let disposition = |stream: &serde_json::Value, key: &str| {
        stream
            .get("disposition")
            .and_then(|d| d.get(key))
            .and_then(|v| v.as_i64())
            == Some(1)
    };
    let of_type = |kind: &str| -> Vec<&serde_json::Value> {
        streams
            .iter()
            .filter(|stream| text(stream, "codec_type").as_deref() == Some(kind))
            .filter(|stream| !disposition(stream, "attached_pic"))
            .collect()
    };
    // Only what tells files apart at a glance; audio, subtitles and HDR are
    // in the Details panel.
    let mut badges = Vec::new();
    if let Some(video) = of_type("video").first() {
        badges.push(format!(
            "{} {}",
            icon::VIDEO,
            resolution_label(
                number(video, "width").unwrap_or(0),
                number(video, "height").unwrap_or(0),
            )
        ));
        if let Some(codec) = text(video, "codec_name") {
            badges.push(match codec.as_str() {
                "hevc" => "HEVC".to_string(),
                "h264" => "H.264".to_string(),
                other => other.to_uppercase(),
            });
        }
    }
    badges
}

/// Codecs in distinct colours so e.g. H.264 stands out next to HEVC.
fn codec_color(codec: &str) -> Color {
    match codec {
        "HEVC" => Color::Green,
        "H.264" => Color::Yellow,
        "AV1" => Color::Magenta,
        _ => Color::Gray,
    }
}

fn media_summary(probe: &serde_json::Value) -> MediaSummary {
    let text = |value: &serde_json::Value, key: &str| {
        value.get(key).and_then(|v| v.as_str()).map(str::to_string)
    };
    let number = |value: &serde_json::Value, key: &str| {
        value.get(key).and_then(|v| {
            v.as_f64()
                .or_else(|| v.as_str().and_then(|v| v.parse().ok()))
        })
    };
    let language = |stream: &serde_json::Value| {
        stream
            .get("tags")
            .and_then(|tags| text(tags, "language"))
            .unwrap_or_else(|| "und".into())
    };
    let flag = |stream: &serde_json::Value, key: &str| {
        stream
            .get("disposition")
            .and_then(|d| d.get(key))
            .and_then(|v| v.as_i64())
            == Some(1)
    };
    let format = probe.get("format").cloned().unwrap_or_default();
    let mut rows = Vec::new();

    let container = text(&format, "format_name")
        .and_then(|name| name.split(',').next().map(str::to_string))
        .unwrap_or_else(|| "?".into());
    let mut file = vec![container];
    if let Some(duration) = number(&format, "duration") {
        let seconds = duration as u64;
        file.push(format!(
            "{}:{:02}:{:02}",
            seconds / 3600,
            seconds / 60 % 60,
            seconds % 60
        ));
    }
    if let Some(size) = number(&format, "size") {
        file.push(format_bytes(size as u64));
    }
    if let Some(bitrate) = number(&format, "bit_rate") {
        file.push(format!("{:.1} Mb/s", bitrate / 1_000_000.0));
    }
    rows.push(("File".to_string(), file.join(", ")));

    let streams = probe
        .get("streams")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let of_type = |kind: &str| -> Vec<&serde_json::Value> {
        streams
            .iter()
            .filter(|stream| text(stream, "codec_type").as_deref() == Some(kind))
            .filter(|stream| !flag(stream, "attached_pic"))
            .collect()
    };
    for stream in of_type("video") {
        let mut parts = vec![
            text(stream, "codec_name")
                .unwrap_or_else(|| "?".into())
                .to_uppercase(),
        ];
        if let (Some(width), Some(height)) = (number(stream, "width"), number(stream, "height")) {
            parts.push(format!("{width}x{height}"));
        }
        if let Some(rate) = text(stream, "avg_frame_rate").and_then(|rate| {
            let (num, den) = rate.split_once('/')?;
            let (num, den): (f64, f64) = (num.parse().ok()?, den.parse().ok()?);
            (den > 0.0 && num > 0.0).then(|| num / den)
        }) {
            parts.push(format!("{rate:.3} fps").replace(".000", ""));
        }
        if text(stream, "pix_fmt").is_some_and(|fmt| fmt.contains("10")) {
            parts.push("10-bit".into());
        }
        match text(stream, "color_transfer").as_deref() {
            Some("smpte2084") => parts.push("HDR10".into()),
            Some("arib-std-b67") => parts.push("HLG".into()),
            _ => {}
        }
        rows.push((
            "Video".to_string(),
            format!("{} {}", icon::VIDEO, parts.join(", ")),
        ));
    }
    let audio: Vec<String> = of_type("audio")
        .into_iter()
        .map(|stream| {
            let channels = channel_label(number(stream, "channels").map(|c| c as u64));
            format!(
                "{} {} {}{}",
                language(stream),
                text(stream, "codec_name")
                    .unwrap_or_else(|| "?".into())
                    .to_uppercase(),
                channels,
                if flag(stream, "default") { "*" } else { "" }
            )
        })
        .collect();
    if !audio.is_empty() {
        rows.push((
            "Audio".to_string(),
            format!("{} {}", icon::AUDIO, audio.join(", ")),
        ));
    }
    let subtitles: Vec<String> = of_type("subtitle")
        .into_iter()
        .map(|stream| {
            format!(
                "{}{}",
                language(stream),
                if flag(stream, "forced") {
                    " (forced)"
                } else {
                    ""
                }
            )
        })
        .collect();
    if !subtitles.is_empty() {
        rows.push((
            "Subtitles".to_string(),
            format!("{} {}", icon::SUBTITLES, subtitles.join(", ")),
        ));
    }
    rows
}

fn config_line(label: &str, value: impl Into<String>) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("{label:<14}"),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(value.into()),
    ])
}

fn panel_block(title: &str, focused: bool) -> Block<'_> {
    let color = if focused {
        Color::Cyan
    } else {
        Color::DarkGray
    };
    Block::default()
        .title(Span::styled(
            format!(" {title} "),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(color))
}

fn key_hint(key: &'static str, label: &'static str) -> [Span<'static>; 2] {
    [
        // Pad the key on both sides so its background block is centred.
        Span::styled(
            format!(" {key} "),
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" {label}"), Style::default().fg(Color::Gray)),
    ]
}

fn key_sep() -> Span<'static> {
    Span::raw("  ")
}

/// A progress bar as its filled part, its empty track and the percentage, so
/// callers can colour the fill and the track apart. The fill uses eighth
/// blocks for sub-cell precision.
fn download_progress(bytes: u64, total: Option<u64>, width: usize) -> (String, String, String) {
    const PARTIAL: [&str; 7] = ["▏", "▎", "▍", "▌", "▋", "▊", "▉"];
    let Some(total) = total.filter(|total| *total > 0) else {
        return (String::new(), "░".repeat(width), "   ?".into());
    };
    let percent = ((bytes as f64 / total as f64) * 100.0).clamp(0.0, 100.0);
    let eighths = ((percent / 100.0) * (width * 8) as f64).round() as usize;
    let mut filled = "█".repeat(eighths / 8);
    let mut cells = eighths / 8;
    if !eighths.is_multiple_of(8) {
        filled.push_str(PARTIAL[eighths % 8 - 1]);
        cells += 1;
    }
    (
        filled,
        "░".repeat(width.saturating_sub(cells)),
        format!("{percent:3.0}%"),
    )
}

/// How often a running download records its progress and throughput.
const PROGRESS_INTERVAL: Duration = Duration::from_secs(1);

/// The throughput of a transfer only when it is actually running: a rate that
/// has not been refreshed recently belongs to a worker that died.
const LIVE_RATE_SQL: &str =
    "CASE WHEN status='downloading' AND updated_at >= datetime('now','-15 seconds') THEN rate END";

/// "12.3 MiB/s · 4m left" for a running transfer.
fn format_throughput(rate: u64, bytes: u64, total: Option<u64>) -> String {
    let mut text = format!("{}/s", format_bytes(rate));
    if let Some(total) = total
        && rate > 0
        && total > bytes
    {
        text.push_str(&format!(
            " · {} left",
            format_duration_short((total - bytes).div_ceil(rate))
        ));
    }
    text
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

fn play_download(config: &Config, entry: &DownloadEntry) -> Result<()> {
    if config.player.trim().is_empty() {
        bail!("player command is empty; set player in jellysync settings");
    }
    std::process::Command::new(&config.player)
        .arg(&entry.path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("start player '{}'", config.player))?;
    Ok(())
}

fn job_directory(config: &Config, job_name: &str) -> Result<PathBuf> {
    if let Some(title) = job_name.strip_prefix("library:") {
        let root = PathBuf::from(&config.local.root);
        return ["TV Shows", "Movies"]
            .iter()
            .map(|kind| root.join(kind).join(safe_component(title)))
            .find(|path| path.is_dir())
            .with_context(|| format!("no download directory for {title}"));
    }
    let job = config
        .jobs
        .iter()
        .find(|job| job.name == job_name)
        .with_context(|| format!("unknown job {job_name}"))?;
    // Auto jobs spread over Movies/ and TV Shows/ under the download root.
    if job.is_auto() {
        return Ok(PathBuf::from(&config.local.root));
    }
    let path = PathBuf::from(expand_home(&resolved_path(config, job, false)?));
    if !path.is_dir() {
        bail!("{} does not exist yet", path.display());
    }
    Ok(path)
}

fn in_path(program: &str) -> bool {
    env::var_os("PATH")
        .is_some_and(|paths| env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
}

fn open_directory(config: &Config, path: &Path) -> Result<String> {
    let command: Vec<String> = match config.file_manager.as_deref().map(str::trim) {
        Some(command) if !command.is_empty() => {
            command.split_whitespace().map(String::from).collect()
        }
        _ if in_path("xdg-open") => vec!["xdg-open".into()],
        _ if in_path("gio") => vec!["gio".into(), "open".into()],
        _ => bail!("neither xdg-open nor gio found; set file_manager in jellysync settings"),
    };
    let label = command.join(" ");
    std::process::Command::new(&command[0])
        .args(&command[1..])
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("start file manager '{label}'"))?;
    Ok(label)
}

fn truncate(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        value.to_string()
    } else {
        format!(
            "{}…",
            value
                .chars()
                .take(max_chars.saturating_sub(1))
                .collect::<String>()
        )
    }
}

/// Ellipsize near the end so the start of the name and the tail (episode tag and
/// extension, e.g. "S01E05.mkv") both stay visible.
fn truncate_near_end(value: &str, max_chars: usize) -> String {
    const TAIL_CHARS: usize = 12;
    let chars: Vec<char> = value.chars().collect();
    if chars.len() <= max_chars {
        return value.to_string();
    }
    let keep = max_chars.saturating_sub(1);
    let tail = TAIL_CHARS.min(keep / 2);
    let head = keep - tail;
    let mut out: String = chars[..head].iter().collect();
    out.push('…');
    out.extend(&chars[chars.len() - tail..]);
    out
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let width = width.min(area.width.saturating_sub(2));
    let height = height.min(area.height.saturating_sub(2));
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect {
        x,
        y,
        width,
        height,
    }
}

/// Swallows late terminal replies that crossterm hands us as keystrokes.
///
/// The startup image-protocol query gives up after a second; when kitty's answer
/// (`ESC _Gi=31;OK ESC \`, often slow through tmux) arrives later, it would be
/// read as typed keys and its `i` opened the job configuration. crossterm
/// reports the APC introducer `ESC _` as Alt+`_` and the terminator `ESC \` as
/// Alt+`\`; everything in between belongs to the reply.
#[derive(Default)]
struct ReplyFilter {
    since: Option<Instant>,
}

impl ReplyFilter {
    /// A reply arrives in one burst; give up on a missing terminator after this.
    const TIMEOUT: Duration = Duration::from_millis(500);

    fn swallow(&mut self, key: &crossterm::event::KeyEvent) -> bool {
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        if let Some(since) = self.since {
            if since.elapsed() <= Self::TIMEOUT {
                if (alt && key.code == KeyCode::Char('\\')) || key.code == KeyCode::Esc {
                    self.since = None;
                }
                return true;
            }
            self.since = None;
        }
        if alt && key.code == KeyCode::Char('_') {
            self.since = Some(Instant::now());
            return true;
        }
        false
    }
}

/// New index after PgUp/PgDn/Home/End in a list of `len` items.
fn list_jump(code: KeyCode, index: usize, len: usize, page: usize) -> usize {
    let last = len.saturating_sub(1);
    match code {
        KeyCode::Home => 0,
        KeyCode::End => last,
        KeyCode::PageUp => index.saturating_sub(page.max(1)),
        KeyCode::PageDown => (index + page.max(1)).min(last),
        _ => index,
    }
}

/// Seconds since the epoch for an SQLite `datetime('now')` value (UTC,
/// `YYYY-MM-DD HH:MM:SS`).
fn sqlite_utc_seconds(value: &str) -> Option<i64> {
    let (date, time) = value.trim().split_once(' ')?;
    let mut date = date.splitn(3, '-').map(str::parse::<i64>);
    let (year, month, day) = (date.next()?.ok()?, date.next()?.ok()?, date.next()?.ok()?);
    let mut time = time.splitn(3, ':').map(str::parse::<i64>);
    let (hour, minute, second) = (time.next()?.ok()?, time.next()?.ok()?, time.next()?.ok()?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    // Days from the civil date (Howard Hinnant's algorithm).
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    Some(days * 86_400 + hour * 3600 + minute * 60 + second)
}

/// "just now", "5 min ago", "3 h ago", "2 d ago" for a status timestamp.
fn status_age(value: &str) -> Option<String> {
    let then = sqlite_utc_seconds(value)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs() as i64;
    Some(format_age(now - then))
}

fn format_age(seconds: i64) -> String {
    match seconds.max(0) {
        0..60 => "just now".into(),
        seconds @ 60..3600 => format!("{} min ago", seconds / 60),
        seconds @ 3600..86_400 => format!("{} h ago", seconds / 3600),
        seconds => format!("{} d ago", seconds / 86_400),
    }
}

/// A path with the home directory shown as `~`.
fn tilde_path(path: &Path) -> String {
    match dirs::home_dir().and_then(|home| path.strip_prefix(home).ok().map(Path::to_path_buf)) {
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// The watched marker for a file row: "watched", whether the mark still has to
/// reach Jellyfin, and when cleanup deletes the file (for `delete_watched` jobs).
fn watched_label(
    state: Option<&WatchedState>,
    grace: Option<Duration>,
    now: i64,
) -> Option<(String, bool)> {
    let state = state.filter(|state| state.played)?;
    let mut label = format!("{} watched", icon::WATCHED);
    if state.pending {
        label.push_str(" (not synced)");
    }
    let mut soon = false;
    if let Some(grace) = grace {
        match cleanup_remaining(state.played_at.as_deref(), grace, now) {
            0 => {
                label.push_str(" · deleted at next sync");
                soon = true;
            }
            remaining => label.push_str(&format!(
                " · deleted in {}",
                format_duration_short(remaining)
            )),
        }
    }
    Some((label, soon))
}

fn download_complete(status: &str) -> bool {
    matches!(status, "complete" | "completed" | "success")
}

/// Whether `path` lives strictly inside one of `roots` (compared via its
/// canonical parent directory so symlinked files are judged by their location).
fn path_within_roots(path: &Path, roots: &[PathBuf]) -> Result<bool> {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Ok(false);
    };
    if name == ".." {
        return Ok(false);
    }
    let parent = parent
        .canonicalize()
        .with_context(|| format!("resolve {}", parent.display()))?;
    Ok(roots.iter().any(|root| parent.starts_with(root)))
}

fn file_row_height(row: &FileRow) -> usize {
    match row {
        FileRow::Season(_) => 1,
        FileRow::File(_) => 3,
    }
}

/// The file under a mouse position in the Files list, skipping season headings.
fn file_row_at(
    area: Rect,
    column: u16,
    row: u16,
    offset: usize,
    rows: &[FileRow],
) -> Option<usize> {
    if column < area.x
        || column >= area.x + area.width
        || row < area.y + 1
        || row >= area.y + area.height.saturating_sub(1)
    {
        return None;
    }
    let mut line = usize::from(row - area.y - 1);
    for file_row in rows.iter().skip(offset) {
        let height = file_row_height(file_row);
        if line < height {
            return match file_row {
                FileRow::File(index) => Some(*index),
                FileRow::Season(_) => None,
            };
        }
        line -= height;
    }
    None
}

fn list_index_at(
    area: Rect,
    column: u16,
    row: u16,
    offset: usize,
    len: usize,
    row_height: usize,
) -> Option<usize> {
    if column < area.x
        || column >= area.x + area.width
        || row < area.y + 1
        || row >= area.y + area.height.saturating_sub(1)
    {
        return None;
    }
    let index = offset + usize::from(row - area.y - 1) / row_height;
    (index < len).then_some(index)
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    // Plain output for pipes and journald; crossterm already honours NO_COLOR on a TTY.
    if !std::io::IsTerminal::is_terminal(&stdout()) {
        crossterm::style::force_color_output(false);
    }
    match cli.command {
        Some(Commands::Status) => {
            // Best effort: status is also for when the config is broken.
            let config = config_path(cli.config)
                .ok()
                .and_then(|path| load_config(&path).ok());
            status(cli.json, config.as_ref())
        }
        // Needs no config, so packaging can run it in a build sandbox.
        Some(Commands::Completions { shell }) => {
            use std::io::Write as _;
            // clap_complete panics when stdout closes early (`| head`); write
            // the script ourselves and treat a closed pipe as done.
            let mut script = Vec::new();
            clap_complete::generate(shell, &mut completion_command(), "jellysync", &mut script);
            match stdout().write_all(&script) {
                Err(error) if error.kind() != std::io::ErrorKind::BrokenPipe => Err(error.into()),
                _ => Ok(()),
            }
        }
        Some(Commands::Version) => {
            if cli.json {
                println!(
                    "{}",
                    json!({
                        "version": version_string(),
                        "release": env!("CARGO_PKG_VERSION"),
                        "revision": (!GIT_REVISION.is_empty()).then_some(GIT_REVISION),
                    })
                );
            } else {
                println!("jellysync {}", version_string());
            }
            Ok(())
        }
        Some(Commands::Start) => systemctl("start", cli.json),
        Some(Commands::Stop) => systemctl("stop", cli.json),
        None | Some(Commands::Tui) if cli.json => bail!("--json is not supported with tui"),
        None | Some(Commands::Tui) => {
            let path = config_path(cli.config)?;
            let config =
                load_config(&path).with_context(|| format!("load config {}", path.display()))?;
            tui(config, path).await
        }
        command => {
            let path = config_path(cli.config)?;
            let config =
                load_config(&path).with_context(|| format!("load config {}", path.display()))?;
            let parallelism = cli.parallelism.unwrap_or(config.parallelism);
            if parallelism == 0 {
                bail!("parallelism must be at least 1");
            }
            match command {
                Some(Commands::Download {
                    target,
                    max_items,
                    max_size,
                }) => {
                    let config = with_auto_target(config, &target, max_items, max_size)?;
                    let result =
                        run_sync_mode(config.clone(), target.clone(), parallelism, cli.json).await;
                    if target.is_empty() {
                        finish_library_jobs(&config).await;
                    }
                    if cli.json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&json!({
                                "command": "download",
                                "targets": target,
                                "ok": result.is_ok(),
                                "error": result.as_ref().err().map(|error| format!("{error:#}")),
                            }))?
                        );
                    }
                    result
                }
                Some(Commands::Prune {
                    dry_run,
                    watched,
                    apply: _,
                    target,
                }) => {
                    let result = prune(config, target.clone(), dry_run, watched, cli.json).await;
                    if cli.json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&json!({
                                "command": "prune",
                                "targets": target,
                                "dry_run": dry_run,
                                "ok": result.is_ok(),
                                "pruned": result.as_ref().ok(),
                                "error": result.as_ref().err().map(|error| format!("{error:#}")),
                            }))?
                        );
                    }
                    result.map(|_| ())
                }
                Some(Commands::Config) => {
                    if cli.json {
                        println!("{}", serde_json::to_string_pretty(&config)?);
                    } else {
                        println!("{}", serde_yaml::to_string(&config)?);
                    }
                    Ok(())
                }
                Some(Commands::Worker { job }) => library_worker(&config, &job).await,
                None
                | Some(
                    Commands::Status
                    | Commands::Start
                    | Commands::Stop
                    | Commands::Tui
                    | Commands::Version
                    | Commands::Completions { .. },
                ) => {
                    unreachable!()
                }
            }
        }
    }
}
fn systemctl(action: &str, json_output: bool) -> Result<()> {
    let status = std::process::Command::new("systemctl")
        .args(["--user", action, "jellysync.service"])
        .status()?;
    if !status.success() {
        bail!("systemctl {action} jellysync.service failed with {status}");
    }
    if json_output {
        println!(
            "{}",
            serde_json::to_string(&json!({
                "command": action,
                "service": "jellysync.service",
                "ok": true,
            }))?
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(item_id: &str, path: &str) -> DownloadEntry {
        DownloadEntry {
            item_id: item_id.into(),
            job: "Show".into(),
            path: PathBuf::from(path),
            status: "complete".into(),
            bytes: 1,
            total: Some(1),
            rate: None,
        }
    }

    #[test]
    fn season_from_folder_or_tag() {
        assert_eq!(parse_season(Path::new("/tv/Show/Season 3/x.mkv")), Some(3));
        assert_eq!(parse_season(Path::new("/tv/Show/Specials/x.mkv")), Some(0));
        assert_eq!(
            parse_season(Path::new("/tv/Show/Show - S02E05 - Title.mkv")),
            Some(2)
        );
        assert_eq!(
            parse_season(Path::new("/tv/Show/show.s10e01.mkv")),
            Some(10)
        );
        assert_eq!(
            parse_season(Path::new("/movies/Sintel (2010)/Sintel.mkv")),
            None
        );
        // "S" inside a word is not a season tag.
        assert_eq!(parse_season(Path::new("/movies/X/Bosses2Ever.mkv")), None);
    }

    #[test]
    fn resolution_labels() {
        assert_eq!(resolution_label(3840, 1600), "4K");
        assert_eq!(resolution_label(1920, 800), "1080p");
        assert_eq!(resolution_label(1280, 720), "720p");
        assert_eq!(resolution_label(720, 576), "576p");
        assert_eq!(resolution_label(0, 0), "SD");
    }

    #[test]
    fn badges_from_ffprobe() {
        let probe = json!({
            "streams": [
                {"codec_type": "video", "codec_name": "mjpeg", "disposition": {"attached_pic": 1}},
                {"codec_type": "video", "codec_name": "hevc", "width": 3840, "height": 2160,
                 "color_transfer": "smpte2084"},
                {"codec_type": "audio", "codec_name": "aac", "channels": 2},
                {"codec_type": "audio", "codec_name": "eac3", "channels": 6,
                 "disposition": {"default": 1}},
                {"codec_type": "subtitle", "codec_name": "subrip"}
            ]
        });
        assert_eq!(media_badges(&probe), ["\u{f0567} 4K", "HEVC"]);
        let rows = media_summary(&probe);
        assert!(
            rows.iter()
                .any(|(label, value)| label == "Video" && value.contains("HDR10"))
        );
        assert_eq!(rows.iter().filter(|(label, _)| label == "Video").count(), 1);
    }

    #[test]
    fn movies_are_not_grouped() {
        let mut files = vec![entry("a", "/m/Sintel (2010)/Sintel.mkv")];
        let rows = file_rows(&mut files, None);
        assert!(matches!(rows.as_slice(), [FileRow::File(0)]));
    }

    #[test]
    fn shows_are_grouped_by_season_in_order() {
        let mut files = vec![
            entry("c", "/tv/Show/Season 10/Show - S10E01.mkv"),
            entry("b", "/tv/Show/Season 2/Show - S02E02.mkv"),
            entry("a", "/tv/Show/Season 2/Show - S02E01.mkv"),
        ];
        let rows = file_rows(&mut files, None);
        let labels: Vec<String> = rows
            .iter()
            .map(|row| match row {
                FileRow::Season(label) => label.clone(),
                FileRow::File(index) => files[*index].item_id.clone(),
            })
            .collect();
        // Season 10 sorts numerically after season 2, not lexically before it.
        assert_eq!(labels, ["Season 2", "a", "b", "Season 10", "c"]);
    }

    #[test]
    fn jellyfin_episode_numbers_win() {
        let mut episodes = HashMap::new();
        for (id, number) in [("x", 2), ("y", 1)] {
            episodes.insert(
                id.to_string(),
                EpisodeInfo {
                    season: Some(1),
                    episode: Some(number),
                    name: format!("Episode {number}"),
                },
            );
        }
        let details = JobDetails {
            series: true,
            title: "Show".into(),
            years: None,
            facts: Vec::new(),
            tagline: None,
            overview: String::new(),
            episodes,
            poster: None,
        };
        let mut files = vec![
            entry("x", "/tv/Show/Season 1/a.mkv"),
            entry("y", "/tv/Show/Season 1/b.mkv"),
        ];
        file_rows(&mut files, Some(&details));
        assert_eq!(files[0].item_id, "y");
        assert_eq!(
            file_title(Some(&details), &files[0], false),
            Some(("S01E01".into(), "Episode 1".into()))
        );
    }

    #[test]
    fn movie_title_only_for_a_lone_file() {
        let details = JobDetails {
            series: false,
            title: "Sintel".into(),
            years: Some("2010".into()),
            facts: Vec::new(),
            tagline: None,
            overview: String::new(),
            episodes: HashMap::new(),
            poster: None,
        };
        let file = entry("m", "/m/Sintel/Sintel.mkv");
        assert_eq!(
            file_title(Some(&details), &file, true),
            Some((String::new(), "Sintel (2010)".into()))
        );
        assert_eq!(file_title(Some(&details), &file, false), None);
        assert_eq!(file_title(None, &file, true), None);
    }

    #[test]
    fn clicks_skip_season_headings() {
        let rows = vec![
            FileRow::Season("Season 1".into()),
            FileRow::File(0),
            FileRow::File(1),
        ];
        let area = Rect::new(0, 0, 40, 20);
        // Line 1 is the heading, lines 2-4 the first file, 5-7 the second.
        assert_eq!(file_row_at(area, 5, 1, 0, &rows), None);
        assert_eq!(file_row_at(area, 5, 2, 0, &rows), Some(0));
        assert_eq!(file_row_at(area, 5, 4, 0, &rows), Some(0));
        assert_eq!(file_row_at(area, 5, 5, 0, &rows), Some(1));
        assert_eq!(file_row_at(area, 5, 8, 0, &rows), None);
        assert_eq!(file_row_at(area, 5, 1, 2, &rows), Some(1));
        // Borders are not rows.
        assert_eq!(file_row_at(area, 5, 0, 0, &rows), None);
    }

    fn season_fixture() -> (Vec<DownloadEntry>, Vec<FileRow>) {
        // One flat folder: seasons come from the SxxEyy tags only.
        let mut files = vec![
            entry("a", "/tv/Show/Show - S01E01.mkv"),
            entry("b", "/tv/Show/Show - S01E02.mkv"),
            entry("c", "/tv/Show/Show - S02E01.mkv"),
        ];
        let rows = file_rows(&mut files, None);
        (files, rows)
    }

    #[test]
    fn season_clear_matches_the_displayed_group() {
        let (files, rows) = season_fixture();
        let ids = |indices: Vec<usize>| -> Vec<String> {
            indices
                .into_iter()
                .map(|i| files[i].item_id.clone())
                .collect()
        };
        // Clearing a season must not take the other season from the same folder.
        assert_eq!(ids(season_files(&rows, &files, 0)), ["a", "b"]);
        assert_eq!(ids(season_files(&rows, &files, 2)), ["c"]);
    }

    #[test]
    fn season_clear_without_headings_uses_the_folder() {
        let files = vec![
            entry("a", "/m/Sintel/Sintel.mkv"),
            entry("b", "/m/Sintel/Sintel-extras.mkv"),
        ];
        // An ungrouped list, as for a movie folder.
        let rows = vec![FileRow::File(0), FileRow::File(1)];
        assert_eq!(season_files(&rows, &files, 1), [0, 1]);
    }

    #[test]
    fn clear_requests_capture_their_targets() {
        let (files, _) = season_fixture();
        let job = TuiJob {
            name: "library:Show".into(),
            adhoc: true,
        };
        let request = clear_request(ClearScope::File, true, &job, &files, 2).unwrap();
        assert_eq!(request.targets[0].item_id, "c");
        // Ad-hoc downloads are never synced again, so there is nothing to ignore.
        assert!(!request.ignore);
        let request = clear_request(ClearScope::Show, true, &job, &files, 0).unwrap();
        assert_eq!(request.label, "Show · all 3 files");
        // An empty ad-hoc job can still be removed; an empty configured job cannot.
        assert!(
            clear_request(ClearScope::Show, false, &job, &[], 0)
                .is_some_and(|r| r.targets.is_empty())
        );
        let configured = TuiJob {
            name: "Show".into(),
            adhoc: false,
        };
        assert!(clear_request(ClearScope::Show, true, &configured, &[], 0).is_none());
        assert!(clear_request(ClearScope::File, true, &configured, &[], 0).is_none());
        let request = clear_request(ClearScope::File, true, &configured, &files, 0).unwrap();
        assert!(request.ignore);
        let request = clear_request(ClearScope::File, false, &configured, &files, 0).unwrap();
        assert!(!request.ignore);
    }

    #[test]
    fn list_jumps_stay_in_bounds() {
        assert_eq!(list_jump(KeyCode::Home, 5, 10, 3), 0);
        assert_eq!(list_jump(KeyCode::End, 5, 10, 3), 9);
        assert_eq!(list_jump(KeyCode::PageDown, 8, 10, 3), 9);
        assert_eq!(list_jump(KeyCode::PageUp, 2, 10, 3), 0);
        assert_eq!(list_jump(KeyCode::PageDown, 0, 10, 0), 1);
        assert_eq!(list_jump(KeyCode::End, 0, 0, 3), 0);
    }

    #[test]
    fn clearing_is_confined_to_the_roots() {
        let base = env::temp_dir().join(format!("jellysync-test-{}", std::process::id()));
        let root = base.join("root");
        let outside = base.join("outside");
        std::fs::create_dir_all(root.join("Show")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let roots = vec![root.canonicalize().unwrap()];
        assert!(path_within_roots(&root.join("Show/a.mkv"), &roots).unwrap());
        assert!(!path_within_roots(&outside.join("a.mkv"), &roots).unwrap());
        assert!(!path_within_roots(&root.join("Show/../../outside/a.mkv"), &roots).unwrap());
        // A symlinked folder pointing outside does not count as inside.
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        assert!(!path_within_roots(&root.join("link/a.mkv"), &roots).unwrap());
        std::fs::remove_dir_all(&base).unwrap();
    }

    fn filter(spec: &str, available: &[u32]) -> Option<Vec<u32>> {
        let value = serde_yaml::from_str::<serde_yaml::Value>(spec).unwrap();
        number_filter(Some(&value), available)
            .unwrap()
            .map(|filter| (0..30).filter(|n| filter.contains(n)).collect())
    }

    #[test]
    fn number_filters() {
        let seasons = [0, 1, 2, 5];
        assert_eq!(filter("all", &seasons), None);
        assert_eq!(filter("latest", &seasons), Some(vec![5]));
        // Counts existing seasons and never picks specials.
        assert_eq!(filter("latest-2", &seasons), Some(vec![2, 5]));
        assert_eq!(filter("latest-9", &seasons), Some(vec![1, 2, 5]));
        assert_eq!(filter("latest", &[0]), Some(vec![0]));
        assert_eq!(filter("3-5", &seasons), Some(vec![3, 4, 5]));
        assert_eq!(filter("1, 3-4", &seasons), Some(vec![1, 3, 4]));
        assert_eq!(filter("[1, \"3-4\"]", &seasons), Some(vec![1, 3, 4]));
        assert_eq!(filter("7", &seasons), Some(vec![7]));
        // Huge ranges are matched, not materialized.
        let value = serde_yaml::Value::from("1-4294967295");
        assert!(
            number_filter(Some(&value), &[])
                .unwrap()
                .unwrap()
                .contains(&4_000_000_000)
        );
    }

    #[test]
    fn invalid_number_filters_are_errors() {
        for spec in ["latestX", "latest-0", "5-3", "one", "[]", "[true]", "[1.5]"] {
            let value = serde_yaml::from_str::<serde_yaml::Value>(spec).unwrap();
            assert!(number_filter(Some(&value), &[1, 2]).is_err(), "{spec}");
        }
        let value = serde_yaml::Value::from("latest");
        assert!(number_filter(Some(&value), &[]).is_err());
    }

    #[test]
    fn directory_substitution_prefers_longer_keys() {
        let mut directories = BTreeMap::new();
        directories.insert("tv".to_string(), "TV".to_string());
        directories.insert("tv_shows".to_string(), "TV Shows".to_string());
        assert_eq!(
            substitute_directories("$tv_shows/X", &directories),
            "TV Shows/X"
        );
        assert_eq!(substitute_directories("$tv/X", &directories), "TV/X");
    }

    #[test]
    fn home_expansion() {
        let home = env::var("HOME").unwrap_or_default();
        assert_eq!(expand_home("~"), home);
        assert_eq!(expand_home("~/Videos"), format!("{home}/Videos"));
        assert_eq!(expand_home("$HOME/Videos"), format!("{home}/Videos"));
        assert_eq!(expand_home("~other/Videos"), "~other/Videos");
        assert_eq!(expand_home("/abs/path"), "/abs/path");
    }

    fn config(yaml: &str) -> Config {
        serde_yaml::from_str(yaml).unwrap()
    }

    #[test]
    fn local_job_directories_expand_home() {
        let home = env::var("HOME").unwrap_or_default();
        let config = config(
            "remote: {hostname: h, username: u, root: /srv}
local: {root: /media, directories: {docs: ~/Documentaries, tv: TV}}
download: {mode: jellyfin}
jobs: []",
        );
        let job: Job = serde_yaml::from_str("{name: Planet, directory: docs}").unwrap();
        assert_eq!(
            resolved_path(&config, &job, false).unwrap(),
            format!("{home}/Documentaries/Planet")
        );
        let job: Job = serde_yaml::from_str("{name: Show, directory: tv}").unwrap();
        assert_eq!(
            resolved_path(&config, &job, false).unwrap(),
            "/media/TV/Show"
        );
        assert_eq!(resolved_path(&config, &job, true).unwrap(), "/srv/tv/Show");
    }

    fn media(name: &str, kind: &str, path: Option<&str>) -> MediaItem {
        MediaItem {
            id: name.to_lowercase(),
            name: name.into(),
            item_type: Some(kind.into()),
            production_year: None,
            path: path.map(str::to_string),
            parent_index_number: Some(2),
            index_number: Some(3),
            user_data: None,
            series_name: None,
            media_sources: None,
        }
    }

    #[test]
    fn exact_titles_win_over_substrings() {
        let items = vec![
            media("Sintel Returns", "Series", None),
            media("Sintel", "Movie", None),
        ];
        assert_eq!(find_one(items, "sintel").unwrap().unwrap().name, "Sintel");
        let items = vec![media("Sintel Returns", "Series", None)];
        assert_eq!(
            find_one(items, "Sintel").unwrap().unwrap().name,
            "Sintel Returns"
        );
        let items = vec![
            media("Sintel Returns", "Series", None),
            media("Sintel 2", "Movie", None),
        ];
        assert!(find_one(items, "Sintel").is_err());
    }

    #[test]
    fn destinations_handle_server_paths() {
        let root = Path::new("/local/Show");
        let item = media("Ep", "Episode", Some("D:\\TV\\Show\\Show - S02E03.mkv"));
        assert_eq!(
            item_destination(&item, root, "Season $season_number", "Show", false).unwrap(),
            PathBuf::from("/local/Show/Season 2/Show - S02E03.mkv")
        );
        let item = media("Movie", "Movie", Some("/movies/Sintel/Sintel.mkv"));
        assert_eq!(
            item_destination(
                &item,
                Path::new("/local/Sintel"),
                "Season $season_number",
                "Sintel",
                true
            )
            .unwrap(),
            PathBuf::from("/local/Sintel/Sintel.mkv")
        );
        let item = media("Bad", "Episode", Some("/tv/.."));
        assert!(item_destination(&item, root, "Season $season_number", "Show", false).is_err());
        let item = media("Bad", "Episode", Some("/tv/x.mkv"));
        assert!(item_destination(&item, root, "../escape", "Show", false).is_err());
    }

    #[test]
    fn content_range_totals() {
        assert_eq!(parse_content_range_total("bytes 0-9/10"), Some(10));
        assert_eq!(parse_content_range_total("bytes */1234"), Some(1234));
        assert_eq!(parse_content_range_total("bytes 0-9/*"), None);
    }

    #[test]
    fn late_terminal_replies_are_not_keystrokes() {
        use crossterm::event::KeyEvent;
        let key = |c: char| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
        let alt = |c: char| KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT);
        let mut filter = ReplyFilter::default();
        // ESC _ G i = 3 1 ; O K ESC \
        assert!(filter.swallow(&alt('_')));
        for c in "Gi=31;OK".chars() {
            assert!(filter.swallow(&key(c)), "{c}");
        }
        assert!(filter.swallow(&alt('\\')));
        // Typing afterwards works normally.
        assert!(!filter.swallow(&key('i')));
        assert!(!filter.swallow(&KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
    }

    #[test]
    fn cell_size_comes_from_the_active_viewer() {
        let clients = "100\t@1\t19\t39\n200\t@1\t12\t24\n300\t@2\t8\t16\n";
        assert_eq!(viewer_cell_size(clients, "@1"), Some((12, 24)));
        assert_eq!(viewer_cell_size(clients, "@2"), Some((8, 16)));
        assert_eq!(viewer_cell_size(clients, "@3"), None);
        // Older tmux without cell sizes reports empty or zero fields.
        assert_eq!(viewer_cell_size("100\t@1\t\t\n200\t@1\t0\t0\n", "@1"), None);
    }

    #[test]
    fn progress_bar_cells() {
        let cells = |(filled, track, _): (String, String, String)| {
            (filled.chars().count(), track.chars().count())
        };
        assert_eq!(cells(download_progress(0, Some(100), 10)), (0, 10));
        assert_eq!(cells(download_progress(50, Some(100), 10)), (5, 5));
        assert_eq!(cells(download_progress(55, Some(100), 10)), (6, 4));
        assert_eq!(cells(download_progress(100, Some(100), 10)), (10, 0));
        assert_eq!(download_progress(55, Some(100), 10).0, "█████▌");
        assert_eq!(download_progress(1, None, 4).1, "░░░░");
    }

    #[test]
    fn job_state_follows_its_files() {
        assert_eq!(job_display_state("success", ["queued"]), ("success", None));
        assert_eq!(job_display_state("running", []), ("running", None));
        assert_eq!(
            job_display_state("running", ["queued", "complete"]),
            ("queued", Some("1 queued".into()))
        );
        assert_eq!(
            job_display_state("running", ["downloading", "queued", "queued"]),
            ("downloading", Some("1 downloading · 2 queued".into()))
        );
    }

    #[test]
    fn plurals() {
        assert_eq!(plural(0, "item"), "0 items");
        assert_eq!(plural(1, "item"), "1 item");
        assert_eq!(plural(2, "sync job"), "2 sync jobs");
    }

    #[test]
    fn throughput_with_eta() {
        assert_eq!(format_throughput(1024 * 1024, 0, None), "1.0 MiB/s");
        assert_eq!(
            format_throughput(1024 * 1024, 0, Some(120 * 1024 * 1024)),
            "1.0 MiB/s · 2m left"
        );
        assert_eq!(format_throughput(1024, 10, Some(10)), "1.0 KiB/s");
    }

    #[test]
    fn status_timestamps() {
        assert_eq!(sqlite_utc_seconds("1970-01-01 00:00:00"), Some(0));
        assert_eq!(
            sqlite_utc_seconds("2026-09-24 22:00:00"),
            Some(1_790_287_200)
        );
        assert_eq!(
            sqlite_utc_seconds("2024-02-29 12:00:00"),
            Some(1_709_208_000)
        );
        assert_eq!(sqlite_utc_seconds("garbage"), None);
        assert_eq!(format_age(5), "just now");
        assert_eq!(format_age(-30), "just now");
        assert_eq!(format_age(125), "2 min ago");
        assert_eq!(format_age(7200), "2 h ago");
        assert_eq!(format_age(3 * 86_400), "3 d ago");
    }

    const BASE_CONFIG: &str = "remote: {hostname: h, username: u, root: /srv}
local: {root: /media}
download: {mode: jellyfin}
jobs:
  - name: Pioneer One
    directory: tv
    seasons: latest
  - name: Sintel
    directory: tv
";

    fn temp_dir(name: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!("jellysync-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn overlay_merges_jobs_by_name() {
        let mut base: serde_yaml::Value = serde_yaml::from_str(BASE_CONFIG).unwrap();
        let overlay: serde_yaml::Value = serde_yaml::from_str(
            "parallelism: 4
jobs:
  - {name: Pioneer One, seasons: null, unwatched: true}
  - {name: Elephants Dream, directory: tv}",
        )
        .unwrap();
        merge_config(&mut base, overlay);
        let config = parse_config(base).unwrap();
        assert_eq!(config.parallelism, 4);
        let names: Vec<_> = config.jobs.iter().map(|job| job.name.as_str()).collect();
        assert_eq!(names, ["Pioneer One", "Sintel", "Elephants Dream"]);
        // null unsets, other keys are kept.
        assert_eq!(config.jobs[0].seasons, None);
        assert_eq!(config.jobs[0].unwatched, Some(true));
        assert_eq!(config.jobs[0].directory.as_deref(), Some("tv"));
    }

    #[test]
    fn overlay_is_optional_and_errors_name_it() {
        let dir = temp_dir("overlay-load");
        let base = dir.join("config.yaml");
        let overlay = dir.join("state.yaml");
        std::fs::write(&base, BASE_CONFIG).unwrap();
        assert_eq!(
            load_config_with_overlay(&base, &overlay)
                .unwrap()
                .jobs
                .len(),
            2
        );
        std::fs::write(&overlay, "").unwrap();
        assert_eq!(
            load_config_with_overlay(&base, &overlay)
                .unwrap()
                .jobs
                .len(),
            2
        );
        std::fs::write(&overlay, "parallelism: 0").unwrap();
        let error = format!(
            "{:#}",
            load_config_with_overlay(&base, &overlay).unwrap_err()
        );
        assert!(error.contains("state.yaml"), "{error}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn settings_go_to_a_writable_config() {
        let dir = temp_dir("save-writable");
        let base = dir.join("config.yaml");
        let overlay = dir.join("state.yaml");
        std::fs::write(&base, BASE_CONFIG).unwrap();
        // A stale overlay value must not hide the edit.
        std::fs::write(&overlay, "jobs: [{name: Sintel, seasons: 1}]").unwrap();
        let written = save_job_setting(
            &base,
            &overlay,
            "Sintel",
            "seasons",
            SettingChange::Set("latest-2".into()),
        )
        .unwrap();
        assert_eq!(written, base);
        let config = load_config_with_overlay(&base, &overlay).unwrap();
        assert_eq!(config.jobs[1].seasons, Some("latest-2".into()));
        assert!(overlay_job_keys(&overlay, "Sintel").is_empty());
        save_job_setting(
            &base,
            &overlay,
            "Pioneer One",
            "seasons",
            SettingChange::Clear,
        )
        .unwrap();
        assert_eq!(
            load_config_with_overlay(&base, &overlay).unwrap().jobs[0].seasons,
            None
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn settings_go_to_the_overlay_when_read_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = temp_dir("save-readonly");
        let base = dir.join("config.yaml");
        let overlay = dir.join("state").join("config.yaml");
        std::fs::write(&base, BASE_CONFIG).unwrap();
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o444)).unwrap();
        if config_is_writable(&base) {
            // Running as root: permissions do not apply, nothing to test.
            return;
        }
        let written = save_job_setting(
            &base,
            &overlay,
            "Pioneer One",
            "unwatched",
            SettingChange::Set(true.into()),
        )
        .unwrap();
        assert_eq!(written, overlay);
        save_job_setting(
            &base,
            &overlay,
            "Pioneer One",
            "seasons",
            SettingChange::Clear,
        )
        .unwrap();
        let config = load_config_with_overlay(&base, &overlay).unwrap();
        assert_eq!(config.jobs[0].unwatched, Some(true));
        assert_eq!(config.jobs[0].seasons, None);
        assert_eq!(std::fs::read_to_string(&base).unwrap(), BASE_CONFIG);
        let keys = overlay_job_keys(&overlay, "Pioneer One");
        assert!(keys.contains("unwatched") && keys.contains("seasons"));
        // Reset drops the override and the base value applies again.
        save_job_setting(
            &base,
            &overlay,
            "Pioneer One",
            "seasons",
            SettingChange::Reset,
        )
        .unwrap();
        let config = load_config_with_overlay(&base, &overlay).unwrap();
        assert_eq!(config.jobs[0].seasons, Some("latest".into()));
        // Invalid filters are rejected before anything is written.
        let before = std::fs::read_to_string(&overlay).unwrap();
        assert!(
            save_job_setting(
                &base,
                &overlay,
                "Pioneer One",
                "seasons",
                SettingChange::Set("nope".into())
            )
            .is_err()
        );
        assert_eq!(std::fs::read_to_string(&overlay).unwrap(), before);
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn setting_input_parsing() {
        assert_eq!(
            parse_setting_input(SettingKind::Filter, " 3 ").unwrap(),
            SettingChange::Set(3u64.into())
        );
        assert_eq!(
            parse_setting_input(SettingKind::Filter, "latest-2").unwrap(),
            SettingChange::Set("latest-2".into())
        );
        assert!(parse_setting_input(SettingKind::Filter, "latestX").is_err());
        assert_eq!(
            parse_setting_input(SettingKind::Text, "  ").unwrap(),
            SettingChange::Clear
        );
        assert_eq!(
            parse_setting_input(SettingKind::Toggle, "yes").unwrap(),
            SettingChange::Set(true.into())
        );
        let job: Job = serde_yaml::from_str("{name: X, seasons: [1, \"3-4\"]}").unwrap();
        assert_eq!(job_setting_text(&job, "seasons").as_deref(), Some("1,3-4"));
    }

    fn read_only_fixture(name: &str) -> Option<(PathBuf, PathBuf, PathBuf)> {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = temp_dir(name);
        let base = dir.join("config.yaml");
        let overlay = dir.join("state").join("config.yaml");
        std::fs::write(&base, BASE_CONFIG).unwrap();
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o444)).unwrap();
        // As root permissions do not apply; nothing to test then.
        (!config_is_writable(&base)).then_some((dir, base, overlay))
    }

    fn cleanup(dir: &Path) {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = std::fs::set_permissions(
            dir.join("config.yaml"),
            std::fs::Permissions::from_mode(0o644),
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn global_settings_in_overlay_and_config() {
        if let Some((dir, base, overlay)) = read_only_fixture("global-ro") {
            let target = SettingTarget::Global(&["jellyfin", "base_url"]);
            save_setting(
                &base,
                &overlay,
                target,
                SettingChange::Set("https://tv".into()),
            )
            .unwrap();
            save_setting(
                &base,
                &overlay,
                SettingTarget::Global(&["parallelism"]),
                SettingChange::Set(5u64.into()),
            )
            .unwrap();
            let config = load_config_with_overlay(&base, &overlay).unwrap();
            assert_eq!(config.jellyfin.unwrap().base_url, "https://tv");
            assert_eq!(config.parallelism, 5);
            let document = read_yaml(&overlay).unwrap();
            assert_eq!(setting_value(&document, target), Some("https://tv".into()));
            // Clearing a required option is rejected.
            let required = SettingTarget::Global(&["remote", "hostname"]);
            assert!(save_setting(&base, &overlay, required, SettingChange::Clear).is_err());
            cleanup(&dir);
        }
        let dir = temp_dir("global-rw");
        let base = dir.join("config.yaml");
        let overlay = dir.join("state.yaml");
        std::fs::write(&base, BASE_CONFIG).unwrap();
        save_setting(
            &base,
            &overlay,
            SettingTarget::Global(&["library", "season_pattern"]),
            SettingChange::Set("S$season_number".into()),
        )
        .unwrap();
        assert!(!overlay.exists());
        let config = load_config_with_overlay(&base, &overlay).unwrap();
        assert_eq!(config.library.unwrap().season_pattern, "S$season_number");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn jobs_can_be_added_disabled_and_deleted() {
        let Some((dir, base, overlay)) = read_only_fixture("jobs-ro") else {
            return;
        };
        add_job(&base, &overlay, " Elephants Dream ", "tv").unwrap();
        assert!(add_job(&base, &overlay, "Elephants Dream", "tv").is_err());
        assert!(add_job(&base, &overlay, "  ", "tv").is_err());
        let config = load_config_with_overlay(&base, &overlay).unwrap();
        let dream = config
            .jobs
            .iter()
            .find(|job| job.name == "Elephants Dream")
            .unwrap();
        assert_eq!(dream.directory.as_deref(), Some("tv"));
        // Jobs from the read-only config can only be disabled.
        assert!(delete_job(&base, &overlay, "Sintel").is_err());
        save_job_setting(
            &base,
            &overlay,
            "Sintel",
            "enabled",
            SettingChange::Set(false.into()),
        )
        .unwrap();
        let config = load_config_with_overlay(&base, &overlay).unwrap();
        let names: Vec<_> = selected_jobs(&config, &[])
            .unwrap()
            .into_iter()
            .map(|job| job.name)
            .collect();
        assert_eq!(names, ["Pioneer One", "Elephants Dream"]);
        // Disabled jobs still run when named.
        assert_eq!(selected_jobs(&config, &["Sintel".into()]).unwrap().len(), 1);
        // Overlay-only jobs can be deleted.
        delete_job(&base, &overlay, "Elephants Dream").unwrap();
        let config = load_config_with_overlay(&base, &overlay).unwrap();
        assert!(config.jobs.iter().all(|job| job.name != "Elephants Dream"));
        cleanup(&dir);
    }

    #[test]
    fn deleting_from_a_writable_config() {
        let dir = temp_dir("delete-rw");
        let base = dir.join("config.yaml");
        let overlay = dir.join("state.yaml");
        std::fs::write(&base, BASE_CONFIG).unwrap();
        std::fs::write(&overlay, "jobs: [{name: Sintel, unwatched: true}]").unwrap();
        delete_job(&base, &overlay, "Sintel").unwrap();
        let config = load_config_with_overlay(&base, &overlay).unwrap();
        let names: Vec<_> = config.jobs.iter().map(|job| job.name.as_str()).collect();
        // Gone from both files, so the overlay entry does not resurrect it.
        assert_eq!(names, ["Pioneer One"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn field_input_parsing() {
        assert_eq!(
            parse_field_input(FieldKind::Number, "4").unwrap(),
            SettingChange::Set(4u64.into())
        );
        assert!(parse_field_input(FieldKind::Number, "four").is_err());
        assert_eq!(
            parse_field_input(FieldKind::List, " -a  --partial ").unwrap(),
            SettingChange::Set(serde_yaml::Value::Sequence(vec![
                "-a".into(),
                "--partial".into()
            ]))
        );
        assert_eq!(
            parse_field_input(FieldKind::Text, "").unwrap(),
            SettingChange::Clear
        );
        let flags: serde_yaml::Value = serde_yaml::from_str("[-a, -v]").unwrap();
        assert_eq!(field_display(Some(&flags)).as_deref(), Some("-a -v"));
        assert_eq!(field_display(Some(&serde_yaml::Value::Null)), None);
    }

    #[test]
    fn settings_rows_group_fields_and_jobs() {
        let config = parse_config(serde_yaml::from_str(BASE_CONFIG).unwrap()).unwrap();
        let rows = settings_rows(&config.jobs);
        assert_eq!(rows[0], SettingsRow::Header("General".into()));
        assert!(rows[1].selectable());
        let fields = rows
            .iter()
            .filter(|row| matches!(row, SettingsRow::Field(_)))
            .count();
        assert_eq!(fields, GLOBAL_FIELDS.len());
        assert!(rows.contains(&SettingsRow::Job("Sintel".into())));
        assert_eq!(rows.last(), Some(&SettingsRow::OpenEditor));
    }

    #[test]
    fn versions_look_like_git_describe() {
        assert_eq!(format_version("1.1.0", ""), "1.1.0");
        assert_eq!(format_version("1.1.0", "135343d"), "1.1.0-135343d");
        assert_eq!(
            format_version("1.1.0", "135343d-dirty"),
            "1.1.0-135343d-dirty"
        );
    }

    #[test]
    fn completions_cover_the_cli() {
        let shells = [
            clap_complete::Shell::Bash,
            clap_complete::Shell::Zsh,
            clap_complete::Shell::Fish,
        ];
        for shell in shells {
            let mut script = Vec::new();
            clap_complete::generate(shell, &mut completion_command(), "jellysync", &mut script);
            let script = String::from_utf8(script).unwrap();
            for word in ["download", "fetch", "prune", "completions"] {
                assert!(script.contains(word), "{shell}: {word}");
            }
            for option in ["config", "json", "version"] {
                // fish declares long options as `-l name`.
                let spelled = match shell {
                    clap_complete::Shell::Fish => format!("-l {option}"),
                    _ => format!("--{option}"),
                };
                assert!(script.contains(&spelled), "{shell}: {spelled}");
            }
            // The internal worker command stays hidden.
            assert!(!script.contains("worker"), "{shell}");
        }
    }

    #[test]
    fn legacy_job_messages_are_pluralized() {
        let dir = temp_dir("state-db-plural");
        let path = dir.join("state.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE jobs (name TEXT PRIMARY KEY, status TEXT NOT NULL, message TEXT, updated_at TEXT NOT NULL);
                INSERT INTO jobs VALUES('a','success','1 item(s) downloaded','');
                INSERT INTO jobs VALUES('b','running','12 item(s)','');
                INSERT INTO jobs VALUES('c','success','downloaded 1 item(s)','');",
            )
            .unwrap();
        }
        let conn = open_db(&path).unwrap();
        let message = |name: &str| -> String {
            conn.query_row(
                "SELECT message FROM jobs WHERE name=?1",
                params![name],
                |row| row.get(0),
            )
            .unwrap()
        };
        assert_eq!(message("a"), "1 item downloaded");
        assert_eq!(message("b"), "12 items");
        assert_eq!(message("c"), "1 item downloaded");
    }

    #[test]
    fn fresh_state_db_survives_parallel_first_use() {
        let dir = temp_dir("state-db");
        let path = dir.join("state.db");
        let workers: Vec<_> = (0..16)
            .map(|index| {
                let path = path.clone();
                std::thread::spawn(move || -> Result<()> {
                    let conn = open_db(&path)?;
                    conn.execute(
                        "INSERT INTO jobs(name,status,message,updated_at) VALUES(?1,'ok','',datetime('now'))",
                        params![format!("job{index}")],
                    )?;
                    Ok(())
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap().unwrap();
        }
        let count: i64 = open_db(&path)
            .unwrap()
            .query_row("SELECT COUNT(*) FROM jobs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 16);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn durations() {
        assert_eq!(
            parse_duration("7d").unwrap(),
            Duration::from_secs(7 * 86_400)
        );
        assert_eq!(
            parse_duration("12h").unwrap(),
            Duration::from_secs(12 * 3600)
        );
        assert_eq!(parse_duration("30m").unwrap(), Duration::from_secs(1800));
        assert_eq!(
            parse_duration("3").unwrap(),
            Duration::from_secs(3 * 86_400)
        );
        assert_eq!(parse_duration("0").unwrap(), Duration::ZERO);
        assert!(parse_duration("soon").is_err());
        assert!(parse_duration("7w").is_err());
        assert_eq!(format_duration_short(3 * 86_400 - 5), "3d");
        assert_eq!(format_duration_short(3 * 86_400 + 5), "4d");
        assert_eq!(format_duration_short(7200), "2h");
    }

    #[test]
    fn grace_period_countdown() {
        let played_at = "2026-09-20 12:00:00";
        let played = sqlite_utc_seconds(played_at).unwrap();
        let grace = Duration::from_secs(7 * 86_400);
        assert_eq!(
            cleanup_remaining(Some(played_at), grace, played + 86_400),
            6 * 86_400
        );
        assert_eq!(
            cleanup_remaining(Some(played_at), grace, played + 8 * 86_400),
            0
        );
        // Unknown play time counts from now: never deleted early.
        assert_eq!(cleanup_remaining(None, grace, played), 7 * 86_400);
        let state = WatchedState {
            played: true,
            pending: true,
            played_at: Some(played_at.into()),
        };
        let (label, soon) = watched_label(Some(&state), Some(grace), played + 86_400).unwrap();
        assert_eq!(label, "\u{f0208} watched (not synced) · deleted in 6d");
        let (label, _) = watched_label(Some(&state), Some(grace), played + 60).unwrap();
        assert_eq!(label, "\u{f0208} watched (not synced) · deleted in 7d");
        assert!(!soon);
        let (label, soon) = watched_label(Some(&state), Some(grace), played + 8 * 86_400).unwrap();
        assert!(label.ends_with("deleted at next sync") && soon);
        let unwatched = WatchedState {
            played: false,
            pending: false,
            played_at: None,
        };
        assert!(watched_label(Some(&unwatched), Some(grace), played).is_none());
        assert_eq!(
            watched_label(Some(&state), None, played).unwrap().0,
            "\u{f0208} watched (not synced)"
        );
    }

    #[test]
    fn local_marks_win_over_the_server_until_pushed() {
        let dir = temp_dir("watched-db");
        let conn = open_db(&dir.join("state.db")).unwrap();
        record_server_watched(
            &conn,
            &[
                ("a".into(), true, Some("2026-09-01 10:00:00".into())),
                ("b".into(), false, None),
            ],
        )
        .unwrap();
        mark_watched(&conn, &["b".into()], true).unwrap();
        mark_watched(&conn, &["a".into()], false).unwrap();
        // The server still reports the old state; the pending marks stay.
        record_server_watched(
            &conn,
            &[("a".into(), true, None), ("b".into(), false, None)],
        )
        .unwrap();
        let states = watched_states(&conn).unwrap();
        assert!(!states["a"].played && states["a"].pending && states["a"].played_at.is_none());
        assert!(states["b"].played && states["b"].pending && states["b"].played_at.is_some());
        // Once pushed, the server's state applies again and keeps the first play time.
        conn.execute("UPDATE watched SET pending=0", []).unwrap();
        record_server_watched(
            &conn,
            &[("b".into(), true, Some("2030-01-01 00:00:00".into()))],
        )
        .unwrap();
        let states = watched_states(&conn).unwrap();
        assert!(states["b"].played && !states["b"].pending);
        assert_ne!(
            states["b"].played_at.as_deref(),
            Some("2030-01-01 00:00:00")
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn jellyfin_user_data_is_parsed() {
        let item: MediaItem = serde_json::from_value(json!({
            "Id": "x",
            "Name": "Earthfall",
            "UserData": {"Played": true, "LastPlayedDate": "2026-09-25T10:55:43.716309Z"}
        }))
        .unwrap();
        let data = item.user_data.unwrap();
        assert_eq!(data.is_played, Some(true));
        assert_eq!(
            data.last_played_date
                .as_deref()
                .and_then(jellyfin_datetime)
                .as_deref(),
            Some("2026-09-25 10:55:43")
        );
    }

    #[test]
    fn open_ended_ranges() {
        assert_eq!(filter("13+", &[1]).unwrap().last(), Some(&29));
        assert_eq!(filter("13+", &[1]).unwrap().first(), Some(&13));
        assert_eq!(filter("13-", &[1]).unwrap().first(), Some(&13));
        assert_eq!(filter("-3", &[1]), Some(vec![0, 1, 2, 3]));
        assert_eq!(filter("\"-3\"", &[1]), Some(vec![0, 1, 2, 3]));
        assert_eq!(filter("[-2, 5]", &[1]), Some(vec![0, 1, 2, 5]));
        assert_eq!(filter("1, 20+", &[1]).unwrap()[..2], [1, 20]);
    }

    #[test]
    fn durations_may_be_bare_numbers() {
        let config = parse_config(
            serde_yaml::from_str(
                "remote: {hostname: h, username: u, root: /r}
local: {root: /l}
cleanup: {delete_watched_after: 0}
jobs: [{name: X, directory: tv, delete_watched: true, delete_watched_after: 3}]",
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            config.jobs[0].watched_grace(&config),
            Some(Duration::from_secs(3 * 86_400))
        );
        assert_eq!(default_watched_grace(&config), Duration::ZERO);
        let broken = serde_yaml::from_str(
            "remote: {hostname: h, username: u, root: /r}
local: {root: /l}
cleanup: {delete_watched_after: soon}",
        )
        .unwrap();
        assert!(parse_config(broken).is_err());
    }

    #[test]
    fn sample_config_is_valid() {
        let sample = include_str!("../jellysync-config.sample.yaml");
        let config = parse_config(serde_yaml::from_str(sample).unwrap()).unwrap();
        assert!(
            config
                .jobs
                .iter()
                .any(|job| job.watched_grace(&config).is_some())
        );
        for job in &config.jobs {
            validate_job_filters(job).unwrap();
        }
    }

    #[test]
    fn auto_selection_respects_both_limits() {
        let gib = 1024 * 1024 * 1024;
        let candidates: Vec<(String, Option<u64>)> = vec![
            ("new".into(), Some(2 * gib)),
            ("huge".into(), Some(40 * gib)),
            ("mid".into(), Some(3 * gib)),
            ("old".into(), Some(gib)),
            ("oldest".into(), None),
        ];
        assert_eq!(
            select_auto_items(&candidates, Some(2), None),
            ["new", "huge"]
        );
        // Too big for what is left is skipped; smaller, older items fill up.
        assert_eq!(
            select_auto_items(&candidates, None, Some(6 * gib)),
            ["new", "mid", "old", "oldest"]
        );
        assert_eq!(
            select_auto_items(&candidates, Some(2), Some(6 * gib)),
            ["new", "mid"]
        );
        assert!(select_auto_items(&candidates, Some(0), None).is_empty());
    }

    #[test]
    fn auto_jobs_parse() {
        let jobs: Vec<Job> = serde_yaml::from_str(
            "[{name: a, auto: true}, {name: b, auto: movies, max_items: 10},
              {name: c, auto: shows, max_size: 30}, {name: d, auto: false}, {name: e}]",
        )
        .unwrap();
        assert_eq!(jobs[0].auto, Some(AutoKind::All));
        assert_eq!(jobs[1].auto, Some(AutoKind::Movies));
        assert_eq!(jobs[2].auto, Some(AutoKind::Shows));
        assert!(!jobs[3].is_auto() && !jobs[4].is_auto());
        // No limit at all: 5 items; a size limit alone means no item limit.
        assert_eq!(jobs[0].auto_max_items(), Some(5));
        assert_eq!(jobs[1].auto_max_items(), Some(10));
        assert_eq!(jobs[2].auto_max_items(), None);
        // Auto jobs clean up watched files unless told not to.
        assert!(jobs[0].deletes_watched() && !jobs[4].deletes_watched());
        assert!(serde_yaml::from_str::<Job>("{name: x, auto: sometimes}").is_err());
    }

    #[test]
    fn auto_target_from_the_command_line() {
        let config = parse_config(serde_yaml::from_str(BASE_CONFIG).unwrap()).unwrap();
        let config = with_auto_target(config, &["auto".into()], None, Some(20.0)).unwrap();
        let auto = config.jobs.iter().find(|job| job.name == "auto").unwrap();
        assert_eq!(auto.auto, Some(AutoKind::All));
        assert_eq!((auto.max_items, auto.max_size), (None, Some(20.0)));
        // Limits without any auto job to apply them to are an error.
        let config = parse_config(serde_yaml::from_str(BASE_CONFIG).unwrap()).unwrap();
        assert!(with_auto_target(config, &["Pioneer One".into()], Some(3), None).is_err());
        // rsync mode cannot run auto jobs.
        let rsync =
            BASE_CONFIG.replace("mode: jellyfin", "mode: rsync") + "  - {name: new, auto: true}\n";
        assert!(parse_config(serde_yaml::from_str(&rsync).unwrap()).is_err());
    }

    #[test]
    fn auto_job_files_group_by_show() {
        let mut files = vec![
            entry("m", "/v/Movies/Sintel/Sintel.mkv"),
            entry(
                "b2",
                "/v/TV Shows/Pioneer One/Season 1/Pioneer One - S01E02.mkv",
            ),
            entry(
                "a1",
                "/v/TV Shows/Another Show/Season 1/Another Show - S01E01.mkv",
            ),
            entry(
                "b1",
                "/v/TV Shows/Pioneer One/Season 1/Pioneer One - S01E01.mkv",
            ),
        ];
        let rows = file_rows(&mut files, None);
        let labels: Vec<String> = rows
            .iter()
            .map(|row| match row {
                FileRow::Season(label) => label.clone(),
                FileRow::File(index) => files[*index].item_id.clone(),
            })
            .collect();
        assert_eq!(
            labels,
            [
                "Another Show · Season 1",
                "a1",
                "Pioneer One · Season 1",
                "b1",
                "b2",
                "Sintel",
                "m"
            ]
        );
    }

    #[test]
    fn auto_jobs_edit_their_limits() {
        let auto: Job = serde_yaml::from_str("{name: new, auto: shows}").unwrap();
        let keys: Vec<_> = job_settings("jellyfin", &auto)
            .iter()
            .map(|setting| setting.key)
            .collect();
        assert!(
            keys.contains(&"max_items") && keys.contains(&"max_size") && !keys.contains(&"seasons")
        );
        let normal: Job = serde_yaml::from_str("{name: x, directory: tv}").unwrap();
        let keys: Vec<_> = job_settings("jellyfin", &normal)
            .iter()
            .map(|setting| setting.key)
            .collect();
        assert!(keys.contains(&"auto") && !keys.contains(&"max_items"));
        assert_eq!(
            parse_setting_input(SettingKind::Gib, "7.5").unwrap(),
            SettingChange::Set(7.5.into())
        );
        assert!(parse_setting_input(SettingKind::Gib, "-1").is_err());
        assert!(parse_setting_input(SettingKind::Number, "ten").is_err());
    }

    #[test]
    fn job_summaries_are_short() {
        let config = parse_config(
            serde_yaml::from_str(
                "remote: {hostname: h, username: u, root: /r}
local: {root: /l}
jobs:
  - {name: a, directory: tv_shows, seasons: latest, unwatched: true, delete_watched: true}
  - {name: b, auto: shows, max_size: 30, library: Kids}
  - {name: c, auto: movies}
  - {name: d, directory: movies, enabled: false}",
            )
            .unwrap(),
        )
        .unwrap();
        let summaries: Vec<String> = config
            .jobs
            .iter()
            .map(|job| job_summary(job, &config))
            .collect();
        assert_eq!(
            summaries,
            [
                "tv_shows · seasons latest · unwatched only · deletes watched after 7d",
                "auto: shows · max 30 GiB · library Kids · deletes watched after 7d",
                "auto: movies · max 5 items · deletes watched after 7d",
                "movies · disabled",
            ]
        );
    }
}
