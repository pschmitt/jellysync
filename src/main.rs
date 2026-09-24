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

#[derive(Parser)]
#[command(name = "jellysync", version, about = "Sync Jellyfin media")]
struct Cli {
    #[arg(short, long, global = true)]
    config: Option<PathBuf>,
    #[arg(long, global = true)]
    parallelism: Option<usize>,
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Option<Commands>,
    /// Legacy form: jellysync [JOB ...] is treated as jellysync sync [JOB ...].
    targets: Vec<String>,
}

#[derive(Subcommand)]
enum Commands {
    Sync {
        target: Vec<String>,
    },
    Status,
    Prune {
        #[arg(long)]
        apply: bool,
        target: Vec<String>,
    },
    Config,
    Start,
    Stop,
    Tui,
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
    if let Some(rest) = value.strip_prefix('~') {
        format!("{home}{rest}")
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
    let mut value = raw.replace("$name", &job.name);
    for (key, path) in directories {
        value = value.replace(&format!("{}{}", "$", key), path);
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

async fn download_library_items(
    config: Config,
    items: Vec<MediaItem>,
    title: String,
    movie: bool,
) -> Result<usize> {
    if items.is_empty() {
        bail!("select at least one item to download");
    }
    let credentials = config
        .jellyfin
        .as_ref()
        .context("dynamic downloads require Jellyfin credentials")?;
    let api = jellyfin_login(credentials).await?;
    let job = format!("library:{title}");
    let mut folder = PathBuf::from(&config.local.root);
    folder.push(if movie { "Movies" } else { "TV Shows" });
    folder.push(safe_component(&title));
    std::fs::create_dir_all(&folder)?;
    update_job(
        &job,
        "running",
        &format!("{} selected item(s)", items.len()),
    )?;
    let count = items.len();
    let slots = Arc::new(Semaphore::new(config.parallelism.max(1)));
    let mut tasks = JoinSet::new();
    for item in items {
        let api = api.clone();
        let job = job.clone();
        let folder = folder.clone();
        let title = title.clone();
        let permit = slots.clone().acquire_owned().await?;
        tasks.spawn(async move {
            let _permit = permit;
            let destination =
                item_destination(&item, &folder, "Season $season_number", &title, movie)?;
            jellyfin_download(&api, &job, &item, &destination, true).await
        });
    }
    let mut failures = Vec::new();
    while let Some(result) = tasks.join_next().await {
        if let Err(error) = result.context("dynamic Jellyfin transfer worker failed")? {
            failures.push(format!("{error:#}"));
        }
    }
    if !failures.is_empty() {
        update_job(&job, "failed", &failures.join("; "))?;
        bail!(
            "{} dynamic download(s) failed: {}",
            failures.len(),
            failures.join("; ")
        );
    }
    update_job(&job, "success", &format!("{count} item(s) downloaded"))?;
    Ok(count)
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

fn number_filter(
    spec: Option<&serde_yaml::Value>,
    latest: Option<u32>,
) -> Result<Option<Vec<u32>>> {
    let Some(spec) = spec else {
        return Ok(None);
    };
    if let Some(values) = spec.as_sequence() {
        return Ok(Some(
            values
                .iter()
                .filter_map(serde_yaml::Value::as_u64)
                .map(|n| n as u32)
                .collect(),
        ));
    }
    if let Some(value) = spec.as_u64() {
        return Ok(Some(vec![value as u32]));
    }
    let spec = spec
        .as_str()
        .context("filter must be a string or list of numbers")?;
    if spec == "all" {
        return Ok(None);
    }
    if let Some(count) = spec.strip_prefix("latest") {
        let latest = latest.context("latest filter has no available media")?;
        let count = count
            .strip_prefix('-')
            .map(str::parse::<u32>)
            .transpose()?
            .unwrap_or(1);
        if count == 0 {
            bail!("latest filter count must be positive");
        }
        let selected = latest.saturating_sub(count - 1)..=latest;
        return Ok(Some(selected.collect()));
    }
    if let Some((first, last)) = spec.split_once('-') {
        let first = first.parse::<u32>().context("invalid filter range start")?;
        let last = last.parse::<u32>().context("invalid filter range end")?;
        if last < first {
            bail!("filter range end must be greater than or equal to start");
        }
        return Ok(Some((first..=last).collect()));
    }
    if spec.contains(',') {
        return Ok(Some(
            spec.split(',')
                .map(|v| v.trim().parse::<u32>())
                .collect::<std::result::Result<Vec<_>, _>>()?,
        ));
    }
    Ok(Some(vec![
        spec.parse::<u32>().context("invalid filter number")?,
    ]))
}

async fn jellyfin_item_ids(api: &JellyfinApi, job: &Job) -> Result<HashSet<String>> {
    let jellyfin_name = job.jellyfin_name.as_deref().unwrap_or(&job.name);
    let series = jellyfin_items(
        api,
        &[
            ("Recursive", "true"),
            ("IncludeItemTypes", "Series"),
            ("SearchTerm", jellyfin_name),
            ("Fields", "Path"),
            ("Limit", "100"),
        ],
    )
    .await?;
    let items = if let Some(series) = find_one(series, jellyfin_name)? {
        jellyfin_items(
            api,
            &[
                ("ParentId", series.id.as_str()),
                ("Recursive", "true"),
                ("IncludeItemTypes", "Episode"),
                ("Fields", "Path"),
                ("Limit", "10000"),
            ],
        )
        .await?
    } else {
        let movies = jellyfin_items(
            api,
            &[
                ("Recursive", "true"),
                ("IncludeItemTypes", "Movie"),
                ("SearchTerm", jellyfin_name),
                ("Fields", "Path"),
                ("Limit", "100"),
            ],
        )
        .await?;
        find_one(movies, jellyfin_name)?.into_iter().collect()
    };
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
            eprintln!("{}", message);
        }
        return Ok(());
    }
    let count = items.len();
    let destination = PathBuf::from(resolved_path(config, job, false)?);
    std::fs::create_dir_all(&destination)?;
    update_job(&job.name, "running", &format!("{count} item(s)"))?;
    let pattern = config
        .library
        .as_ref()
        .map(|l| l.season_pattern.clone())
        .unwrap_or_else(default_season_pattern);
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
    let found = jellyfin_items(
        &api,
        &[
            ("Recursive", "true"),
            ("IncludeItemTypes", "Series"),
            ("SearchTerm", jellyfin_name),
            ("Fields", "Path"),
            ("Limit", "100"),
        ],
    )
    .await?;
    let series = find_one(found, jellyfin_name)?;
    let movie_mode;
    let mut items = if let Some(series) = series {
        movie_mode = false;
        jellyfin_items(
            &api,
            &[
                ("ParentId", series.id.as_str()),
                ("Recursive", "true"),
                ("IncludeItemTypes", "Episode"),
                ("Fields", "Path,UserData,ParentIndexNumber,IndexNumber"),
                ("Limit", "10000"),
            ],
        )
        .await?
    } else {
        movie_mode = true;
        let found = jellyfin_items(
            &api,
            &[
                ("Recursive", "true"),
                ("IncludeItemTypes", "Movie"),
                ("SearchTerm", jellyfin_name),
                ("Fields", "Path,UserData"),
                ("Limit", "100"),
            ],
        )
        .await?;
        find_one(found, jellyfin_name)?.into_iter().collect()
    };
    let max_season = items
        .iter()
        .filter_map(|item| item.parent_index_number)
        .max();
    if let Some(selected) = number_filter(job.seasons.as_ref(), max_season)? {
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
            let max_episode = season.iter().filter_map(|item| item.index_number).max();
            if let Some(selected) = number_filter(Some(filter), max_episode)? {
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
    let filename = Path::new(path)
        .file_name()
        .context("Jellyfin media path has no filename")?;
    let mut folder = root.to_path_buf();
    if !movie && let Some(season) = item.parent_index_number {
        let season = pattern
            .replace("$season_number", &season.to_string())
            .replace("$name", show);
        if season.contains('/') || season.contains('\\') || season == ".." {
            bail!("season pattern must produce one directory name");
        }
        folder.push(season);
    }
    let filename = filename.to_string_lossy();
    if filename == "." || filename == ".." || filename.contains('/') || filename.contains('\\') {
        bail!("Jellyfin returned an unsafe filename");
    }
    Ok(folder.join(filename.as_ref()))
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
            update_transfer(
                &item.id,
                job,
                destination,
                "interrupted",
                bytes,
                transfer_total(&item.id)?,
            )?;
            Err(error)
        }
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
        return Ok(());
    }
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
    let total = response
        .headers()
        .get("Content-Range")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.rsplit('/').next())
        .and_then(|v| v.parse::<u64>().ok())
        .or_else(|| response.content_length().map(|length| start + length));
    let mut file = if append {
        tokio::fs::OpenOptions::new()
            .append(true)
            .open(&partial)
            .await?
    } else {
        tokio::fs::File::create(&partial).await?
    };
    update_transfer(&item.id, job, destination, "downloading", start, total)?;
    let mut stream = response.bytes_stream();
    let mut bytes = start;
    let mut checkpoint = start;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("read Jellyfin download stream")?;
        tokio::io::AsyncWriteExt::write_all(&mut file, &chunk).await?;
        bytes += chunk.len() as u64;
        if bytes - checkpoint >= 4 * 1024 * 1024 {
            update_transfer(&item.id, job, destination, "downloading", bytes, total)?;
            checkpoint = bytes;
        }
    }
    tokio::io::AsyncWriteExt::flush(&mut file).await?;
    file.sync_all().await?;
    drop(file);
    tokio::fs::rename(&partial, destination).await?;
    update_transfer(
        &item.id,
        job,
        destination,
        "complete",
        bytes,
        total.or(Some(bytes)),
    )?;
    if !quiet {
        println!("downloaded {}", destination.display());
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
        println!("synced {} -> {}", job.name, destination);
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
            "running" | "downloading" | "queued" => ("◐", TerminalColor::Yellow),
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
    let mut statement = conn.prepare("SELECT job,path,status,bytes,total FROM transfers WHERE status IN ('queued','downloading','interrupted') ORDER BY job,path")?;
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
            "queued" => TerminalColor::Yellow,
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
    let mut statement = conn.prepare("SELECT job,path,status,bytes,total FROM transfers WHERE status IN ('queued','downloading','interrupted') ORDER BY job,path")?;
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
                    clear_tracked(&config, entry, ClearScope::File)?;
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
#[derive(Clone, Copy)]
enum ClearScope {
    File,
    Season,
    Show,
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

fn dashboard_snapshot() -> Result<DashboardSnapshot> {
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
    let timer = std::process::Command::new("systemctl")
        .args(["--user", "is-active", "jellysync.timer"])
        .output()
        .ok()
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|state| !state.is_empty())
        .unwrap_or_else(|| "unknown".into());
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

fn clear_tracked(config: &Config, selected: &DownloadEntry, scope: ClearScope) -> Result<usize> {
    let root = PathBuf::from(&config.local.root)
        .canonicalize()
        .context("local root must exist before clearing downloads")?;
    let parent = selected.path.parent().map(Path::to_path_buf);
    let targets: Vec<_> = tracked_downloads()?
        .into_iter()
        .filter(|entry| match scope {
            ClearScope::File => entry.item_id == selected.item_id,
            ClearScope::Season => {
                entry.job == selected.job && entry.path.parent() == parent.as_deref()
            }
            ClearScope::Show => entry.job == selected.job,
        })
        .collect();
    let conn = db()?;
    let job_names: HashSet<_> = targets.iter().map(|entry| entry.job.clone()).collect();
    for entry in &targets {
        let partial = PathBuf::from(format!("{}.partial", entry.path.to_string_lossy()));
        for path in [&entry.path, &partial] {
            if path.exists() {
                let canonical = path.canonicalize()?;
                if canonical == root || !canonical.starts_with(&root) {
                    bail!("refusing to clear a tracked file outside the configured local root");
                }
                std::fs::remove_file(canonical)?;
            }
        }
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

async fn tui(config: Config) -> Result<()> {
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
        let query_started = Instant::now();
        let picker = Picker::from_query_stdio().unwrap_or_else(|_| Picker::from_fontsize((8, 16)));
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
        let mut confirm_clear = None;
        let mut show_help = false;
        let mut show_job_config = false;
        let mut file_info: Option<FileInfo> = None;
        let mut file_info_task: Option<FileInfoTask> = None;
        let mut file_meta: Option<FileMeta> = None;
        let mut file_meta_task: Option<FileMetaTask> = None;
        let mut jellyfin_api: Option<JellyfinApi> = None;
        let mut explore: Option<ExploreState> = None;
        let mut explore_notice: Option<(String, bool)> = None;
        let mut explore_task: Option<tokio::task::JoinHandle<Result<usize>>> = None;
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
        let mut explore_poster_tasks = HashMap::<String, PosterTask>::new();
        let mut explore_poster_protocols = HashMap::<String, StatefulProtocol>::new();
        let mut preview_episode_task: Option<PreviewEpisodeTask> = None;
        let mut poster_task: Option<PosterTask> = None;
        let mut sync_task: Option<tokio::task::JoinHandle<Result<()>>> = None;
        let mut sync_notice = None;
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
        let mut dashboard_task =
            Some(tokio::task::spawn_blocking(dashboard_snapshot));
        let mut dashboard_refreshed_at = Instant::now();
        loop {
            terminal.autoresize()?;
            if dashboard_task
                .as_ref()
                .is_some_and(|task| task.is_finished())
            {
                let task = dashboard_task
                    .take()
                    .expect("finished dashboard task exists");
                match task.await {
                    Ok(Ok(snapshot)) => dashboard = snapshot,
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
                dashboard_task = Some(tokio::task::spawn_blocking(dashboard_snapshot));
            }
            if ping_task.as_ref().is_some_and(|task| task.is_finished()) {
                let task = ping_task.take().expect("finished ping task exists");
                let reachable = task.await.unwrap_or(false);
                let was_online = online == Some(Some(true));
                let was_offline = online == Some(Some(false));
                online = Some(Some(reachable));
                pinged_at = Some(Instant::now());
                if reachable && !was_online {
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
                    for (job_name, item) in posters {
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
            let finished_explore_posters: Vec<_> = explore_poster_tasks
                .iter()
                .filter_map(|(item_id, task)| task.is_finished().then_some(item_id.clone()))
                .collect();
            for item_id in finished_explore_posters {
                let task = explore_poster_tasks
                    .remove(&item_id)
                    .expect("finished Explore poster task exists");
                if let Ok((item_id, Ok(Some((data, protocol))))) = task.await {
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
            if explore_task.as_ref().is_some_and(|task| task.is_finished()) {
                let task = explore_task.take().expect("finished Explore task exists");
                let result = task.await;
                if let Some(task) = job_poster_catalog_task.take() {
                    task.abort();
                }
                let config = config.clone();
                job_poster_catalog_task = Some(tokio::spawn(async move {
                    jellyfin_job_poster_catalog(&config).await
                }));
                explore_notice = Some(match result {
                    Ok(Ok(count)) => (format!("Queued {count} library download(s)"), true),
                    Ok(Err(error)) => (format!("Explore download failed: {error:#}"), false),
                    Err(error) => (format!("Explore download stopped: {error}"), false),
                });
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
            if selected >= jobs.len() && !jobs.is_empty() {
                selected = jobs.len() - 1;
            }
            let selected_job = jobs.get(selected);
            let downloads: Vec<_> = dashboard
                .downloads
                .iter()
                .filter(|entry| selected_job.is_some_and(|job| entry.job == job.name))
                .cloned()
                .collect();
            if downloads.is_empty() {
                selected_download = 0;
            } else if selected_download >= downloads.len() {
                selected_download = downloads.len() - 1;
            }
            let entries: Vec<ListItem> = jobs
                .iter()
                .map(|job| {
                    let (state, message, time) = history
                        .get(&job.name)
                        .map(|(state, message, time)| {
                            (state.as_str(), message.as_str(), time.as_str())
                        })
                        .unwrap_or(("not run", "Waiting for first sync", ""));
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
                            Span::styled(
                                if time.is_empty() {
                                    String::new()
                                } else {
                                    format!("  {time}")
                                },
                                Style::default()
                                    .fg(Color::DarkGray)
                                    .add_modifier(Modifier::ITALIC),
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
            // Borders, the "▌ " highlight symbol and the "● " bullet.
            let download_text_width = usize::from(downloads_area.width.saturating_sub(6)).max(8);
            // Leading indent plus the " 100%" suffix.
            let download_bar_width = download_text_width.saturating_sub(7).clamp(8, 60);
            let download_rows: Vec<ListItem> = downloads
                .iter()
                .map(|entry| {
                    let filename = entry.path.file_name().unwrap_or_default().to_string_lossy();
                    let (bar, percent) = download_progress(entry.bytes, entry.total, download_bar_width);
                    let completed = matches!(
                        entry.status.as_str(),
                        "complete" | "completed" | "success"
                    );
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
                    let subtitle_line = if completed {
                        Line::from(vec![
                            Span::styled("  ✓ downloaded", Style::default().fg(Color::Green)),
                            Span::styled(format!("  {size}"), Style::default().fg(Color::Gray)),
                        ])
                    } else {
                        Line::from(vec![
                            Span::styled(format!("  {}", entry.status), state_style(&entry.status)),
                            Span::styled(format!("  {size}"), Style::default().fg(Color::Gray)),
                        ])
                    };
                    let progress_line = if completed {
                        Line::from("")
                    } else {
                        Line::from(vec![
                            Span::styled("  ", Style::default()),
                            Span::styled(bar, Style::default().fg(Color::Cyan)),
                            Span::styled(format!(" {percent}"), Style::default().fg(Color::DarkGray)),
                        ])
                    };
                    ListItem::new(vec![
                        Line::from(vec![
                            Span::styled("● ", state_style(&entry.status)),
                            Span::styled(
                                truncate_near_end(&filename, download_text_width),
                                Style::default()
                                    .fg(Color::White)
                                    .add_modifier(Modifier::BOLD),
                            ),
                        ]),
                        subtitle_line,
                        progress_line,
                    ])
                })
                .collect();
            terminal.draw(|frame| {
                let sync_label = if sync_task.is_some() {
                    "SYNC RUNNING"
                } else {
                    "READY"
                };
                let header_text = Line::from(vec![
                    Span::styled(
                        "  ◆ JELLYSYNC",
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled("  ·  ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        format!("{} mode", config.download.mode),
                        Style::default().fg(Color::White),
                    ),
                    Span::styled(
                        format!("  ·  {} workers", config.parallelism),
                        Style::default().fg(Color::Gray),
                    ),
                    Span::styled("  ·  ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        sync_label,
                        state_style(if sync_task.is_some() {
                            "running"
                        } else {
                            "success"
                        })
                        .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        if online.is_some() { "  ·  " } else { "" },
                        Style::default().fg(Color::DarkGray),
                    ),
                    match online {
                        None => Span::raw(""),
                        Some(None) => Span::styled("CHECKING", state_style("stopped")),
                        Some(Some(true)) => Span::styled(
                            "ONLINE",
                            state_style("success").add_modifier(Modifier::BOLD),
                        ),
                        Some(Some(false)) => Span::styled(
                            "OFFLINE",
                            state_style("failed").add_modifier(Modifier::BOLD),
                        ),
                    },
                    if reconcile_task.is_some() {
                        Span::styled("  ·  indexing…", Style::default().fg(Color::Gray))
                    } else if let Some(count) = indexed.filter(|count| *count > 0) {
                        Span::styled(format!("  ·  {count} indexed"), Style::default().fg(Color::Gray))
                    } else {
                        Span::raw("")
                    },
                    Span::styled("  ·  timer ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        dashboard.timer.clone(),
                        if dashboard.timer == "active" {
                            state_style("success")
                        } else {
                            state_style("stopped")
                        },
                    ),
                ]);
                frame.render_widget(Paragraph::new(header_text), header);

                let jobs_title = format!("Jobs · {}", jobs.len());
                let downloads_title = format!(
                    "Files: {} ({} {})",
                    selected_job.map_or("no job selected", |job| job.name.as_str()),
                    downloads.len(),
                    if downloads.len() == 1 { "file" } else { "files" }
                );
                let jobs_block = panel_block(&jobs_title, !download_focus);
                let downloads_block = panel_block(&downloads_title, download_focus);
                if !jobs.is_empty() {
                    jobs_state.select(Some(selected));
                }
                if !downloads.is_empty() {
                    downloads_state.select(Some(selected_download));
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
                frame.render_stateful_widget(download_list, downloads_area, &mut downloads_state);
                // Each file row is 3 lines tall.
                let visible_files = usize::from(downloads_area.height.saturating_sub(2)) / 3;
                if downloads.len() > visible_files {
                    let mut scrollbar_state = ScrollbarState::new(
                        downloads.len().saturating_sub(visible_files),
                    )
                    .position(downloads_state.offset());
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
                    };
                    if let Some(still) = still
                        && still_width > 0
                    {
                        frame.render_stateful_widget(
                            StatefulImage::default().resize(Resize::Fit(Some(FilterType::Triangle))),
                            Rect { x: inner.x, y: inner.y, width: still_width - 1, height: rows_area.height.min(9) },
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
                            },
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
                    let shortcuts = if area.width < 110 {
                        vec![
                            ("Tab", "focus"),
                            ("s", "sync"),
                            ("b", "browse"),
                            ("p", "play"),
                            ("i", "info"),
                            ("?", "more"),
                            ("q", "quit"),
                        ]
                    } else {
                        vec![
                            ("Tab", "focus"),
                            ("↑/↓", "move"),
                            ("s", "sync"),
                            ("b", "browse"),
                            ("p", "play"),
                            ("i", "config"),
                            ("?", "more"),
                            ("q", "quit"),
                        ]
                    };
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
                if confirm_clear.is_some() {
                    let popup = centered_rect(54, 5, area);
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
                        Paragraph::new("This removes the selected tracked media files.")
                            .style(Style::default().fg(Color::White))
                            .alignment(ratatui::layout::Alignment::Center),
                        Rect {
                            x: popup.x + 1,
                            y: popup.y + 2,
                            width: popup.width.saturating_sub(2),
                            height: 1,
                        },
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
                                            if item.item_type.as_deref() == Some("Series") { "Show contents · Enter open" } else { "Movie · d download" },
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
                                        "Library · {} · {} · / search · f type",
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
                                            "Tab/→/Enter select episodes · d download movie"
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
                                            "Press d to download it.",
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
                            "{}  ·  ↑/↓ navigate  ·  Tab switch pane  ·  d download  ·  Esc {}",
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
                    let help_popup = centered_rect(68, 58, area);
                    let help_lines = vec![
                        Line::from(Span::styled(
                            "Main view",
                            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                        )),
                        Line::from("  ↑/↓ or click     select a job or download"),
                        Line::from("  Tab / ← / →       switch focus"),
                        Line::from("  Mouse drag        resize Jobs/Details split"),
                        Line::from("  s / S             sync selected / all jobs"),
                        Line::from("  b                 browse the Jellyfin library"),
                        Line::from("  i / Enter         job configuration / file media info"),
                        Line::from("  p / double-click  play selected file"),
                        Line::from("  o                 open the file's or show's directory"),
                        Line::from("  x / X / c         clear file / season / show (x on ad-hoc job removes it)"),
                        Line::from("  Ctrl-C, Ctrl-C    quit the TUI"),
                        Line::from(""),
                        Line::from(Span::styled(
                            "Explore",
                            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                        )),
                        Line::from("  ↑/↓               move through library or episodes"),
                        Line::from("  /                 search library"),
                        Line::from("  f                 cycle type filter"),
                        Line::from("  Tab / ← / → / Enter  switch library / details pane"),
                        Line::from("  Space / a         select episode / select all"),
                        Line::from("  d                 download selected media"),
                        Line::from("  Ctrl-C or Esc     close Explore"),
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
            if event::poll(Duration::from_millis(250))? {
                let input = event::read()?;
                if let Event::Resize(_, _) = input {
                    terminal.autoresize()?;
                    terminal.clear()?;
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
                    } else if confirm_clear.is_none() {
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
                                } else if let Some(index) = list_index_at(
                                    downloads_area,
                                    mouse.column,
                                    mouse.row,
                                    downloads_state.offset(),
                                    downloads.len(),
                                    3,
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
                if key.code == KeyCode::Char('?') {
                    show_help = !show_help;
                    show_job_config = false;
                } else if show_help && key.code == KeyCode::Esc {
                    show_help = false;
                } else if file_info.is_some() && key.code == KeyCode::Char('p') {
                    if let Some(entry) = downloads.get(selected_download) {
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
                } else if show_job_config && key.code == KeyCode::Esc {
                    show_job_config = false;
                } else if show_job_config && key.code == KeyCode::Char('q') {
                    show_job_config = false;
                } else if key.code == KeyCode::Char('c')
                    && key.modifiers.contains(KeyModifiers::CONTROL)
                {
                    if explore.is_some() {
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
                        sync_notice = Some(("Press Ctrl-C again to quit".into(), false));
                    }
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
                        KeyCode::Esc => {
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
                        KeyCode::Char('f') if !browser.details_focus => {
                            browser.type_filter = browser.type_filter.next();
                            browser.selected = 0;
                            explore_notice = None;
                        }
                        KeyCode::Char('d') => {
                            if explore_task.is_some() {
                                explore_notice = Some(("A library download is already running".into(), false));
                            } else if browser.details_focus
                                && let Some(title) = browser
                                    .filtered_items()
                                    .get(browser.selected)
                                    .map(|item| item.name.clone())
                            {
                                let items: Vec<_> = browser.selected_episodes.iter()
                                    .filter_map(|index| browser.episodes.get(*index).cloned())
                                    .collect();
                                if items.is_empty() {
                                    explore_notice = Some(("Select episodes with Space or a first".into(), false));
                                } else {
                                    let config = config.clone();
                                    explore_notice = Some((format!("Starting {} download(s) for {title}", items.len()), true));
                                    explore_task = Some(tokio::spawn(async move {
                                        download_library_items(config, items, title, false).await
                                    }));
                                }
                            } else if let Some(item) = browser.filtered_items().get(browser.selected).map(|item| (*item).clone())
                                && item.item_type.as_deref() == Some("Movie")
                            {
                                let title = item.name.clone();
                                let config = config.clone();
                                explore_notice = Some((format!("Starting download for {title}"), true));
                                explore_task = Some(tokio::spawn(async move {
                                    download_library_items(config, vec![item], title, true).await
                                }));
                            }
                        }
                        KeyCode::Char(character) if !browser.details_focus && !character.is_control() => {
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
                } else if let Some(scope) = confirm_clear {
                    last_ctrl_c = None;
                    match key.code {
                        KeyCode::Char('y') => {
                            let (entry, scope) = if download_focus {
                                (downloads.get(selected_download), scope)
                            } else {
                                (
                                    jobs.get(selected).and_then(|job| {
                                        downloads.iter().find(|entry| entry.job == job.name)
                                    }),
                                    ClearScope::Show,
                                )
                            };
                            let result = if let Some(entry) = entry {
                                clear_tracked(&config, entry, scope).and_then(|removed| {
                                    // Ad-hoc jobs are dropped by clear_tracked once empty;
                                    // recording a status here would resurrect them.
                                    if matches!(scope, ClearScope::Show)
                                        && !entry.job.starts_with("library:")
                                    {
                                        update_job(
                                            &entry.job,
                                            "cleared",
                                            &format!("cleared {removed} tracked files"),
                                        )?;
                                    }
                                    Ok(())
                                })
                            } else if let Some(job) = jobs.get(selected).filter(|job| job.adhoc) {
                                forget_job(&job.name)
                            } else {
                                Ok(())
                            };
                            if let Err(error) = result {
                                sync_notice = Some((format!("Could not clear: {error:#}"), false));
                            }
                            confirm_clear = None;
                        }
                        KeyCode::Char('n') | KeyCode::Esc => confirm_clear = None,
                        _ => {}
                    }
                } else {
                    last_ctrl_c = None;
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => break,
                        KeyCode::Char('i') | KeyCode::Enter if download_focus && !downloads.is_empty() => {
                            if let Some(entry) = downloads.get(selected_download) {
                                let path = entry.path.clone();
                                file_info = Some((
                                    path.file_name()
                                        .unwrap_or_default()
                                        .to_string_lossy()
                                        .into_owned(),
                                    None,
                                ));
                                file_info_task = Some(tokio::spawn(probe_media(path)));
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
                            if explore_task.is_some() {
                                explore_notice = Some(("Wait for the current library download to finish".into(), false));
                            } else if let Some((api, items)) = library_cache.clone() {
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
                        KeyCode::Char('x') if download_focus && !downloads.is_empty() => {
                            confirm_clear = Some(ClearScope::File)
                        }
                        KeyCode::Char('x')
                            if !download_focus
                                && jobs.get(selected).is_some_and(|job| job.adhoc) =>
                        {
                            confirm_clear = Some(ClearScope::Show)
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
                            confirm_clear = Some(ClearScope::Season)
                        }
                        KeyCode::Char('c') if download_focus && !downloads.is_empty() => {
                            confirm_clear = Some(ClearScope::Show)
                        }
                        KeyCode::Char('c') if !jobs.is_empty() => {
                            download_focus = false;
                            confirm_clear = Some(ClearScope::Show);
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
        "running" | "downloading" | "queued" => Color::Yellow,
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

async fn probe_media(path: PathBuf) -> Result<MediaSummary> {
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
    Ok(media_summary(&probe))
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
            let channels = match number(stream, "channels").map(|c| c as u64) {
                Some(1) => "mono".to_string(),
                Some(2) => "2.0".to_string(),
                Some(6) => "5.1".to_string(),
                Some(8) => "7.1".to_string(),
                Some(n) => format!("{n}ch"),
                None => "?".to_string(),
            };
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
        Span::styled(
            format!("{key} "),
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(label, Style::default().fg(Color::Gray)),
    ]
}

fn key_sep() -> Span<'static> {
    Span::styled("│", Style::default().fg(Color::DarkGray))
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
    match cli.command {
        Some(Commands::Status) => status(cli.json),
        Some(Commands::Start) => systemctl("start", cli.json),
        Some(Commands::Stop) => systemctl("stop", cli.json),
        Some(Commands::Tui) if cli.json => bail!("--json is not supported with tui"),
        Some(Commands::Tui) => {
            let path = config_path(cli.config)?;
            let config =
                load_config(&path).with_context(|| format!("load config {}", path.display()))?;
            tui(config).await
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
                Some(Commands::Sync { target }) => {
                    let result = run_sync_mode(config, target.clone(), parallelism, cli.json).await;
                    if cli.json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&json!({
                                "command": "sync",
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
                None => {
                    let targets = cli.targets;
                    let result =
                        run_sync_mode(config, targets.clone(), parallelism, cli.json).await;
                    if cli.json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&json!({
                                "command": "sync",
                                "targets": targets,
                                "ok": result.is_ok(),
                                "error": result.as_ref().err().map(|error| format!("{error:#}")),
                            }))?
                        );
                    }
                    result
                }
                Some(Commands::Status | Commands::Start | Commands::Stop | Commands::Tui) => {
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
