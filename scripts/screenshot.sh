#!/usr/bin/env bash
# Take the README screenshot (docs/screenshot.png) against a throwaway Jellyfin
# server that only holds Creative Commons media, so no real library can leak
# into it. Run from the repository root on a host with Docker and Nix; `just
# screenshot` does that on a remote build host and fetches the image back.
#
# Media: the official Big Buck Bunny and Sintel trailers (Blender Foundation,
# CC BY) with Wikimedia Commons posters, plus tiny generated episode files named
# after Pioneer One (CC BY-NC-SA), whose metadata and artwork Jellyfin looks up
# itself. The TUI then runs in a real kitty (so posters use the kitty graphics
# protocol) under a virtual X server with software OpenGL.
#
# Environment:
#   FONT_FAMILY    kitty font (default: JetBrainsMono Nerd Font Mono; the TUI
#                  draws its icons with Nerd Font glyphs)
#   FONT_DIR       extra font directory to make FONT_FAMILY available
#   JELLYFIN_PORT  loopback port for the throwaway server (default: any free one)
#   OUTPUT         image path (default: docs/screenshot.png)

JELLYFIN_IMAGE="docker.io/jellyfin/jellyfin:12.0.20260908-012347"
JELLYFIN_USER="admin"
JELLYFIN_PASSWORD="screenshot"
# Window size in pixels; the capture is cropped to exactly this.
WINDOW_WIDTH=1560
WINDOW_HEIGHT=920

WORK_DIR="$PWD/.screenshot"
CONTAINER="jellysync-screenshot-$$"
FONT_FAMILY="${FONT_FAMILY:-JetBrainsMono Nerd Font Mono}"
FONT_DIR="${FONT_DIR:-}"
JELLYFIN_PORT="${JELLYFIN_PORT:-}"
OUTPUT="${OUTPUT:-docs/screenshot.png}"
BASE_URL=
CLIENT_AUTH='MediaBrowser Client="jellysync-screenshot", Device="screenshot", DeviceId="jellysync-screenshot", Version="1"'
TOKEN=

log() {
  echo "==> $*" >&2
}

log_error() {
  echo "ERROR: $*" >&2
}

cleanup() {
  docker rm -f "$CONTAINER" >/dev/null 2>&1
}

# Path of a nixpkgs package's output, building it if needed.
nix_path() {
  nix build --no-link --print-out-paths "nixpkgs#$1" 2>/dev/null | head -1
}

fetch() {
  local url="$1"
  local target="$2"

  if [[ -s "$target" ]]
  then
    return 0
  fi
  if ! curl -fsSL --max-time 180 -o "$target" "$url"
  then
    log_error "download failed: $url"
    return 1
  fi
}

fetch_movies() {
  local dir="$WORK_DIR/media/movies"

  mkdir -p "$dir/Big Buck Bunny (2008)" "$dir/Sintel (2010)"
  fetch "https://download.blender.org/peach/trailer/trailer_iphone.m4v" \
    "$dir/Big Buck Bunny (2008)/Big Buck Bunny (2008).m4v" || return 1
  fetch "https://upload.wikimedia.org/wikipedia/commons/c/c5/Big_buck_bunny_poster_big.jpg" \
    "$dir/Big Buck Bunny (2008)/poster.jpg" || return 1
  fetch "https://download.blender.org/durian/trailer/sintel_trailer-480p.mp4" \
    "$dir/Sintel (2010)/Sintel (2010).mp4" || return 1
  fetch "https://upload.wikimedia.org/wikipedia/commons/8/8f/Sintel_poster.jpg" \
    "$dir/Sintel (2010)/poster.jpg" || return 1
}

