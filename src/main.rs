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
use crossterm::{
    event::{self, Event, KeyCode, MouseButton, MouseEventKind},
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
    widgets::{Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph},
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

struct ExploreState {
    api: Option<JellyfinApi>,
    items: Vec<MediaItem>,
    episodes: Vec<MediaItem>,
    selected: usize,
    episode_cursor: usize,
    selected_episodes: HashSet<usize>,
    search: String,
    series_title: Option<String>,
    poster_id: Option<String>,
    poster_loading: bool,
    poster_loaded: bool,
    catalog_loading: bool,
    episode_loading: bool,
}

impl ExploreState {
    fn filtered_items(&self) -> Vec<&MediaItem> {
        self.items
            .iter()
            .filter(|item| {
                self.search.is_empty()
                    || item
                        .name
                        .to_lowercase()
                        .contains(&self.search.to_lowercase())
            })
            .collect()
    }
}
#[derive(Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct UserData {
    is_played: Option<bool>,
}

async fn jellyfin_login(config: &Jellyfin) -> Result<JellyfinApi> {
    let base = config.base_url.trim_end_matches('/').to_string();
    let client = HttpClient::builder()
        .user_agent("jellysync/1.1.0")
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

async fn fetch_poster_protocol(
    api: JellyfinApi,
    item: MediaItem,
    picker: Picker,
    cached_data: Option<Vec<u8>>,
) -> Result<Option<(Vec<u8>, StatefulProtocol)>> {
    tokio::time::sleep(Duration::from_millis(120)).await;
    let data = if let Some(data) = cached_data {
        data
    } else {
        let Some(data) = jellyfin_poster(&api, &item).await? else {
            return Ok(None);
        };
        data
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
    if browser.poster_id.as_deref() == Some(item.id.as_str()) {
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
    update_job(&job, "success", &format!("downloaded {count} item(s)"))?;
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
        let message = format!(
            "No Jellyfin media matched '{}' or its filters",
            job.jellyfin_name.as_deref().unwrap_or(&job.name)
        );
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
    update_job(&job.name, "success", &format!("downloaded {count} item(s)"))?;
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

async fn run_sync(config: Config, target: Vec<String>, parallelism: usize) -> Result<()> {
    run_sync_mode(config, target, parallelism, false).await
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
fn status() -> Result<()> {
    let conn = db()?;
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
    for row in rows {
        let (name, state, message, time) = row?;
        println!(
            "{name}: {state} ({time}){}",
            message.map(|m| format!(" — {m}")).unwrap_or_default()
        );
    }
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
    for row in rows {
        let (job, path, state, bytes, total) = row?;
        println!(
            "download {job}: {} — {state}, {bytes}/{} bytes",
            Path::new(&path)
                .file_name()
                .unwrap_or_default()
                .to_string_lossy(),
            total
                .map(|size| size.to_string())
                .unwrap_or_else(|| "?".into())
        );
    }
    if let Ok(output) = std::process::Command::new("systemctl")
        .args(["--user", "is-active", "jellysync.timer"])
        .output()
    {
        println!("timer: {}", String::from_utf8_lossy(&output.stdout).trim());
    }
    Ok(())
}
async fn prune(config: Config, target: Vec<String>, apply: bool) -> Result<()> {
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
                    println!("pruning {}", entry.path.display());
                    clear_tracked(&config, entry, ClearScope::File)?;
                } else {
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
    let reconcile_config = config.clone();
    let reconcile_task = tokio::spawn(async move { reconcile_existing(&reconcile_config).await });
    let mut out = stdout();
    enable_raw_mode()?;
    execute!(out, EnterAlternateScreen, EnableMouseCapture)?;
    let result = async move {
        let backend = CrosstermBackend::new(out);
        let mut terminal = Terminal::new(backend)?;
        let picker = Picker::from_query_stdio().unwrap_or_else(|_| Picker::from_fontsize((8, 16)));
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
        let mut confirm_clear = None;
        let mut explore: Option<ExploreState> = None;
        let mut explore_notice: Option<(String, bool)> = None;
        let mut explore_task: Option<tokio::task::JoinHandle<Result<usize>>> = None;
        let mut reconcile_task = Some(reconcile_task);
        let mut catalog_task: Option<CatalogTask> = None;
        let mut episode_task: Option<tokio::task::JoinHandle<(String, Result<Vec<MediaItem>>) >> =
            None;
        let mut poster_task: Option<PosterTask> = None;
        let mut sync_task: Option<tokio::task::JoinHandle<Result<()>>> = None;
        let mut sync_notice = None;
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
            if dashboard_task.is_none()
                && dashboard_refreshed_at.elapsed() >= Duration::from_millis(500)
            {
                dashboard_task = Some(tokio::task::spawn_blocking(dashboard_snapshot));
            }
            if reconcile_task
                .as_ref()
                .is_some_and(|task| task.is_finished())
            {
                let task = reconcile_task.take().expect("finished reconcile task exists");
                sync_notice = Some(match task.await {
                    Ok(Ok(0)) => ("Finished scanning existing media".into(), true),
                    Ok(Ok(count)) => (format!("Indexed {count} existing media files"), true),
                    Ok(Err(error)) => (format!("Could not reconcile existing files: {error:#}"), false),
                    Err(error) => (format!("Reconciliation stopped: {error}"), false),
                });
            }
            if episode_task.as_ref().is_some_and(|task| task.is_finished()) {
                let task = episode_task.take().expect("finished episode task exists");
                match task.await {
                    Ok((series_title, Ok(episodes))) => {
                        if let Some(browser) = explore.as_mut()
                            && browser.series_title.as_deref() == Some(series_title.as_str())
                        {
                            browser.episodes = episodes;
                            browser.episode_loading = false;
                        }
                    }
                    Ok((series_title, Err(error))) => {
                        if let Some(browser) = explore.as_mut()
                            && browser.series_title.as_deref() == Some(series_title.as_str())
                        {
                            browser.episode_loading = false;
                            explore_notice = Some((format!("Could not load episodes: {error:#}"), false));
                        }
                    }
                    Err(error) => {
                        explore_notice = Some((format!("Episode request stopped: {error}"), false));
                    }
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
                        explore_notice = Some((format!("Could not open Explore: {error:#}"), false));
                    }
                    Err(error) => {
                        explore = None;
                        explore_notice = Some((format!("Explore request stopped: {error}"), false));
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
                                }
                                Err(error) => {
                                    poster_protocol.empty_protocol();
                                    browser.poster_loaded = false;
                                    explore_notice = Some((format!("Poster unavailable: {error:#}"), false));
                                }
                            }
                        }
                    }
                    Err(error) if !error.is_cancelled() => {
                        explore_notice = Some((format!("Poster request stopped: {error}"), false));
                    }
                    Err(_) => {}
                }
            }
            if explore_task.as_ref().is_some_and(|task| task.is_finished()) {
                let task = explore_task.take().expect("finished Explore task exists");
                explore_notice = Some(match task.await {
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
            let jobs = &config.jobs;
            if selected >= jobs.len() && !jobs.is_empty() {
                selected = jobs.len() - 1;
            }
            let selected_job = jobs.get(selected);
            let downloads: Vec<_> = dashboard
                .downloads
                .iter()
                .filter(|entry| {
                    entry.job.starts_with("library:")
                        || selected_job.is_some_and(|job| entry.job == job.name)
                })
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
                    ListItem::new(vec![
                        Line::from(vec![
                            Span::styled("● ", state_style(state)),
                            Span::styled(
                                job.name.clone(),
                                Style::default()
                                    .fg(Color::White)
                                    .add_modifier(Modifier::BOLD),
                            ),
                        ]),
                        Line::from(vec![
                            Span::styled(format!("  {}", state.to_uppercase()), state_style(state)),
                            Span::styled(
                                if time.is_empty() {
                                    String::new()
                                } else {
                                    format!("  {time}")
                                },
                                Style::default().fg(Color::DarkGray),
                            ),
                        ]),
                        Line::from(Span::styled(
                            format!("  {}", truncate(message, 64)),
                            Style::default().fg(Color::Gray),
                        )),
                    ])
                })
                .collect();
            let download_rows: Vec<ListItem> = downloads
                .iter()
                .map(|entry| {
                    let filename = entry.path.file_name().unwrap_or_default().to_string_lossy();
                    let (bar, percent) = download_progress(entry.bytes, entry.total, 18);
                    let size = entry
                        .total
                        .map(|total| {
                            format!("{} / {}", format_bytes(entry.bytes), format_bytes(total))
                        })
                        .unwrap_or_else(|| format!("{} downloaded", format_bytes(entry.bytes)));
                    ListItem::new(vec![
                        Line::from(vec![
                            Span::styled("● ", state_style(&entry.status)),
                            Span::styled(
                                truncate(&filename, 48),
                                Style::default()
                                    .fg(Color::White)
                                    .add_modifier(Modifier::BOLD),
                            ),
                        ]),
                        Line::from(vec![
                            Span::styled(
                                format!("  {}  ·  {}", entry.job, entry.status),
                                state_style(&entry.status),
                            ),
                            Span::styled(format!("  {size}"), Style::default().fg(Color::Gray)),
                        ]),
                        Line::from(vec![
                            Span::styled("  ", Style::default()),
                            Span::styled(bar, Style::default().fg(Color::Cyan)),
                            Span::styled(
                                format!(" {percent}"),
                                Style::default().fg(Color::DarkGray),
                            ),
                        ]),
                    ])
                })
                .collect();
            let size = terminal.size()?;
            let area = Rect::new(0, 0, size.width, size.height);
            let explore_popup = centered_rect(90, 88, area);
            let [explore_list_area, explore_poster_area] = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(46), Constraint::Percentage(54)])
                .areas(Rect {
                    x: explore_popup.x + 1,
                    y: explore_popup.y + 2,
                    width: explore_popup.width.saturating_sub(2),
                    height: explore_popup.height.saturating_sub(4),
                });
            let [header, body, footer] = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3),
                    Constraint::Min(4),
                    Constraint::Length(2),
                ])
                .areas(area);
            let (jobs_area, downloads_area) = if area.width >= 100 {
                let [downloads_area, jobs_area] = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([Constraint::Percentage(64), Constraint::Percentage(36)])
                    .areas(body);
                (jobs_area, downloads_area)
            } else {
                let [jobs_area, downloads_area] = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Percentage(35), Constraint::Percentage(65)])
                    .areas(body);
                (jobs_area, downloads_area)
            };
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
                frame.render_widget(
                    Paragraph::new(header_text).block(
                        Block::default()
                            .borders(Borders::BOTTOM)
                            .border_style(Style::default().fg(Color::DarkGray)),
                    ),
                    header,
                );

                let jobs_title = format!("Jobs · {}", jobs.len());
                let downloads_title = format!(
                    "Tracked downloads · {} · {}",
                    selected_job.map_or("no job selected", |job| job.name.as_str()),
                    downloads.len()
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
                frame.render_stateful_widget(download_list, downloads_area, &mut downloads_state);

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
                            ("click", "select"),
                            ("Tab", "focus"),
                            ("s", "sync"),
                            ("S", "all"),
                            ("e", "explore"),
                            ("x/X/c", "clear"),
                            ("q", "quit"),
                        ]
                    } else {
                        vec![
                            ("click", "select"),
                            ("Tab", "focus"),
                            ("↑/↓", "select"),
                            ("s", "sync job"),
                            ("S", "sync all"),
                            ("e", "explore library"),
                            ("x", "clear file"),
                            ("X", "clear season"),
                            ("c", "clear show"),
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
                    } else if let Some(series_title) = &explore.series_title {
                        if explore.episode_loading {
                            frame.render_widget(
                                Paragraph::new("Loading episodes…")
                                    .style(Style::default().fg(Color::Gray))
                                    .alignment(ratatui::layout::Alignment::Center)
                                    .block(panel_block("Episodes", true)),
                                explore_list_area,
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
                                ListItem::new(format!("{mark}S{season:02}E{number:02}  {}", episode.name))
                            })
                            .collect();
                        episode_state.select((!rows.is_empty()).then_some(explore.episode_cursor));
                        frame.render_stateful_widget(
                            List::new(rows)
                                .block(panel_block(&format!("{series_title} · episodes"), true))
                                .highlight_style(Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD))
                                .highlight_symbol("▌ "),
                            explore_list_area,
                            &mut episode_state,
                        );
                        let details = vec![
                            Line::from(Span::styled(series_title.clone(), Style::default().fg(Color::White).add_modifier(Modifier::BOLD))),
                            Line::from(Span::styled(format!("{} episodes · {} selected", explore.episodes.len(), explore.selected_episodes.len()), Style::default().fg(Color::Gray))),
                            Line::from(Span::styled("Space select · a select all · d download", Style::default().fg(Color::Cyan))),
                        ];
                        frame.render_widget(Paragraph::new(details).block(panel_block("Series", false)), explore_poster_area);
                        }
                    } else {
                        let rows: Vec<_> = filtered
                            .iter()
                            .map(|item| {
                                let kind = item.item_type.as_deref().unwrap_or("Media");
                                let year = item.production_year.map(|year| format!(" · {year}")).unwrap_or_default();
                                ListItem::new(vec![
                                    Line::from(Span::styled(item.name.clone(), Style::default().fg(Color::White).add_modifier(Modifier::BOLD))),
                                    Line::from(Span::styled(format!("{kind}{year}"), Style::default().fg(Color::Gray))),
                                ])
                            })
                            .collect();
                        explore_state.select((!rows.is_empty()).then_some(explore.selected.min(rows.len().saturating_sub(1))));
                        frame.render_stateful_widget(
                            List::new(rows)
                                .block(panel_block(&format!("Library · {} · / search", filtered.len()), true))
                                .highlight_style(Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD))
                                .highlight_symbol("▌ "),
                            explore_list_area,
                            &mut explore_state,
                        );
                        if let Some(item) = filtered.get(explore.selected) {
                            let details_area = Rect { x: explore_poster_area.x + 1, y: explore_poster_area.y + 1, width: explore_poster_area.width.saturating_sub(2), height: 3 };
                            frame.render_widget(
                                Paragraph::new(vec![
                                    Line::from(Span::styled(item.name.clone(), Style::default().fg(Color::White).add_modifier(Modifier::BOLD))),
                                    Line::from(Span::styled(format!("{}{}", item.item_type.as_deref().unwrap_or("Media"), item.production_year.map(|year| format!(" · {year}")).unwrap_or_default()), Style::default().fg(Color::Gray))),
                                    Line::from(Span::styled("Enter open series · d download movie", Style::default().fg(Color::Cyan))),
                                ]),
                                details_area,
                            );
                            let poster_box = Rect { x: explore_poster_area.x + 2, y: explore_poster_area.y + 5, width: explore_poster_area.width.saturating_sub(4), height: explore_poster_area.height.saturating_sub(7) };
                            if explore.poster_loaded {
                                frame.render_stateful_widget(
                                    StatefulImage::default()
                                        .resize(Resize::Fit(Some(FilterType::Lanczos3))),
                                    poster_box,
                                    &mut poster_protocol,
                                );
                            } else {
                                let status = if explore.poster_loading {
                                    "Loading poster…"
                                } else {
                                    "Poster unavailable"
                                };
                                frame.render_widget(Paragraph::new(status).style(Style::default().fg(Color::DarkGray)), poster_box);
                            }
                        }
                    }
                    let footer = Rect { x: explore_popup.x + 1, y: explore_popup.y + explore_popup.height.saturating_sub(2), width: explore_popup.width.saturating_sub(2), height: 1 };
                    let text = if let Some((notice, _)) = &explore_notice {
                        format!("{}  ·  {}  ·  Esc close", explore.search, notice)
                    } else {
                        format!("{}  ·  ↑/↓ navigate  ·  Enter open  ·  d download  ·  Esc close", if explore.series_title.is_some() { "episode selection".to_string() } else { format!("Search: {}", explore.search) })
                    };
                    frame.render_widget(Paragraph::new(text).style(Style::default().fg(Color::Gray)), footer);
                }
            })?;
            if event::poll(Duration::from_millis(250))? {
                let input = event::read()?;
                if let Event::Mouse(mouse) = &input {
                    if let Some(browser) = explore.as_mut() {
                        match mouse.kind {
                            MouseEventKind::Down(MouseButton::Left) if browser.series_title.is_some() => {
                                if mouse.column >= explore_list_area.x
                                    && mouse.column < explore_list_area.x + explore_list_area.width
                                    && mouse.row > explore_list_area.y
                                    && mouse.row < explore_list_area.y + explore_list_area.height.saturating_sub(1)
                                {
                                    browser.episode_cursor = (usize::from(mouse.row - explore_list_area.y - 1)).min(browser.episodes.len().saturating_sub(1));
                                    if !browser.episodes.is_empty()
                                        && !browser
                                            .selected_episodes
                                            .remove(&browser.episode_cursor)
                                    {
                                        browser.selected_episodes.insert(browser.episode_cursor);
                                    }
                                }
                            }
                            MouseEventKind::Down(MouseButton::Left) => {
                                if mouse.column >= explore_list_area.x
                                    && mouse.column < explore_list_area.x + explore_list_area.width
                                    && mouse.row > explore_list_area.y
                                    && mouse.row < explore_list_area.y + explore_list_area.height.saturating_sub(1)
                                {
                                    let row = usize::from(mouse.row - explore_list_area.y - 1);
                                    browser.selected = (row / 2).min(browser.filtered_items().len().saturating_sub(1));
                                }
                            }
                            MouseEventKind::ScrollUp if browser.series_title.is_some() => {
                                browser.episode_cursor = browser.episode_cursor.saturating_sub(1);
                            }
                            MouseEventKind::ScrollDown if browser.series_title.is_some() => {
                                if browser.episode_cursor + 1 < browser.episodes.len() {
                                    browser.episode_cursor += 1;
                                }
                            }
                            MouseEventKind::ScrollUp => browser.selected = browser.selected.saturating_sub(1),
                            MouseEventKind::ScrollDown => {
                                let len = browser.filtered_items().len();
                                if browser.selected + 1 < len {
                                    browser.selected += 1;
                                }
                            }
                            _ => {}
                        }
                        if browser.series_title.is_none()
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
                    } else if confirm_clear.is_none() {
                        match mouse.kind {
                            MouseEventKind::Down(MouseButton::Left) => {
                                if let Some(index) = list_index_at(
                                    jobs_area,
                                    mouse.column,
                                    mouse.row,
                                    jobs_state.offset(),
                                    jobs.len(),
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
                                ) {
                                    selected_download = index;
                                    download_focus = true;
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
                if let Some(browser) = explore.as_mut() {
                    match key.code {
                        KeyCode::Esc => {
                            if browser.series_title.take().is_some() {
                                if let Some(task) = episode_task.take() {
                                    task.abort();
                                }
                                browser.selected_episodes.clear();
                                browser.episodes.clear();
                                browser.episode_cursor = 0;
                                browser.episode_loading = false;
                            } else {
                                if let Some(task) = catalog_task.take() {
                                    task.abort();
                                }
                                if let Some(task) = poster_task.take() {
                                    task.abort();
                                }
                                if let Some(task) = episode_task.take() {
                                    task.abort();
                                }
                                poster_protocol.empty_protocol();
                                explore = None;
                                explore_notice = None;
                            }
                        }
                        KeyCode::Up => {
                            if browser.series_title.is_some() {
                                browser.episode_cursor = browser.episode_cursor.saturating_sub(1);
                            } else {
                                browser.selected = browser.selected.saturating_sub(1);
                            }
                        }
                        KeyCode::Down => {
                            if browser.series_title.is_some() {
                                if browser.episode_cursor + 1 < browser.episodes.len() {
                                    browser.episode_cursor += 1;
                                }
                            } else {
                                let len = browser.filtered_items().len();
                                if browser.selected + 1 < len {
                                    browser.selected += 1;
                                }
                            }
                        }
                        KeyCode::Char('/') if browser.series_title.is_none() => {
                            browser.search.clear();
                            browser.selected = 0;
                        }
                        KeyCode::Backspace if browser.series_title.is_none() => {
                            browser.search.pop();
                            browser.selected = 0;
                        }
                        KeyCode::Char(' ') if browser.series_title.is_some() && !browser.episodes.is_empty() => {
                            if !browser.selected_episodes.remove(&browser.episode_cursor) {
                                browser.selected_episodes.insert(browser.episode_cursor);
                            }
                        }
                        KeyCode::Char('a') if browser.series_title.is_some() => {
                            browser.selected_episodes = (0..browser.episodes.len()).collect();
                        }
                        KeyCode::Enter if browser.series_title.is_none() => {
                            if let Some(item) = browser.filtered_items().get(browser.selected).map(|item| (*item).clone())
                                && item.item_type.as_deref() == Some("Series")
                            {
                                browser.episodes.clear();
                                browser.episode_cursor = 0;
                                browser.selected_episodes.clear();
                                browser.episode_loading = true;
                                browser.series_title = Some(item.name.clone());
                                let api = browser.api.clone();
                                episode_task = Some(tokio::spawn(async move {
                                    let title = item.name.clone();
                                    let result = match api {
                                        Some(api) => fetch_episodes(api, item).await,
                                        None => Err(anyhow::anyhow!("Explore Jellyfin session is not ready")),
                                    };
                                    (title, result)
                                }));
                                explore_notice = None;
                            }
                        }
                        KeyCode::Char('d') => {
                            if explore_task.is_some() {
                                explore_notice = Some(("A library download is already running".into(), false));
                            } else if let Some(title) = browser.series_title.clone() {
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
                        KeyCode::Char(character) if browser.series_title.is_none() && !character.is_control() => {
                            browser.search.push(character);
                            browser.selected = 0;
                        }
                        _ => {}
                    }
                    if let Some(browser) = explore.as_mut()
                        && browser.series_title.is_none()
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
                    match key.code {
                        KeyCode::Char('y') => {
                            if download_focus {
                                if let Some(entry) = downloads.get(selected_download) {
                                    let removed = clear_tracked(&config, entry, scope)?;
                                    if matches!(scope, ClearScope::Show) {
                                        update_job(
                                            &entry.job,
                                            "cleared",
                                            &format!("cleared {removed} tracked files"),
                                        )?;
                                    }
                                }
                            } else if let Some(job) = jobs.get(selected)
                                && let Some(entry) =
                                    downloads.iter().find(|entry| entry.job == job.name)
                            {
                                let removed = clear_tracked(&config, entry, ClearScope::Show)?;
                                update_job(
                                    &job.name,
                                    "cleared",
                                    &format!("cleared {removed} tracked files"),
                                )?;
                            }
                            confirm_clear = None;
                        }
                        KeyCode::Char('n') | KeyCode::Esc => confirm_clear = None,
                        _ => {}
                    }
                } else {
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => break,
                        KeyCode::Char('e') => {
                            if explore_task.is_some() {
                                explore_notice = Some(("Wait for the current library download to finish".into(), false));
                            } else {
                                explore = Some(ExploreState {
                                    api: None,
                                    items: Vec::new(),
                                    episodes: Vec::new(),
                                    selected: 0,
                                    episode_cursor: 0,
                                    selected_episodes: HashSet::new(),
                                    search: String::new(),
                                    series_title: None,
                                    poster_id: None,
                                    poster_loading: false,
                                    poster_loaded: false,
                                    catalog_loading: true,
                                    episode_loading: false,
                                });
                                let config = config.clone();
                                catalog_task = Some(tokio::spawn(async move {
                                    jellyfin_catalog(&config).await
                                }));
                                explore_notice = None;
                            }
                        }
                        KeyCode::Tab => download_focus = !download_focus,
                        KeyCode::Up if download_focus => {
                            selected_download = selected_download.saturating_sub(1)
                        }
                        KeyCode::Down
                            if download_focus && selected_download + 1 < downloads.len() =>
                        {
                            selected_download += 1
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
                        KeyCode::Char('s') if !jobs.is_empty() => {
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
        "failed" | "error" => Color::Red,
        "interrupted" | "cleared" => Color::Magenta,
        "not run" | "stopped" => Color::DarkGray,
        _ => Color::Cyan,
    };
    Style::default().fg(color)
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
            format!(" {key} "),
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" {label} "), Style::default().fg(Color::Gray)),
    ]
}

fn key_sep() -> Span<'static> {
    Span::styled("│", Style::default().fg(Color::DarkGray))
}

fn download_progress(bytes: u64, total: Option<u64>, width: usize) -> (String, String) {
    let Some(total) = total.filter(|total| *total > 0) else {
        return ("·".repeat(width), "  ?".into());
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

fn list_index_at(area: Rect, column: u16, row: u16, offset: usize, len: usize) -> Option<usize> {
    if column < area.x
        || column >= area.x + area.width
        || row < area.y + 1
        || row >= area.y + area.height.saturating_sub(1)
    {
        return None;
    }
    let index = offset + usize::from(row - area.y - 1) / 3;
    (index < len).then_some(index)
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Some(Commands::Status) => status(),
        Some(Commands::Start) => systemctl("start"),
        Some(Commands::Stop) => systemctl("stop"),
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
                Some(Commands::Sync { target }) => run_sync(config, target, parallelism).await,
                Some(Commands::Prune { apply, target }) => prune(config, target, apply).await,
                Some(Commands::Config) => {
                    println!("{}", serde_yaml::to_string(&config)?);
                    Ok(())
                }
                None => run_sync(config, cli.targets, parallelism).await,
                Some(Commands::Status | Commands::Start | Commands::Stop | Commands::Tui) => {
                    unreachable!()
                }
            }
        }
    }
}
fn systemctl(action: &str) -> Result<()> {
    let status = std::process::Command::new("systemctl")
        .args(["--user", action, "jellysync.service"])
        .status()?;
    if !status.success() {
        bail!("systemctl {action} jellysync.service failed with {status}");
    }
    Ok(())
}
