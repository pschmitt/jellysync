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

#[derive(Parser)]
#[command(
    name = "jellysync",
    version,
    about = "◆ jellysync · keep a local copy of your Jellyfin shows and movies",
    long_about = None,
    styles = HELP_STYLES,
    after_help = "Run without a command to open the TUI."
)]
struct Cli {
    /// Config file (default: $JELLYSYNC_CONFIG, ./jellysync.yaml, ~/.config/jellysync/config.yaml)
    #[arg(short, long, global = true, value_name = "FILE")]
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
        /// Job names to download (default: all jobs)
        #[arg(value_name = "JOB")]
        target: Vec<String>,
    },
    /// Show recent job status and the timer state
    Status,
    /// Remove tracked files whose media no longer exists in Jellyfin (or remotely, in rsync mode)
    Prune {
        /// Actually delete files instead of previewing
        #[arg(long)]
        apply: bool,
        /// Job names to prune (default: all jobs)
        #[arg(value_name = "JOB")]
        target: Vec<String>,
    },
    /// Print the parsed configuration
    Config,
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
    jobs: Vec<Job>,
}
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
    let content =
        std::fs::read_to_string(path).with_context(|| format!("read config {}", path.display()))?;
    let mut config: Config = serde_yaml::from_str(&content).context("parse YAML config")?;
    if config.parallelism == 0 {
        bail!("parallelism must be at least 1");
    }
    if config.download.mode != "rsync" && config.download.mode != "jellyfin" {
        bail!("download.mode must be either 'rsync' or 'jellyfin'");
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
fn state_db() -> Result<PathBuf> {
    let base = env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".local/state")))
        .context("cannot determine state directory")?;
    let dir = base.join("jellysync");
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join("state.db"))
}
fn db() -> Result<Connection> {
    let conn = Connection::open(state_db()?)?;
    conn.busy_timeout(Duration::from_secs(10))?;
    conn.execute_batch("PRAGMA journal_mode=WAL;
        CREATE TABLE IF NOT EXISTS jobs (name TEXT PRIMARY KEY, status TEXT NOT NULL, message TEXT, updated_at TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS transfers (item_id TEXT PRIMARY KEY, job TEXT NOT NULL, path TEXT NOT NULL, status TEXT NOT NULL, bytes INTEGER NOT NULL DEFAULT 0, total INTEGER, updated_at TEXT NOT NULL);")?;
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
fn update_transfer(
    item_id: &str,
    job: &str,
    path: &Path,
    status: &str,
    bytes: u64,
    total: Option<u64>,
) -> Result<()> {
    db()?.execute("INSERT INTO transfers(item_id,job,path,status,bytes,total,updated_at) VALUES(?1,?2,?3,?4,?5,?6,datetime('now'))
        ON CONFLICT(item_id) DO UPDATE SET job=excluded.job,path=excluded.path,status=excluded.status,bytes=excluded.bytes,total=excluded.total,updated_at=excluded.updated_at",
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
    is_played: Option<bool>,
}

/// Cheap unauthenticated reachability check used by the TUI's offline mode.
async fn jellyfin_reachable(config: &Jellyfin) -> bool {
    let Ok(client) = HttpClient::builder()
        .user_agent("jellysync/1.1.0")
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

async fn jellyfin_login(config: &Jellyfin) -> Result<JellyfinApi> {
    let base = config.base_url.trim_end_matches('/').to_string();
    let client = HttpClient::builder()
        .user_agent("jellysync/1.1.0")
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
        let authorization = r#"MediaBrowser Client="jellysync", Device="jellysync", DeviceId="jellysync", Version="1.1.0""#;
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
        r#"MediaBrowser Client="jellysync", Device="jellysync", DeviceId="jellysync", Version="1.1.0", Token="{token}""#
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
    update_job(&job, "queued", &format!("{} item(s) queued", items.len()))?;
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
        update_job(job, "running", &format!("{} item(s)", pending.len()))?;
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
            update_job(job, "success", &format!("{count} item(s) downloaded"))?;
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
                serde_yaml::Value::Number(number) => number
                    .as_u64()
                    .map(|number| number.to_string())
                    .context("filter numbers must be non-negative integers"),
                serde_yaml::Value::String(text) => Ok(text.clone()),
                _ => bail!("filter list entries must be numbers or strings"),
            })
            .collect::<Result<_>>()?
    } else if let Some(value) = spec.as_u64() {
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
        } else if let Some((first, last)) = part.split_once('-') {
            let (first, last) = (number(first)?, number(last)?);
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
    let (api, movie_mode, items) = jellyfin_job_items(config, job).await?;
    if items.is_empty() {
        let message = if job.unwatched == Some(true) {
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
    let count = items.len();
    if !quiet {
        say!(
            "{} {}  {}",
            "▸".with(TerminalColor::Cyan),
            job.name.clone().with(TerminalColor::Cyan).bold(),
            format!("{count} item(s)").with(TerminalColor::DarkGrey)
        );
    }
    let destination = PathBuf::from(resolved_path(config, job, false)?);
    std::fs::create_dir_all(&destination)?;
    update_job(&job.name, "running", &format!("{count} item(s)"))?;
    let pattern = config
        .library
        .as_ref()
        .map(|l| l.season_pattern.clone())
        .unwrap_or_else(default_season_pattern);
    // Record every pending item up front so ones waiting for a worker slot show as queued.
    for item in &items {
        if let Ok(output) = item_destination(item, &destination, &pattern, &job.name, movie_mode) {
            mark_queued(&job.name, item, &output)?;
        }
    }
    let mut tasks = JoinSet::new();
    for item in items {
        let api = api.clone();
        let name = job.name.clone();
        let destination = destination.clone();
        let pattern = pattern.clone();
        let permit = slots.clone().acquire_owned().await?;
        tasks.spawn(async move {
            let _permit = permit;
            let output = item_destination(&item, &destination, &pattern, &name, movie_mode)?;
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
            "{} Jellyfin download(s) failed for '{}': {}",
            failures.len(),
            job.name,
            failures.join("; ")
        );
    }
    update_job(&job.name, "success", &format!("{count} item(s) downloaded"))?;
    Ok(())
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
    if job.unwatched == Some(true) {
        items.retain(|item| item.user_data.as_ref().and_then(|u| u.is_played) == Some(false));
    }
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

/// Record download progress unless the transfer was paused meanwhile; returns false
/// when it was paused and the download should stop.
/// Record progress; false when the transfer was paused or cleared meanwhile, in
/// which case the download stops (a cleared one must not come back to life).
fn update_progress(item_id: &str, bytes: u64, total: Option<u64>) -> Result<bool> {
    let updated = db()?.execute(
        "UPDATE transfers SET status='downloading',bytes=?2,total=?3,updated_at=datetime('now') WHERE item_id=?1 AND status NOT IN ('paused','cleared')",
        params![item_id, bytes as i64, total.map(|v| v as i64)],
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
    if destination.exists() || transfer_status(&item.id)?.as_deref() == Some("paused") {
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
    if transfer_status(&item.id)?.as_deref() == Some("paused") {
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
    if !update_progress(&item.id, start, total)? {
        return Ok(());
    }
    let mut stream = response.bytes_stream();
    let mut bytes = start;
    let mut checkpoint = start;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("read Jellyfin download stream")?;
        tokio::io::AsyncWriteExt::write_all(&mut file, &chunk).await?;
        bytes += chunk.len() as u64;
        if bytes - checkpoint >= 4 * 1024 * 1024 {
            if !update_progress(&item.id, bytes, total)? {
                // Paused: keep the .partial file and free this worker slot.
                tokio::io::AsyncWriteExt::flush(&mut file).await?;
                return Ok(());
            }
            checkpoint = bytes;
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
                "{} job(s) · {} mode · {parallelism} worker(s)",
                jobs.len(),
                config.download.mode
            )
            .with(TerminalColor::DarkGrey)
        );
        say!("{}", "─".repeat(48).with(TerminalColor::DarkGrey));
    }
    let slots = Arc::new(Semaphore::new(parallelism));
    let mut tasks = JoinSet::new();
    for job in jobs {
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
            "{} sync job(s) failed:\n{}",
            failures.len(),
            failures.join("\n")
        );
    }
    Ok(())
}
fn selected_jobs(config: &Config, targets: &[String]) -> Result<Vec<Job>> {
    let jobs: Vec<Job> = if targets.is_empty() {
        config.jobs.clone()
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
fn status(json_output: bool) -> Result<()> {
    let conn = db()?;
    if json_output {
        return status_json(&conn);
    }
    println!("\n{} {}", "◆".with(TerminalColor::Cyan), "Jellysync".bold());
    println!("{}", "─".repeat(48).with(TerminalColor::DarkGrey));
    println!("{}", "JOBS".with(TerminalColor::Magenta).bold());
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
    let mut job_count = 0;
    for row in rows {
        let (name, state, message, time) = row?;
        job_count += 1;
        let (icon, color) = match state.to_ascii_lowercase().as_str() {
            "success" | "complete" | "completed" => ("●", TerminalColor::Green),
            "running" | "downloading" => ("◐", TerminalColor::Yellow),
            "queued" => ("◌", TerminalColor::Blue),
            "paused" => ("⏸", TerminalColor::DarkGrey),
            "failed" | "error" | "skipped" => ("✕", TerminalColor::Red),
            "interrupted" | "cleared" => ("◆", TerminalColor::Magenta),
            _ => ("○", TerminalColor::DarkGrey),
        };
        println!(
            "  {} {:<28} {} {}",
            icon.with(color),
            truncate(&name, 28).bold(),
            state.to_uppercase().with(color),
            if time.is_empty() {
                String::new().dim()
            } else {
                time.dim()
            }
        );
        if let Some(message) = message.filter(|message| !message.is_empty()) {
            println!(
                "      {}",
                truncate(&message, 88).with(TerminalColor::DarkGrey)
            );
        }
    }
    if job_count == 0 {
        println!("  {}", "No jobs have run yet".with(TerminalColor::DarkGrey));
    }
    println!(
        "\n{}",
        "ACTIVE DOWNLOADS".with(TerminalColor::Magenta).bold()
    );
    let mut statement = conn.prepare("SELECT job,path,status,bytes,total FROM transfers WHERE status IN ('queued','downloading','interrupted','paused') ORDER BY job,path")?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, u64>(3)?,
            row.get::<_, Option<u64>>(4)?,
        ))
    })?;
    let mut download_count = 0;
    for row in rows {
        let (job, path, state, bytes, total) = row?;
        download_count += 1;
        let (bar, percent) = download_progress(bytes, total, 20);
        let filename = Path::new(&path)
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
        println!(
            "  {} {} {}",
            "↳".with(state_color),
            truncate(&filename, 42).bold(),
            format!("({job})").with(TerminalColor::DarkGrey)
        );
        println!(
            "    {} {}  {}",
            bar.with(state_color),
            percent.with(state_color),
            state.to_uppercase().with(state_color)
        );
    }
    if download_count == 0 {
        println!(
            "  {}",
            "Nothing is downloading".with(TerminalColor::DarkGrey)
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
fn status_json(conn: &Connection) -> Result<()> {
    let mut statement =
        conn.prepare("SELECT name,status,message,updated_at FROM jobs ORDER BY name")?;
    let jobs = statement
        .query_map([], |row| {
            Ok(json!({
                "name": row.get::<_, String>(0)?,
                "status": row.get::<_, String>(1)?,
                "message": row.get::<_, Option<String>>(2)?,
                "updated_at": row.get::<_, String>(3)?,
            }))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut statement = conn.prepare("SELECT job,path,status,bytes,total FROM transfers WHERE status IN ('queued','downloading','interrupted','paused') ORDER BY job,path")?;
    let downloads = statement
        .query_map([], |row| {
            Ok(json!({
                "job": row.get::<_, String>(0)?,
                "path": row.get::<_, String>(1)?,
                "status": row.get::<_, String>(2)?,
                "bytes": row.get::<_, u64>(3)?,
                "total": row.get::<_, Option<u64>>(4)?,
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
async fn prune(config: Config, target: Vec<String>, apply: bool, json_output: bool) -> Result<()> {
    let jobs = selected_jobs(&config, &target)?;
    if config.download.mode == "jellyfin" {
        for job in jobs {
            let api = jellyfin_login(
                config
                    .jellyfin
                    .as_ref()
                    .context("Jellyfin mode requires credentials")?,
            )
            .await?;
            let current = jellyfin_item_ids(&api, &job).await?;
            let stale: Vec<_> = tracked_downloads()?
                .into_iter()
                .filter(|entry| entry.job == job.name && !current.contains(&entry.item_id))
                .collect();
            for entry in &stale {
                if apply {
                    if !json_output {
                        println!("pruning {}", entry.path.display());
                    }
                    clear_tracked(&config, std::slice::from_ref(entry))?;
                } else if !json_output {
                    println!("would prune {}", entry.path.display());
                }
            }
        }
        return Ok(());
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
        if !apply {
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
    Ok(())
}
#[derive(Clone, Copy, PartialEq, Debug)]
enum ClearScope {
    File,
    Season,
    Show,
}

/// A pending clear: the exact files it removes are fixed when it is requested,
/// so list refreshes or re-sorting before the confirmation cannot change them.
struct ClearRequest {
    scope: ClearScope,
    job: String,
    adhoc: bool,
    targets: Vec<DownloadEntry>,
    label: String,
}

/// Build a clear request for the selected job, or None when there is nothing to clear.
fn clear_request(
    scope: ClearScope,
    job: &TuiJob,
    downloads: &[DownloadEntry],
    rows: &[FileRow],
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
        ClearScope::Season => {
            let targets: Vec<_> = season_files(rows, downloads, selected)
                .into_iter()
                .map(|index| downloads[index].clone())
                .collect();
            let position = rows
                .iter()
                .position(|row| matches!(row, FileRow::File(index) if *index == selected))?;
            let season = rows[..position].iter().rev().find_map(|row| match row {
                FileRow::Season(label) => Some(label.clone()),
                FileRow::File(_) => None,
            });
            let label = match season {
                Some(season) => format!("{show} · {season} ({})", files(targets.len())),
                None => format!("{show} · this folder ({})", files(targets.len())),
            };
            (targets, label)
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
}

#[derive(Default)]
struct DashboardSnapshot {
    history: BTreeMap<String, (String, String, String)>,
    downloads: Vec<DownloadEntry>,
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
    Ok(DashboardSnapshot {
        history,
        downloads,
        timer,
    })
}

fn tracked_downloads() -> Result<Vec<DownloadEntry>> {
    let conn = db()?;
    let mut statement = conn.prepare("SELECT item_id,job,path,status,bytes,total FROM transfers WHERE status != 'cleared' ORDER BY job,path")?;
    let rows = statement.query_map([], |row| {
        Ok(DownloadEntry {
            item_id: row.get(0)?,
            job: row.get(1)?,
            path: PathBuf::from(row.get::<_, String>(2)?),
            status: row.get(3)?,
            bytes: row.get(4)?,
            total: row.get(5)?,
        })
    })?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

/// Delete the given tracked files (and their partial data) and mark them cleared.
fn clear_tracked(config: &Config, targets: &[DownloadEntry]) -> Result<usize> {
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
            "UPDATE transfers SET status='cleared',updated_at=datetime('now') WHERE item_id=?1",
            params![entry.item_id],
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
    for job in &config.jobs {
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

async fn tui(config: Config, config_path: PathBuf) -> Result<()> {
    // Status messages used to read "downloaded N item(s)".
    db()?.execute(
        "UPDATE jobs SET message=substr(message,12)||' downloaded' WHERE message LIKE 'downloaded % item(s)'",
        [],
    )?;
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
        let mut confirm_clear: Option<ClearRequest> = None;
        let mut show_help = false;
        let mut show_job_config = false;
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
            if ping_task.as_ref().is_some_and(|task| task.is_finished()) {
                let task = ping_task.take().expect("finished ping task exists");
                let reachable = task.await.unwrap_or(false);
                let was_online = online == Some(Some(true));
                let was_offline = online == Some(Some(false));
                online = Some(Some(reachable));
                pinged_at = Some(Instant::now());
                if reachable && !was_online {
                    // Retry what failed while the connection was flaky.
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
                    let (kind, kind_style) = if job.adhoc {
                        ("LIBRARY", Style::default().fg(Color::Magenta))
                    } else {
                        ("JOB", Style::default().fg(Color::DarkGray))
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
                    let total_size = dashboard
                        .downloads
                        .iter()
                        .filter(|entry| entry.job == job.name)
                        .fold(0u64, |total, entry| {
                            total.saturating_add(entry.total.unwrap_or(entry.bytes))
                        });
                    ListItem::new(vec![
                        Line::from(vec![
                            Span::raw(POSTER_INDENT),
                            Span::styled(if job.adhoc { "↳ " } else { "● " }, state_style(state)),
                            Span::styled(job.name.clone(), title_style),
                            Span::styled(format!("  {kind}"), kind_style),
                        ]),
                        Line::from(vec![
                            Span::raw(POSTER_INDENT),
                            Span::styled(format!("  {}", state.to_uppercase()), state_style(state)),
                            Span::styled(
                                format!("  {}", format_bytes(total_size)),
                                Style::default().fg(Color::Gray),
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
                let height = (downloads_area.height * 2 / 5).clamp(10, 16);
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
            let season_stats = |row: usize| {
                // Files and size from this heading up to the next one.
                let files: Vec<&DownloadEntry> = rows[row + 1..]
                    .iter()
                    .map_while(|row| match row {
                        FileRow::File(index) => downloads.get(*index),
                        FileRow::Season(_) => None,
                    })
                    .collect();
                let done = files.iter().filter(|entry| download_complete(&entry.status)).count();
                let size = files
                    .iter()
                    .fold(0u64, |total, entry| total.saturating_add(entry.total.unwrap_or(entry.bytes)));
                (files.len(), done, size)
            };
            let download_rows: Vec<ListItem> = rows
                .iter()
                .enumerate()
                .map(|(row, file_row)| {
                    let entry = match file_row {
                        FileRow::Season(label) => {
                            let (count, done, size) = season_stats(row);
                            let mut stats = format!(
                                "{count} {}",
                                if count == 1 { "file" } else { "files" }
                            );
                            if done < count {
                                stats.push_str(&format!(", {done} done"));
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
                    let (bar, percent) = download_progress(entry.bytes, entry.total, download_bar_width);
                    let completed = download_complete(&entry.status);
                    let size = if completed {
                        format_bytes(entry.bytes)
                    } else {
                        entry
                            .total
                            .map(|total| {
                                format!("{} / {}", format_bytes(entry.bytes), format_bytes(total))
                            })
                            .unwrap_or_else(|| format!("{} downloaded", format_bytes(entry.bytes)))
                    };
                    let mut subtitle = if completed {
                        vec![
                            Span::styled("  ✓ downloaded", Style::default().fg(Color::Green)),
                            Span::styled(format!("  {size}"), Style::default().fg(Color::Gray)),
                        ]
                    } else {
                        vec![
                            Span::styled(format!("  {}", entry.status), state_style(&entry.status)),
                            Span::styled(format!("  {size}"), Style::default().fg(Color::Gray)),
                        ]
                    };
                    if completed && let Some(Ok(probe)) = probe_cache.get(&entry.path) {
                        let used: usize = subtitle.iter().map(|span| span.content.chars().count()).sum();
                        let badges = probe.badges.join(" · ");
                        subtitle.push(Span::styled(
                            format!("  {}", truncate(&badges, download_text_width.saturating_sub(used + 2))),
                            Style::default().fg(Color::Blue),
                        ));
                    }
                    let third_line = if !completed {
                        Line::from(vec![
                            Span::styled("  ", Style::default()),
                            Span::styled(bar, Style::default().fg(Color::Cyan)),
                            Span::styled(format!(" {percent}"), Style::default().fg(Color::DarkGray)),
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
                    let mut first = vec![Span::styled("● ", state_style(&entry.status))];
                    first.extend(title_line);
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
                let sync_label = if sync_task.is_some() {
                    "SYNC RUNNING"
                } else {
                    "READY"
                };
                let mut header_segments: Vec<Vec<Span>> = vec![
                    vec![Span::styled(
                        "  ◆ JELLYSYNC",
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    )],
                    vec![Span::styled(
                        format!("{} mode", config.download.mode),
                        Style::default().fg(Color::White),
                    )],
                    vec![Span::styled(
                        format!("{} workers", config.parallelism),
                        Style::default().fg(Color::Gray),
                    )],
                    vec![Span::styled(
                        sync_label,
                        state_style(if sync_task.is_some() {
                            "running"
                        } else {
                            "success"
                        })
                        .add_modifier(Modifier::BOLD),
                    )],
                ];
                match online {
                    None => {}
                    Some(None) => header_segments
                        .push(vec![Span::styled("CHECKING", state_style("stopped"))]),
                    Some(Some(true)) => header_segments.push(vec![Span::styled(
                        "ONLINE",
                        state_style("success").add_modifier(Modifier::BOLD),
                    )]),
                    Some(Some(false)) => header_segments.push(vec![Span::styled(
                        "OFFLINE",
                        state_style("failed").add_modifier(Modifier::BOLD),
                    )]),
                }
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
                header_segments.push(vec![
                    Span::styled("timer ", Style::default().fg(Color::Gray)),
                    Span::styled(
                        dashboard.timer.clone(),
                        if dashboard.timer == "active" {
                            state_style("success")
                        } else {
                            state_style("stopped")
                        },
                    ),
                ]);
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
                let files_size = downloads
                    .iter()
                    .fold(0u64, |total, entry| total.saturating_add(entry.total.unwrap_or(entry.bytes)));
                let downloads_title = match selected_job {
                    None => "Files · no job selected".to_string(),
                    Some(job) => {
                        let mut title = format!(
                            "Files: {} · {} {}",
                            job.name.trim_start_matches("library:"),
                            downloads.len(),
                            if downloads.len() == 1 { "file" } else { "files" }
                        );
                        if seasons > 1 {
                            title.push_str(&format!(" · {seasons} seasons"));
                        }
                        if !downloads.is_empty() {
                            title.push_str(&format!(" · {}", format_bytes(files_size)));
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
                let job_list = List::new(entries)
                    .block(jobs_block)
                    .highlight_style(if !download_focus {
                        list_style
                    } else {
                        Style::default()
                    })
                    .highlight_symbol("▌ ");
                let download_list = List::new(download_rows)
                    .block(downloads_block)
                    .highlight_style(if download_focus {
                        list_style
                    } else {
                        Style::default()
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
                    render_job_details(
                        frame,
                        details_area,
                        &job.name,
                        job_details.get_mut(&job.name),
                        loading,
                        last_sync,
                        file_label.filter(|_| !still_downloading),
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

                if show_job_config {
                    let popup = centered_rect(76, 72, area);
                    let lines = if let Some(job) = jobs
                        .get(selected)
                        .and_then(|selected| config.jobs.iter().find(|job| job.name == selected.name))
                    {
                        vec![
                            config_line("Name", job.name.clone()),
                            config_line(
                                "Jellyfin",
                                job.jellyfin_name.clone().unwrap_or_else(|| job.name.clone()),
                            ),
                            config_line("Directory", job.directory.clone().unwrap_or_else(|| "—".into())),
                            config_line("Remote dir", job.remote_dir.clone().unwrap_or_else(|| "—".into())),
                            config_line("Local dir", job.local_dir.clone().unwrap_or_else(|| "—".into())),
                            config_line("Seasons", yaml_value_display(job.seasons.as_ref())),
                            config_line("Episodes", yaml_value_display(job.episodes.as_ref())),
                            config_line("Wildcard", optional_bool_display(job.wildcard)),
                            config_line("Unwatched", optional_bool_display(job.unwatched)),
                        ]
                    } else if let Some(job) = jobs.get(selected) {
                        vec![
                            config_line("Name", job.name.clone()),
                            config_line("Type", "Ad-hoc library download"),
                            config_line("Configured", "No static job configuration"),
                        ]
                    } else {
                        vec![Line::from("No job selected")]
                    };
                    frame.render_widget(Clear, popup);
                    frame.render_widget(
                        Paragraph::new(lines)
                            .block(panel_block("Job configuration · i / Esc close", true))
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
                        shortcuts.extend([("p", "play"), ("i", "info"), ("x", "clear")]);
                    } else {
                        shortcuts.extend([("s", "sync"), ("i", "config")]);
                    }
                    shortcuts.extend([("b", "browse"), ("?", "more"), ("q", "quit")]);
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
                                .unwrap_or_else(|| {
                                    "  Downloads follow the selected job; clear a file, season, or show."
                                        .into()
                                }),
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
                                } else {
                                    "Deletes these files from disk.  y confirm · n / Esc cancel"
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
                                            let mark = if explore.selected_episodes.contains(&index) { "✓ " } else { "  " };
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
                    let help_popup = centered_rect(84, 30, area);
                    let help_lines = vec![
                        Line::from(Span::styled(
                            "Main view",
                            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                        )),
                        Line::from("  ↑/↓ or click     select a job or download"),
                        Line::from("  PgUp/PgDn Home/End  jump through the list"),
                        Line::from("  Tab / ← / →       switch focus"),
                        Line::from("  Mouse drag        resize Jobs/Files split (wide terminals)"),
                        Line::from("  s / S             sync selected / all jobs"),
                        Line::from("  b                 browse the Jellyfin library"),
                        Line::from("  i                 job configuration (Jobs focused)"),
                        Line::from("  i / Enter         file media info (Files focused)"),
                        Line::from("  p / double-click  play selected file"),
                        Line::from("  Space             pause / resume selected download"),
                        Line::from("  o                 open the file's or show's directory"),
                        Line::from("  x / X / c         clear file / season / show (x on ad-hoc job removes it)"),
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
                if let Event::Key(key) = &input
                    && reply_filter.swallow(key)
                {
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
                            MouseEventKind::Drag(MouseButton::Left) if resizing_split => {
                                let relative = mouse.column.saturating_sub(body.x);
                                let ratio = relative.saturating_mul(100) / body.width.max(1);
                                main_split = ratio.clamp(20, 80);
                            }
                            MouseEventKind::Up(MouseButton::Left) => {
                                resizing_split = false;
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
                } else if show_job_config
                    && matches!(key.code, KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('i'))
                {
                    show_job_config = false;
                } else if key.code == KeyCode::Char('c')
                    && key.modifiers.contains(KeyModifiers::CONTROL)
                {
                    // Ctrl-C closes the topmost dialog first. Closing one does not count
                    // towards quitting: the next Ctrl-C is the first of the quitting pair.
                    if show_help || show_job_config || confirm_clear.is_some() || file_info.is_some() {
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
                } else if show_help || show_job_config || file_info.is_some() {
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
                                        Ok(()) => (format!("Queued {count} download(s); they continue after the TUI exits"), true),
                                        Err(error) => (format!("Queued {count} download(s) but could not start worker: {error:#}"), false),
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
                                clear_tracked(&config, &request.targets).and_then(|removed| {
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
                        KeyCode::Char('i') if !jobs.is_empty() => {
                            show_job_config = !show_job_config;
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
                        KeyCode::Char('x') if download_focus && !downloads.is_empty() => {
                            confirm_clear = selected_job.and_then(|job| {
                                clear_request(ClearScope::File, job, &downloads, &rows, selected_download)
                            });
                        }
                        KeyCode::Char('x')
                            if !download_focus
                                && jobs.get(selected).is_some_and(|job| job.adhoc) =>
                        {
                            confirm_clear = selected_job.and_then(|job| {
                                clear_request(ClearScope::Show, job, &downloads, &rows, selected_download)
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
                        KeyCode::Char('X') if download_focus && !downloads.is_empty() => {
                            confirm_clear = selected_job.and_then(|job| {
                                clear_request(ClearScope::Season, job, &downloads, &rows, selected_download)
                            });
                        }
                        KeyCode::Char('c') if !jobs.is_empty() => {
                            confirm_clear = selected_job.and_then(|job| {
                                clear_request(ClearScope::Show, job, &downloads, &rows, selected_download)
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

fn yaml_value_display(value: Option<&serde_yaml::Value>) -> String {
    value
        .map(|value| {
            serde_yaml::to_string(value)
                .unwrap_or_else(|_| "<invalid>".into())
                .trim()
                .replace('\n', " ")
        })
        .unwrap_or_else(|| "—".into())
}

fn optional_bool_display(value: Option<bool>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "—".into())
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
        rating.push(format!("★ {score:.1}"));
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
        facts.push(format!("★ {score:.1}"));
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
    downloads.sort_by_cached_key(|entry| {
        (
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
            rows.push(FileRow::Season(season_label(key.0, &folder)));
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
            if fact.starts_with('★') {
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
    let mut badges = Vec::new();
    if let Some(video) = of_type("video").first() {
        badges.push(resolution_label(
            number(video, "width").unwrap_or(0),
            number(video, "height").unwrap_or(0),
        ));
        if let Some(codec) = text(video, "codec_name") {
            badges.push(match codec.as_str() {
                "hevc" => "HEVC".to_string(),
                "h264" => "H.264".to_string(),
                other => other.to_uppercase(),
            });
        }
        match text(video, "color_transfer").as_deref() {
            Some("smpte2084") => badges.push("HDR10".into()),
            Some("arib-std-b67") => badges.push("HLG".into()),
            _ => {}
        }
    }
    let audio = of_type("audio");
    if let Some(track) = audio
        .iter()
        .find(|stream| disposition(stream, "default"))
        .or(audio.first())
    {
        badges.push(format!(
            "{} {}",
            text(track, "codec_name")
                .unwrap_or_else(|| "?".into())
                .to_uppercase(),
            channel_label(number(track, "channels"))
        ));
    }
    if audio.len() > 1 {
        badges.push(format!("{} audio", audio.len()));
    }
    let subtitles = of_type("subtitle").len();
    if subtitles > 0 {
        badges.push(format!(
            "{subtitles} {}",
            if subtitles == 1 { "sub" } else { "subs" }
        ));
    }
    badges
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
        rows.push(("Video".to_string(), parts.join(", ")));
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
        rows.push(("Audio".to_string(), audio.join(", ")));
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
        rows.push(("Subtitles".to_string(), subtitles.join(", ")));
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

fn download_progress(bytes: u64, total: Option<u64>, width: usize) -> (String, String) {
    let Some(total) = total.filter(|total| *total > 0) else {
        return ("─".repeat(width), "   ?".into());
    };
    let percent = ((bytes as f64 / total as f64) * 100.0).clamp(0.0, 100.0);
    let filled = ((percent / 100.0) * width as f64).round() as usize;
    (
        format!(
            "{}{}",
            "━".repeat(filled),
            "─".repeat(width.saturating_sub(filled))
        ),
        format!("{percent:3.0}%"),
    )
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
        Some(Commands::Status) => status(cli.json),
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
                Some(Commands::Download { target }) => {
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
                Some(Commands::Prune { apply, target }) => {
                    let result = prune(config, target.clone(), apply, cli.json).await;
                    if cli.json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&json!({
                                "command": "prune",
                                "targets": target,
                                "apply": apply,
                                "ok": result.is_ok(),
                                "error": result.as_ref().err().map(|error| format!("{error:#}")),
                            }))?
                        );
                    }
                    result
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
                | Some(Commands::Status | Commands::Start | Commands::Stop | Commands::Tui) => {
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
            parse_season(Path::new("/movies/Heat (1995)/Heat.mkv")),
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
        assert_eq!(
            media_badges(&probe),
            ["4K", "HEVC", "HDR10", "EAC3 5.1", "2 audio", "1 sub"]
        );
        let rows = media_summary(&probe);
        assert!(
            rows.iter()
                .any(|(label, value)| label == "Video" && value.contains("HDR10"))
        );
        assert_eq!(rows.iter().filter(|(label, _)| label == "Video").count(), 1);
    }

    #[test]
    fn movies_are_not_grouped() {
        let mut files = vec![entry("a", "/m/Heat (1995)/Heat.mkv")];
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
            title: "Heat".into(),
            years: Some("1995".into()),
            facts: Vec::new(),
            tagline: None,
            overview: String::new(),
            episodes: HashMap::new(),
            poster: None,
        };
        let file = entry("m", "/m/Heat/Heat.mkv");
        assert_eq!(
            file_title(Some(&details), &file, true),
            Some((String::new(), "Heat (1995)".into()))
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
            entry("a", "/m/Heat/Heat.mkv"),
            entry("b", "/m/Heat/Heat-extras.mkv"),
        ];
        // An ungrouped list, as for a movie folder.
        let rows = vec![FileRow::File(0), FileRow::File(1)];
        assert_eq!(season_files(&rows, &files, 1), [0, 1]);
    }

    #[test]
    fn clear_requests_capture_their_targets() {
        let (files, rows) = season_fixture();
        let job = TuiJob {
            name: "library:Show".into(),
            adhoc: true,
        };
        let request = clear_request(ClearScope::Season, &job, &files, &rows, 1).unwrap();
        assert_eq!(request.targets.len(), 2);
        assert_eq!(request.label, "Show · Season 1 (2 files)");
        let request = clear_request(ClearScope::File, &job, &files, &rows, 2).unwrap();
        assert_eq!(request.targets[0].item_id, "c");
        let request = clear_request(ClearScope::Show, &job, &files, &rows, 0).unwrap();
        assert_eq!(request.label, "Show · all 3 files");
        // An empty ad-hoc job can still be removed; an empty configured job cannot.
        assert!(
            clear_request(ClearScope::Show, &job, &[], &[], 0)
                .is_some_and(|r| r.targets.is_empty())
        );
        let configured = TuiJob {
            name: "Show".into(),
            adhoc: false,
        };
        assert!(clear_request(ClearScope::Show, &configured, &[], &[], 0).is_none());
        assert!(clear_request(ClearScope::File, &configured, &[], &[], 0).is_none());
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
        for spec in ["latestX", "latest-0", "5-3", "one", "[]", "[true]", "[-1]"] {
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
        }
    }

    #[test]
    fn exact_titles_win_over_substrings() {
        let items = vec![
            media("Heat Wave", "Series", None),
            media("Heat", "Movie", None),
        ];
        assert_eq!(find_one(items, "heat").unwrap().unwrap().name, "Heat");
        let items = vec![media("Heat Wave", "Series", None)];
        assert_eq!(find_one(items, "Heat").unwrap().unwrap().name, "Heat Wave");
        let items = vec![
            media("Heat Wave", "Series", None),
            media("Heat 2", "Movie", None),
        ];
        assert!(find_one(items, "Heat").is_err());
    }

    #[test]
    fn destinations_handle_server_paths() {
        let root = Path::new("/local/Show");
        let item = media("Ep", "Episode", Some("D:\\TV\\Show\\Show - S02E03.mkv"));
        assert_eq!(
            item_destination(&item, root, "Season $season_number", "Show", false).unwrap(),
            PathBuf::from("/local/Show/Season 2/Show - S02E03.mkv")
        );
        let item = media("Movie", "Movie", Some("/movies/Heat/Heat.mkv"));
        assert_eq!(
            item_destination(
                &item,
                Path::new("/local/Heat"),
                "Season $season_number",
                "Heat",
                true
            )
            .unwrap(),
            PathBuf::from("/local/Heat/Heat.mkv")
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
}