# Two-second HEVC clips with two 5.1 audio tracks, so the Files list shows
# realistic stream badges without downloading a series.
generate_episodes() {
  local dir="$WORK_DIR/media/shows/Pioneer One/Season 01"
  local episode
  local file

  mkdir -p "$dir"
  for episode in 1 2 3 4 5 6
  do
    file="$dir/$(printf 'Pioneer One - S01E%02d.mkv' "$episode")"
    if [[ -s "$file" ]]
    then
      continue
    fi
    if ! nix shell nixpkgs#ffmpeg -c ffmpeg -loglevel error -y \
      -f lavfi -i testsrc2=size=1920x1080:rate=24 \
      -f lavfi -i "sine=frequency=$((300 + episode * 50))" \
      -t 2 -map 0 -map 1 -map 1 \
      -c:v libx265 -pix_fmt yuv420p10le -c:a ac3 -ac 6 \
      -metadata:s:a:0 language=eng -metadata:s:a:1 language=spa \
      "$file"
    then
      log_error "could not generate $file"
      return 1
    fi
  done
}

# Wait until an HTTP endpoint answers 200. /health answers before the server
# accepts API calls, and the server restarts once after the setup wizard.
wait_for() {
  local url="$1"
  local attempt

  for attempt in $(seq 1 90)
  do
    if [[ "$(curl -s -o /dev/null -w '%{http_code}' "$url")" == "200" ]]
    then
      return 0
    fi
    sleep 2
  done
  log_error "timed out waiting for $url (after $attempt attempts)"
  return 1
}

start_jellyfin() {
  local address

  # Without JELLYFIN_PORT, Docker picks a free loopback port.
  if ! docker run --detach --name "$CONTAINER" \
    --publish "127.0.0.1:${JELLYFIN_PORT}:8096" \
    --volume "$WORK_DIR/media:/media:ro" \
    "$JELLYFIN_IMAGE" >/dev/null
  then
    log_error "could not start the Jellyfin container"
    return 1
  fi
  if ! address=$(docker port "$CONTAINER" 8096/tcp | head -1) || [[ -z "$address" ]]
  then
    log_error "could not find the Jellyfin container's port"
    return 1
  fi
  BASE_URL="http://$address"
  log "throwaway Jellyfin at $BASE_URL"
  wait_for "$BASE_URL/System/Info/Public"
}

api() {
  local method="$1"
  local path="$2"
  local body="${3:-}"
  local auth="$CLIENT_AUTH"

  if [[ -n "$TOKEN" ]]
  then
    auth="MediaBrowser Token=\"$TOKEN\""
  fi
  if [[ -n "$body" ]]
  then
    curl -fsS -X "$method" -H "Authorization: $auth" \
      -H "Content-Type: application/json" -d "$body" "$BASE_URL$path"
  else
    curl -fsS -X "$method" -H "Authorization: $auth" "$BASE_URL$path"
  fi
}

setup_jellyfin() {
  local response

  api POST /Startup/Configuration \
    '{"UICulture":"en-US","MetadataCountryCode":"US","PreferredMetadataLanguage":"en"}' || return 1
  api GET /Startup/User >/dev/null || return 1
  api POST /Startup/User \
    "{\"Name\":\"$JELLYFIN_USER\",\"Password\":\"$JELLYFIN_PASSWORD\"}" || return 1
  api POST /Startup/RemoteAccess \
    '{"EnableRemoteAccess":false,"EnableAutomaticPortMapping":false}' || return 1
  api POST /Startup/Complete || return 1
  wait_for "$BASE_URL/System/Info/Public" || return 1
  if ! response=$(api POST /Users/AuthenticateByName \
    "{\"Username\":\"$JELLYFIN_USER\",\"Pw\":\"$JELLYFIN_PASSWORD\"}")
  then
    log_error "could not log in to the throwaway Jellyfin"
    return 1
  fi
  TOKEN=$(jq -r .AccessToken <<< "$response")
}

