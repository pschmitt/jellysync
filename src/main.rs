// jellysync - Sync media from SSH or Jellyfin
// Copyright (C) 2026 pschmitt
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use crossterm::{
    event::{self, Event, KeyCode},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures_util::StreamExt;
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    widgets::{Block, Borders, List, ListItem, Paragraph},
};
use reqwest::{Client as HttpClient, StatusCode};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::{BTreeMap, HashSet},
    env,
    io::stdout,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
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
    username: String,
    password_file: String,
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
    path: Option<String>,
    parent_index_number: Option<u32>,
    index_number: Option<u32>,
    user_data: Option<UserData>,
}
#[derive(Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct UserData {
    is_played: Option<bool>,
}

async fn jellyfin_login(config: &Jellyfin) -> Result<JellyfinApi> {
    let password = std::fs::read_to_string(&config.password_file)
        .with_context(|| format!("read Jellyfin password file {}", config.password_file))?;
    let base = config.base_url.trim_end_matches('/').to_string();
    let client = HttpClient::builder()
        .user_agent("jellysync/1.1.0")
        .build()?;
    let authorization = r#"MediaBrowser Client="jellysync", Device="jellysync", DeviceId="jellysync", Version="1.1.0""#;
    let response = client
        .post(format!("{base}/Users/AuthenticateByName"))
        .header("Authorization", authorization)
        .json(&json!({ "Username": config.username, "Pw": password.trim_end() }))
        .send()
        .await
        .context("authenticate to Jellyfin")?
        .error_for_status()
        .context("Jellyfin rejected authentication")?
        .json::<AuthResult>()
        .await
        .context("parse Jellyfin authentication response")?;
    Ok(JellyfinApi {
        client,
        base,
        token: response.access_token,
        user_id: response.user.id,
    })
}

async fn jellyfin_items(api: &JellyfinApi, query: &[(&str, &str)]) -> Result<Vec<MediaItem>> {
    Ok(api
        .client
        .get(format!("{}/Users/{}/Items", api.base, api.user_id))
        .header("X-Emby-Token", &api.token)
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
    let series = jellyfin_items(
        api,
        &[
            ("Recursive", "true"),
            ("IncludeItemTypes", "Series"),
            ("SearchTerm", job.name.as_str()),
            ("Fields", "Path"),
            ("Limit", "100"),
        ],
    )
    .await?;
    let items = if let Some(series) = find_one(series, &job.name)? {
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
                ("SearchTerm", job.name.as_str()),
                ("Fields", "Path"),
                ("Limit", "100"),
            ],
        )
        .await?;
        find_one(movies, &job.name)?.into_iter().collect()
    };
    if items.is_empty() {
        bail!(
            "Jellyfin returned no media for '{}'; refusing to prune its downloads",
            job.name
        );
    }
    Ok(items.into_iter().map(|item| item.id).collect())
}

