# Installation and configuration

This guide is for the supported target: **Fedora, KDE Plasma, Wayland, x86_64**.
Murmduck is a normal CLI program; it installs no service, no tray icon and no
daemon.

## 1. Runtime prerequisites

```bash
sudo dnf install wl-clipboard \
    gstreamer1 gstreamer1-plugins-base gstreamer1-plugins-good \
    gstreamer1-plugins-ugly-free
```

Why each is needed:

| Package | Purpose |
| --- | --- |
| `wl-clipboard` | Supplies `wl-paste`, used as `wl-paste --primary --no-newline`. |
| `gstreamer1`, `gstreamer1-plugins-base` | Core, `mpegaudioparse`, `audioconvert`, `audioresample`. |
| `gstreamer1-plugins-good` | Audio output plugins (e.g. `pulsesink`) for the Wayland/PipeWire session. |
| `gstreamer1-plugins-ugly-free` | Provides the MP3 decoder (`mpg123audiodec`) on Fedora. |

If MP3 playback fails with a missing-element error, check which enabled
repository provides `mpg123audiodec` on your Fedora release and install that
plugin package; the pipeline requires MPEG-1 Layer III decoding.

You also need an OpenAI-compatible TTS server that accepts
`POST {base_url}/audio/speech` and returns `audio/mpeg`. The MVP assumes MPEG-1
MP3 (see ADR-0001).

## 2. Build prerequisites

### Native build dependencies

`gstreamer-rs` calls `pkg-config` at build time, so install the native -devel
packages first. Rust and Cargo are deliberately **not** part of this command —
they are a separate prerequisite below.

```bash
sudo dnf install gstreamer1-devel gstreamer1-plugins-base-devel pkgconf-pkg-config
```

### Rust toolchain