# Add the libraries and wait until Jellyfin has scanned them and fetched
# metadata and posters for every title.
add_libraries() {
  local attempt
  local titles

  api POST "/Library/VirtualFolders?name=Movies&collectionType=movies&refreshLibrary=true" \
    '{"LibraryOptions":{"PathInfos":[{"Path":"/media/movies"}]}}' || return 1
  api POST "/Library/VirtualFolders?name=Shows&collectionType=tvshows&refreshLibrary=true" \
    '{"LibraryOptions":{"PathInfos":[{"Path":"/media/shows"}]}}' || return 1
  for attempt in $(seq 1 60)
  do
    titles=$(api GET "/Items?Recursive=true&IncludeItemTypes=Movie,Series&Fields=Overview" \
      | jq '[.Items[] | select(.Overview != null and .ImageTags.Primary != null)] | length')
    if [[ "$titles" == "3" ]]
    then
      return 0
    fi
    sleep 3
  done
  log_error "Jellyfin did not finish fetching metadata (after $attempt attempts)"
  return 1
}

build_jellysync() {
  if ! nix develop path:. -c cargo build --release --locked --quiet
  then
    log_error "cargo build failed"
    return 1
  fi
}

write_config() {
  mkdir -p "$WORK_DIR/Videos" "$WORK_DIR/state"
  printf '%s' "$JELLYFIN_PASSWORD" > "$WORK_DIR/password"
  cat > "$WORK_DIR/config.yaml" <<EOF
download:
  mode: jellyfin
remote: {hostname: jellyfin.example.com, username: me, root: /srv/media}
local:
  root: $WORK_DIR/Videos
  directories: {tv_shows: TV Shows, movies: Movies}
jellyfin:
  base_url: $BASE_URL
  username: $JELLYFIN_USER
  password_file: $WORK_DIR/password
jobs:
  - {name: Pioneer One, directory: tv_shows}
  - {name: Sintel, directory: movies}
  - {name: Big Buck Bunny, directory: movies}
EOF
}

run_jellysync() {
  XDG_STATE_HOME="$WORK_DIR/state" JELLYSYNC_CONFIG="$WORK_DIR/config.yaml" \
    "$PWD/target/release/jellysync" "$@"
}

write_kitty_config() {
  local themes
  local dejavu
  local font_dirs=""

  themes=$(nix_path kitty-themes)
  dejavu=$(nix_path dejavu_fonts)
  if [[ -n "$FONT_DIR" ]]
  then
    font_dirs="<dir>$FONT_DIR</dir>"
  else
    font_dirs="<dir>$(nix_path nerd-fonts.jetbrains-mono)/share/fonts</dir>"
  fi
  # DejaVu covers symbols the main font may lack (★ ◆ ✓ ▸).
  cat > "$WORK_DIR/fonts.conf" <<EOF
<?xml version="1.0"?>
<!DOCTYPE fontconfig SYSTEM "fonts.dtd">
<fontconfig>
  $font_dirs
  <dir>$dejavu/share/fonts</dir>
  <cachedir>$WORK_DIR/fontcache</cachedir>
</fontconfig>
EOF
  # Dark Nord with a slightly darker background.
  cat > "$WORK_DIR/kitty.conf" <<EOF
include $themes/share/kitty-themes/themes/Nord.conf
background #24282F
font_family $FONT_FAMILY
font_size 12
window_padding_width 14
remember_window_size no
initial_window_width $WINDOW_WIDTH
initial_window_height $WINDOW_HEIGHT
EOF
}

