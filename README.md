# Murmduck

On-demand, streaming text-to-speech for Linux. Select text in an application,
press a global KDE shortcut, and Murmduck reads the PRIMARY selection aloud
using an OpenAI-compatible TTS server. Press the shortcut again to stop.

Murmduck is a single CLI executable. There is no daemon, no systemd service and
no GUI. Each invocation either becomes the one session owner or sends a control
command to it; it exits as soon as playback finishes or is cancelled.

Target platform for the MVP: **Fedora, KDE Plasma, Wayland, x86_64**.

## How it works

1. KDE runs `murmduck toggle` when the global shortcut is pressed.
2. With no active session, Murmduck reads the PRIMARY selection via
   `wl-paste --primary --no-newline`, validates it, and streams it to
   `POST {base_url}/audio/speech`.
3. The server's audio bytes (MP3 or WAV, per `tts.response_format`) are fed
   incrementally into a bounded GStreamer `appsrc` pipeline; playback starts
   before the whole HTTP response has arrived.
4. A second `toggle` (or `stop`) reaches the owner over a temporary Unix
   socket and cancels the request, the pipeline and the selection subprocess
   in a fixed order, then releases the lock and exits.

Audio backend decision: [`docs/adr/0001-gstreamer-streaming-audio-backend.md`](docs/adr/0001-gstreamer-streaming-audio-backend.md).

## Requirements

- Fedora KDE Plasma Wayland (x86_64).
- A Rust toolchain (Rust and Cargo) to install from source.
- `wl-clipboard` (`wl-paste`) for the PRIMARY selection.
- GStreamer with an audio sink plugin, plus the decoder for your configured
  format: `mpg123audiodec` for MP3 (from the "ugly" plugin package on Fedora) or
  `wavparse` for WAV (from `gstreamer1-plugins-good`).
- A reachable OpenAI-compatible streaming TTS server that returns `audio/mpeg`
  (MP3) or `audio/wav` (WAV), matching `tts.response_format`. Authentication is
  optional.

## Install

Install from a checkout of this repository. Full details, including the
Rust toolchain and Fedora build dependencies, are in
[`docs/installation.md`](docs/installation.md).

### Native build dependencies

On Fedora, install only the native libraries GStreamer's Rust bindings need at
build time:

```bash
sudo dnf install gstreamer1-devel gstreamer1-plugins-base-devel pkgconf-pkg-config
```

### Rust toolchain

Install Rust and Cargo as a separate step, for example with
[rustup](https://rustup.rs/):

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"   # or start a new shell
```

### From source

```bash
cargo install --path . --locked
```

Cargo installs the binary to `$CARGO_HOME/bin` (normally `~/.cargo/bin/murmduck`).
Add that directory to the graphical session's `PATH`, or use the absolute path
in the KDE shortcut (see below).

## Configure

Create `~/.config/murmduck/config.toml` (or
`$XDG_CONFIG_HOME/murmduck/config.toml`). A missing file uses documented
defaults, but a TTS server must be reachable:

```toml
[tts]
base_url = "http://localhost:8880/v1"
api_key_env = "MURMDUCK_API_KEY"   # omit this key for servers without auth
model = "tts-1"
voice = "alloy"
speed = 1.0
response_format = "mp3"   # "mp3" (default) or "wav"

[audio]
volume = 1.0

[selection]
provider = "wayland-primary"
max_chars = 20000
timeout_ms = 2000

[logging]
level = "info"
```

`response_format` selects what the server produces. Streaming WAV is supported
for OpenAI-compatible servers that only emit WAV (for example VieNeu-TTS),
including responses whose RIFF/data lengths are placeholders; playback still
starts before synthesis finishes.

If your server needs an API key, put the **value** in an environment variable
named by `api_key_env`, and make sure that variable is present in the
graphical session (not only in an interactive shell). See
[`docs/installation.md#api-credentials-in-the-graphical-session`](docs/installation.md#api-credentials-in-the-graphical-session).

Selected text and API keys are never written to logs; only sizes and a
`<redacted>` marker are logged.

## KDE global shortcut

1. Open **Settings → Keyboard → Shortcuts**.
2. Click **Add New → Command or Script**.
3. Enter the command and a name, then click **Add**. Use `murmduck toggle` if
   Cargo's bin directory is on the graphical session's `PATH`, or an absolute
   path such as `/home/your-user/.cargo/bin/murmduck toggle`.
4. Select the new command and assign a shortcut, for example `Alt+A`. Then
   select text in a supported application and press it to start reading; press
   it again to stop.

An absolute path is recommended because KDE does not necessarily launch commands
with the `PATH` of an interactive shell. Full details, including how to run
without a terminal and how focus is preserved, are in
[`docs/installation.md#kde-plasma-custom-shortcut`](docs/installation.md#kde-plasma-custom-shortcut).

## Commands

| Command | Behavior |
| --- | --- |
| `murmduck toggle` | Start reading the selection, or stop the active session. |
| `murmduck read-selection` | Read the selection; replace an active reading. |
| `murmduck stop` | Stop the active session (no-op when idle). |
| `murmduck status` / `status --json` | Report session state. |
| `murmduck test-voice` | Play a short sample with the configured voice. |
| `murmduck --version` / `--help` | Version and help. |

## Verification

`docs/acceptance-report.md` records what is verified automatically here and
which criteria require the Fedora KDE Wayland target.

## License

Dual-licensed under MIT OR Apache-2.0. See [`LICENSE`](LICENSE).
