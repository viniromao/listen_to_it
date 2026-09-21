# listen_to_it

[![Release](https://github.com/viniromao/listen_to_it/actions/workflows/release.yml/badge.svg)](https://github.com/viniromao/listen_to_it/actions/workflows/release.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/Rust-1.75%2B-orange?logo=rust&logoColor=white)](https://www.rust-lang.org)
[![Platform: Linux](https://img.shields.io/badge/platform-Linux-blue?logo=linux&logoColor=white)](#dependencies)

A terminal-based YouTube music player. Search for any song, browse results with album art previews, and play audio directly in your terminal — no browser required.

![screenshot](screenshot.png)

---

## Features

- **YouTube search** — search by title, artist, or any query and get 10 results instantly
- **Thumbnail preview** — album art displayed inline if your terminal supports it (Kitty, iTerm2, WezTerm)
- **Queue management** — add tracks to a queue and skip forward/backward
- **Your own playlists** — save tracks you pick out of YouTube into named playlists, kept locally and playable offline of any search
- **Progress bar** — clickable seek bar with current position and total duration
- **MPRIS2 integration** — media keys (play/pause/stop) work system-wide via D-Bus
- **Keyboard-driven** — fully operable without a mouse, with a built-in key reference on `?`

---

## Dependencies

### Required

| Dependency | Purpose | Install |
|---|---|---|
| [Rust](https://rustup.rs) ≥ 1.75 | Build toolchain | `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \| sh` |
| [mpv](https://mpv.io) | Audio playback backend | `pacman -S mpv` / `apt install mpv` |

> **yt-dlp** is managed automatically — on first run the app downloads the official standalone binary to `~/.cache/listen_to_it/`. No manual installation needed.

### Audio backend (one of)

| Option | Notes |
|---|---|
| **PipeWire** | Recommended on modern Linux distros |
| **PulseAudio** | Works via PipeWire's PulseAudio compatibility layer |

mpv will use whatever audio server is running — no extra configuration needed.

### Optional

| Dependency | Purpose |
|---|---|
| A terminal with image protocol support | Inline thumbnail display (Kitty, WezTerm, iTerm2) |
| A D-Bus session (standard on any desktop) | MPRIS2 media key support |

---

## Building and running

```bash
git clone https://github.com/viniromao/listen_to_it
cd listen_to_it

# Build (release recommended for performance)
cargo build --release

# Run
./target/release/listen_to_it
```

Or run directly with cargo:

```bash
cargo run --release
```

The first build will take a minute to compile all dependencies.

### Installing (macOS and Linux)

```bash
curl -LsSf https://github.com/viniromao/listen_to_it/releases/latest/download/listen_to_it-installer.sh | sh
```

That drops the binary in `$CARGO_HOME/bin` (`~/.cargo/bin`) and leaves an
install receipt in `~/.config/listen_to_it`.

### Updating itself

A copy installed that way keeps itself current. On startup — at most once a
day — it looks up the latest release and, if it is newer than the running
version, installs it in the background with the same installer. Nothing is
interrupted mid-song; the new version takes over the next time you start the
app, which it tells you once it is done.

Only installed copies do this: the check is skipped when there is no install
receipt, so a `cargo run`, a `cargo install`, or a build from a package
manager is never overwritten behind your back. To turn it off entirely:

```bash
LISTEN_TO_IT_NO_UPDATE=1 listen_to_it
```

Releases are cut by cargo-dist from a version tag (`git tag v0.2.0 && git push
--tags`), which builds macOS and Linux binaries and publishes them along with
the installer.

---

## Keybindings

### Navigation

| Key | Action |
|---|---|
| `/` or `s` | Open search bar |
| `Esc` | Close search bar |
| `j` / `↓` | Move selection down |
| `k` / `↑` | Move selection up |
| `Enter` | Play selected track (clears queue) |
| `f` | Add selected track to queue |
| `a` | Save selected track to one of your playlists |
| `A` | Save the current track + queue as a new playlist |
| `p` | Switch between search results and your playlists |
| `?` or `F1` | Show the full key reference |
| `q` | Quit |

### Help (`?`)

`?` (or `F1`) opens a scrollable overlay listing every key and what it does,
grouped by what you're trying to do. `j` / `k` scroll it; any other key closes
it. A `[?] help` badge sits in the top-right corner at all times, so the key
is there whether or not you went looking for it.

### Your playlists (`p`)

Left pane lists your playlists, right pane the tracks of the highlighted one;
`Tab` (or `←` / `→`) moves between them.

| Key | Action |
|---|---|
| `Enter` | Play the playlist — or, over a track, start from that track |
| `f` | Queue the playlist — or, over a track, queue just that track |
| `n` | Create a new, empty playlist |
| `R` | Rename the highlighted playlist |
| `x` | Delete the playlist, or remove the highlighted track (asks first) |
| `J` / `K` | Move the highlighted track down / up |
| `Esc` | Back to the search results |

Playlists live in `~/.local/share/listen_to_it/playlists.json` (or
`$XDG_DATA_HOME`) — your data, not a cache, so clearing
`~/.cache/listen_to_it` leaves them alone. Each track is stored with enough
metadata to play it again without searching for it first.

To turn a YouTube playlist into one of your own, queue it with `f` and then
press `A` to save the whole queue under a name of your choosing.

### Playback

| Key | Action |
|---|---|
| `Space` | Pause / resume |
| `h` / `←` | Seek back 5 seconds |
| `l` / `→` | Seek forward 5 seconds |
| `[` | Skip to previous track in history |
| `]` | Skip to next track in queue |
| `+` / `=` | Volume up 5% |
| `-` | Volume down 5% |
| `r` | Loop the current track on / off |
| `z` | Shuffle on / off |
| `d` | Toggle thumbnail visibility |

Shuffle reorders whatever is already queued and drops anything queued after
that at a random spot, so a playlist comes out in a different order every time.
Tracks still leave the queue as they play — each one comes up once, and the
queue panel always shows what is really coming next.

### Mouse

| Action | Effect |
|---|---|
| Click on the progress bar | Jump to that position in the song |

---

## How it works

1. **Search** — queries YouTube via `yt-dlp` and returns the top 20 results with thumbnails and metadata
2. **yt-dlp auto-setup** — on first run the app checks for `yt-dlp` in `$PATH`; if absent, downloads the official standalone binary to `~/.cache/listen_to_it/` automatically
3. **Stream resolution** — the app runs `yt-dlp` itself to turn a video into a direct audio URL, caches the result, and resolves ahead of time (the next queued track, and whichever search row you're reading) so pressing play usually costs nothing
4. **Playback** — `mpv` gets that already-resolved URL, communicating with the app through a Unix socket (`/tmp/listen_to_it_mpv.sock`) for pause, seek, and volume control
5. **Thumbnails** — downloaded asynchronously and rendered inline using `ratatui-image` if the terminal supports a graphics protocol
6. **Saved playlists** — written to `~/.local/share/listen_to_it/playlists.json` through a temp file and a rename, so an interrupted save can't leave you with a half-written library
7. **MPRIS2** — `souvlaki` publishes the current track metadata on D-Bus so media keys and widgets (e.g. waybar, playerctl) work normally

---

## Troubleshooting

**No audio / mpv fails to start**
- Make sure `mpv` is installed and accessible in `$PATH`: `which mpv`
- Test manually: `mpv --no-video "https://www.youtube.com/watch?v=dQw4w9WgXcQ"`

**Search returns no results**
- Delete the cached binary to force a fresh download: `rm ~/.cache/listen_to_it/yt-dlp`
- YouTube occasionally changes their API; the next launch will download the latest yt-dlp automatically

**No thumbnail in the preview panel**
- Thumbnails require a terminal that implements the Kitty graphics protocol or iTerm2 protocol
- Tested terminals: Kitty, WezTerm, iTerm2
- In unsupported terminals the preview panel will show text metadata only

**Media keys not working**
- Requires a D-Bus session bus (standard on any desktop environment)
- Check with: `playerctl status`

---

## License

MIT