# Runs under xvfb-run: start kitty, let the TUI load posters and probe files,
# focus the Files list on the second episode and capture the window.
write_capture_script() {
  local ffprobe_dir="$1"

  cat > "$WORK_DIR/capture.sh" <<EOF
#!/usr/bin/env bash
export FONTCONFIG_FILE="$WORK_DIR/fonts.conf"
export XDG_STATE_HOME="$WORK_DIR/state" JELLYSYNC_CONFIG="$WORK_DIR/config.yaml"
export PATH="$ffprobe_dir:\$PATH"
# Every input event the TUI sees, for debugging lost key presses.
export JELLYSYNC_INPUT_LOG="$WORK_DIR/input.log"
# Software rendering is slow; starting jellysync only once kitty is idle keeps
# kitty's reply to the image-protocol query within the TUI's 1s timeout.
kitty --config "$WORK_DIR/kitty.conf" \\
  sh -c "sleep 8; exec '$PWD/target/release/jellysync'" > "$WORK_DIR/kitty.log" 2>&1 &
kitty_pid=\$!
# kitty's first start (font cache) is slow; key presses before the TUI runs
# would be lost. Wait for jellysync, then give it time to load posters and
# probe the files.
for attempt in \$(seq 1 120)
do
  if pgrep -f '$PWD/target/release/jellysync' >/dev/null
  then
    break
  fi
  sleep 1
done
sleep 20
# Keys sent while kitty is still busy rendering (slowly, in software) the
# previous change get lost, so leave time between them.
xdotool key Tab
sleep 2
xdotool key Down
# With posters on screen a software-rendered frame takes seconds; wait for
# kitty to catch up before capturing.
sleep 20
import -window root -crop ${WINDOW_WIDTH}x${WINDOW_HEIGHT}+0+0 +repage "$WORK_DIR/screenshot.png"
kill "\$kitty_pid"
EOF
  chmod +x "$WORK_DIR/capture.sh"
}

capture() {
  local mesa
  local ffprobe_dir

  mesa=$(nix_path mesa)
  # shellcheck disable=SC2016 # expands in the inner shell, on purpose
  if ! ffprobe_dir=$(nix shell nixpkgs#ffmpeg -c sh -c 'dirname "$(command -v ffprobe)"')
  then
    log_error "could not provide ffprobe"
    return 1
  fi
  write_kitty_config
  write_capture_script "$ffprobe_dir"
  rm -f "$WORK_DIR/screenshot.png"
  # llvmpipe's JIT can emit instructions a VM does not expose (SIGILL);
  # softpipe is slower but safe.
  if ! LD_LIBRARY_PATH="$mesa/lib" \
    LIBGL_DRIVERS_PATH="$mesa/lib/dri" \
    __GLX_VENDOR_LIBRARY_NAME=mesa \
    __EGL_VENDOR_LIBRARY_DIRS="$mesa/share/glvnd/egl_vendor.d" \
    LIBGL_ALWAYS_SOFTWARE=1 \
    GALLIUM_DRIVER=softpipe \
    nix shell nixpkgs#kitty nixpkgs#xvfb-run nixpkgs#imagemagick nixpkgs#xdotool -c \
    xvfb-run -a -s "-screen 0 $((WINDOW_WIDTH + 40))x$((WINDOW_HEIGHT + 40))x24" \
    "$WORK_DIR/capture.sh"
  then
    log_error "capture failed; see $WORK_DIR/kitty.log"
    return 1
  fi
  if [[ ! -s "$WORK_DIR/screenshot.png" ]]
  then
    log_error "no screenshot was captured; see $WORK_DIR/kitty.log"
    return 1
  fi
  mkdir -p "$(dirname "$OUTPUT")"
  if ! nix shell nixpkgs#oxipng -c oxipng -q -o 4 --strip safe \
    --out "$OUTPUT" "$WORK_DIR/screenshot.png"
  then
    log_error "could not optimize the screenshot"
    return 1
  fi
}

main() {
  local step

  if ! command -v docker >/dev/null || ! command -v nix >/dev/null
  then
    log_error "needs docker and nix"
    return 1
  fi
  trap cleanup EXIT
  mkdir -p "$WORK_DIR"
  for step in fetch_movies generate_episodes start_jellyfin setup_jellyfin \
    add_libraries build_jellysync write_config
  do
    log "$step"
    if ! "$step"
    then
      log_error "$step failed"
      return 1
    fi
  done
  log "downloading the media with jellysync"
  if ! run_jellysync download
  then
    log_error "jellysync download failed"
    return 1
  fi
  log "capturing the TUI"
  capture || return 1
  log "wrote $OUTPUT"
}

# Sourcing the script (e.g. for manual end-to-end tests) only defines functions.
if [[ "${BASH_SOURCE[0]}" == "$0" ]]
then
  main "$@"
fi