Install Rust and Cargo separately, for example with
[rustup](https://rustup.rs/):

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"   # or start a new shell
```

Verify with `cargo --version`. If you prefer a distro package, install `rust`
and `cargo` as a separate step rather than bundling them with the native build
dependencies above.

## 3. Install from source

From a checkout of this repository:

```bash
cargo install --path . --locked
```

Cargo installs the binary to `$CARGO_HOME/bin` (normally `~/.cargo/bin/murmduck`).
There is no manual copy step. Verify with:

```bash
~/.cargo/bin/murmduck --version
```

Add `~/.cargo/bin` (or `$CARGO_HOME/bin`) to the graphical session's `PATH` if
you want to invoke `murmduck` without an absolute path; see the next section.
Using the absolute path in the KDE shortcut avoids `PATH` issues entirely.

## 4. Configure

Configuration is read fresh on every invocation from:

1. `$XDG_CONFIG_HOME/murmduck/config.toml`, if `XDG_CONFIG_HOME` is set; else
2. `~/.config/murmduck/config.toml`.

A missing file is not an error: documented defaults are used. Omitted keys also
fall back to defaults. Invalid values fail before any network, selection or
audio work begins, with an error naming the offending key.

```toml
[tts]
base_url = "http://localhost:8880/v1"
api_key_env = "MURMDUCK_API_KEY"   # remove for an unauthenticated server
model = "tts-1"
voice = "alloy"
speed = 1.0
response_format = "mp3"

[audio]
volume = 1.0

[selection]
provider = "wayland-primary"
max_chars = 20000
timeout_ms = 2000

[logging]
level = "info"
```

Key points:

- **Defaulting policy:** the file, any section, and any individual key may be
  omitted; omitted values use the defaults shown above. A value that is present
  but malformed is rejected before any session work with an error naming the
  full key (for example `tts.speed`), and a present value that is semantically
  invalid (empty `tts.model`, out-of-range `audio.volume`, …) is rejected the
  same way.
- `api_key_env` is the **name** of an environment variable, never the secret.
  Omit it for servers without authentication.
- `selection.max_chars` is a limit in Unicode **characters**, not bytes.
- `selection.timeout_ms` bounds how long `wl-paste` may run.
- `logging.level` is one of `off`, `error`, `warn`, `info`, `debug`, `trace`.
- Only `response_format = "mp3"` is supported in the MVP.

Check the effective configuration non-interactively:

```bash
murmduck status --json
```

### API credentials in the graphical session

KDE launches the executable directly, so a key exported only in an interactive
shell is not visible. Put the value in a user environment file instead:

```bash
mkdir -p ~/.config/environment.d
printf 'MURMDUCK_API_KEY=%s\n' 'your-key-here' > ~/.config/environment.d/murmduck.conf
chmod 600 ~/.config/environment.d/murmduck.conf
```

Log out and back in once so the user session picks the variable up. This is
standard user-environment configuration; Murmduck itself does not require a
systemd service or run in the background. The value is never logged — startup
logging emits only `<redacted>` or `none`.

The same mechanism can be used to add Cargo's install directory to `PATH`:

```bash
# ~/.config/environment.d/paths.conf
PATH=$HOME/.cargo/bin:$PATH
```

If `CARGO_HOME` is set to something other than `~/.cargo`, use
`$CARGO_HOME/bin` instead.

## 5. KDE Plasma Custom Shortcut

1. Open **System Settings → Shortcuts → Custom Shortcuts**.
2. Choose **Edit → New → Global Shortcut → Command or URL**.
3. Give it a name such as *Murmduck: read / stop selection*.
4. Under **Trigger**, set your shortcut (for example `Ctrl+Alt+R`). Pick a
   combination that does not clash with the applications you use.
5. Under **Action → Command URL**, enter:

   ```
   /home/your-user/.cargo/bin/murmduck toggle
   ```

   Use the absolute path of the Cargo-installed binary (`$CARGO_HOME/bin/murmduck`,
   normally `~/.cargo/bin/murmduck`), or a directory on the graphical session's
   `PATH`.

6. Click **Apply / OK**.

Usage:

- Select text in a supported application, press the shortcut: reading starts.
- Press the same shortcut again: the active reading stops.
- `murmduck read-selection` from a second invocation replaces the current
  reading; `murmduck stop` stops it.

### Playing without a terminal

KDE executes the command directly with no controlling TTY, so no terminal
window is opened and no `Terminal=true` desktop entry is involved. Murmduck is
a short-lived console process: while it runs, it holds the single-instance
lock and a control socket, then exits when playback ends. It never forks a
background daemon. Its structured logs go to stderr, which KDE discards for a
shortcut-launched command.

To troubleshoot a shortcut, temporarily point a second shortcut at a command
that captures stderr, or run `murmduck toggle` from a terminal (with
`logging.level = "debug"` in the config) while a selection is active.

### Preserving focus

Murmduck deliberately does not activate or raise any window:

- it reads the PRIMARY selection with `wl-paste --primary --no-newline`, which
  does not change the regular clipboard and does not steal focus;
- it has no GUI and no desktop notification on the normal path;
- the KDE shortcut runs a command, not a window.

Therefore the application that owned the selection keeps keyboard focus
before, during and after playback. A second shortcut press is delivered by KDE
to the same command regardless of which application is focused.

## 6. Troubleshooting

| Symptom | Check |
| --- | --- |
| "PRIMARY selection is empty" | Make sure text is actually selected (highlighted), not just copied. `wl-paste --primary --no-newline` should print it. |
| "could not start selection provider" | `wl-clipboard` is missing or not on the graphical session's `PATH`. |
| "TTS server returned HTTP …" | Check `base_url`, model/voice, and server logs. |
| Connection refused | TTS server is down or `base_url` is wrong. The session exits and releases its resources. |
| No sound | Check the GStreamer MP3 decoder and audio sink plugins, and that PipeWire/PulseAudio has a default sink. Use `murmduck test-voice` to isolate playback. |
| API key not found | The variable must exist in the graphical session (see above), not only in your shell. |
| Shortcut does nothing | Use the absolute executable path in the KDE action; confirm `murmduck --version` works as the same user. |

## 7. Privacy

Murmduck retrieves the selected text only after the explicit shortcut action,
transmits it only to the configured TTS endpoint, and never writes it to logs.
Startup logging records the version, configured level and a `<redacted>` /
`none` API-key marker. Selection logging records only character and byte
counts. Error messages carry counts and status codes, never text or keys.
See `docs/acceptance-report.md` for the automated privacy verification.