async fn jellyfin_job(config: &Config, job: &Job, slots: Arc<Semaphore>) -> Result<()> {
    let credentials = config
        .jellyfin
        .as_ref()
        .context("Jellyfin mode requires jellyfin credentials")?;
    let api = jellyfin_login(credentials).await?;
    let found = jellyfin_items(
        &api,
        &[
            ("Recursive", "true"),
            ("IncludeItemTypes", "Series"),
            ("SearchTerm", job.name.as_str()),
            ("Fields", "Path"),
            ("Limit", "100"),
        ],
    )
    .await?;
    let series = find_one(found, &job.name)?;
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
                ("SearchTerm", job.name.as_str()),
                ("Fields", "Path,UserData"),
                ("Limit", "100"),
            ],
        )
        .await?;
        find_one(found, &job.name)?.into_iter().collect()
    };
    if items.is_empty() {
        bail!("no Jellyfin media matched '{}'", job.name);
    }
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
    if items.is_empty() {
        update_job(&job.name, "success", "no media matched filters")?;
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
            jellyfin_download(&api, &name, &item, &output).await
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
) -> Result<()> {
    match jellyfin_download_inner(api, job, item, destination).await {
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
        .header("X-Emby-Token", &api.token);
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
            .header("X-Emby-Token", &api.token)
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
    println!("downloaded {}", destination.display());
    Ok(())
}

async fn run_sync(config: Config, target: Vec<String>, parallelism: usize) -> Result<()> {
    let jobs = selected_jobs(&config, &target)?;
    let slots = Arc::new(Semaphore::new(parallelism));
    let mut tasks = JoinSet::new();
    for job in jobs {
        let config = config.clone();
        let slots = slots.clone();
        tasks.spawn(async move { sync_job(config, job, slots).await });
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
async fn sync_job(config: Config, job: Job, slots: Arc<Semaphore>) -> Result<()> {
    if config.download.mode == "jellyfin" {
        return jellyfin_job(&config, &job, slots).await;
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
    println!("synced {} -> {}", job.name, destination);
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
struct DownloadEntry {
    item_id: String,
    job: String,
    path: PathBuf,
    status: String,
    bytes: u64,
    total: Option<u64>,
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

fn tui(config: Config) -> Result<()> {
    let mut out = stdout();
    enable_raw_mode()?;
    execute!(out, EnterAlternateScreen)?;
    let result = (|| -> Result<()> {
        let backend = CrosstermBackend::new(out);
        let mut terminal = Terminal::new(backend)?;
        let mut selected = 0usize;
        let mut selected_download = 0usize;
        let mut download_focus = false;
        let mut confirm_clear = None;
        loop {
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
                history.insert(
                    name,
                    format!("{state:<10} {time} {}", message.unwrap_or_default()),
                );
            }
            let downloads = tracked_downloads()?;
            if selected_download >= downloads.len() && !downloads.is_empty() {
                selected_download = downloads.len() - 1;
            }
            let jobs = &config.jobs;
            if selected >= jobs.len() && !jobs.is_empty() {
                selected = jobs.len() - 1;
            }
            let entries: Vec<ListItem> = jobs
                .iter()
                .enumerate()
                .map(|(index, job)| {
                    let marker = if index == selected { ">" } else { " " };
                    let state = history
                        .get(&job.name)
                        .map(String::as_str)
                        .unwrap_or("not run");
                    ListItem::new(format!("{marker} {:<28} {state}", job.name))
                })
                .collect();
            let download_rows: Vec<ListItem> = downloads
                .iter()
                .enumerate()
                .map(|(index, entry)| {
                    let marker = if download_focus && index == selected_download {
                        ">"
                    } else {
                        " "
                    };
                    let progress = entry
                        .total
                        .map(|total| format!("{}/{total} bytes", entry.bytes))
                        .unwrap_or_else(|| format!("{} bytes", entry.bytes));
                    ListItem::new(format!(
                        "{marker} {:<20} {} — {}, {progress}",
                        entry.job,
                        entry.path.file_name().unwrap_or_default().to_string_lossy(),
                        entry.status
                    ))
                })
                .collect();
            terminal.draw(|frame| {
                let area = frame.area();
                let layout = ratatui::layout::Layout::default()
                    .direction(ratatui::layout::Direction::Vertical)
                    .constraints([
                        ratatui::layout::Constraint::Percentage(50),
                        ratatui::layout::Constraint::Percentage(50),
                        ratatui::layout::Constraint::Length(1),
                    ])
                    .split(area);
                frame.render_widget(
                    List::new(entries).block(
                        Block::default()
                            .title("Jellysync jobs")
                            .borders(Borders::ALL),
                    ),
                    layout[0],
                );
                frame.render_widget(
                    List::new(download_rows).block(
                        Block::default()
                            .title("Tracked downloads")
                            .borders(Borders::ALL),
                    ),
                    layout[1],
                );
                let help = if confirm_clear.is_some() {
                    "Clear selected download(s)? y: confirm | n: cancel"
                } else {
                    "Tab: focus jobs/downloads | ↑/↓: select | d: file | s: season | S: show | q: quit"
                };
                frame.render_widget(Paragraph::new(help), layout[2]);
            })?;
            if event::poll(Duration::from_secs(1))?
                && let Event::Key(key) = event::read()?
            {
                if let Some(scope) = confirm_clear {
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
                        KeyCode::Tab => download_focus = !download_focus,
                        KeyCode::Up if download_focus => {
                            selected_download = selected_download.saturating_sub(1)
                        }
                        KeyCode::Down
                            if download_focus && selected_download + 1 < downloads.len() =>
                        {
                            selected_download += 1
                        }
                        KeyCode::Up => selected = selected.saturating_sub(1),
                        KeyCode::Down if selected + 1 < jobs.len() => selected += 1,
                        KeyCode::Char('d') if download_focus && !downloads.is_empty() => {
                            confirm_clear = Some(ClearScope::File)
                        }
                        KeyCode::Char('s') if download_focus && !downloads.is_empty() => {
                            confirm_clear = Some(ClearScope::Season)
                        }
                        KeyCode::Char('S') if download_focus && !downloads.is_empty() => {
                            confirm_clear = Some(ClearScope::Show)
                        }
                        KeyCode::Char('d') if !jobs.is_empty() => {
                            download_focus = false;
                            confirm_clear = Some(ClearScope::Show);
                        }
                        _ => {}
                    }
                }
            }
        }
        Ok(())
    })();
    disable_raw_mode()?;
    execute!(stdout(), LeaveAlternateScreen)?;
    result
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
            tui(config)
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
