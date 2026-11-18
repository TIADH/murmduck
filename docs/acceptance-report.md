# Acceptance report — Murmduck MVP (ticket #9)

Date: 2026-11-18. Scope: issue #1 §13 (M3) and §14 acceptance criteria, and
issue #9's acceptance criteria.

## 1. Verification environment — read this first

Checks ran on a **headless Ubuntu 26.04.1 x86_64 VM**, which is *not* the
supported target (Fedora KDE Plasma Wayland):

- `WAYLAND_DISPLAY` and `DISPLAY` are unset; there is no Wayland compositor.
- `wl-paste`/`wl-clipboard` is not installed, so real PRIMARY selection cannot
  be read here.
- GStreamer 1.28.2 dev/runtime files come from a rootless sysroot under
  `/tmp/opencode` (not system packages); the only audio device is a virtual
  PulseAudio sink which, per ADR-0001, does not drain in real time, so audible
  playback cannot be confirmed here.
- No RPM/dnf tooling or fresh Fedora install is available.

Consequently the automated tests exercise the **public seams** with fakes: the
injectable `SelectionSource`, a local fake TTS HTTP server, `AudioSink::Fake`
and `SyncedFake`, injected `RuntimePaths`, and a captured `tracing` writer.
They verify logic, lifecycle, cleanup and privacy — not desktop behaviour,
audible output or packaging on the target.

Commands used (all green):

```bash
cargo test
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

`tests/acceptance.rs` contains 10 consolidated ticket-#9 tests; the full suite
adds the earlier milestone suites. New tests added by this ticket are marked
**[new]** below.

## 2. Verified automatically in this environment

| Acceptance criterion (spec §14) | Evidence | Status |
| --- | --- | --- |
| Unicode and Vietnamese text handled correctly | **[new]** `tests/acceptance.rs::vietnamese_and_unicode_selection_is_transmitted_verbatim_and_read_aloud` asserts the exact Vietnamese/CJK/emoji/em-dash string reaches the TTS request body, the char count matches `chars().count()`, and audio is decoded. Also `tests/selection.rs::the_provider_source_preserves_utf8_text`. | Verified |
| Text-size limit applies | **[new]** `tests/acceptance.rs::the_character_limit_applies_to_unicode_text_before_any_tts_request` (11-char Vietnamese string, limit 10 → `TooLong { max: 10, actual: 11 }`, no TTS request). Also `tests/selection.rs::the_char_limit_counts_unicode_characters_not_bytes`, `a_selection_over_the_char_limit_is_rejected`. | Verified |
| Rapid repeated invocations never overlap audio | **[new]** `tests/acceptance.rs::rapid_repeated_toggles_never_overlap_audio_or_leave_resources` (8 concurrent toggles, peak 1 player, no leftover resources). Also `tests/cancellation.rs::rapid_repeated_toggles_never_overlap_audio`, `tests/session_lifecycle.rs::concurrent_first_invocations_never_overlap`. | Verified |
| Slow or unavailable TTS server behaves correctly | **[new]** `tests/acceptance.rs::an_unavailable_tts_server_surfaces_an_error_without_an_orphan` (connection refused, fails fast), `a_slow_server_stays_controllable_and_stop_tears_everything_down` (stalled server, status reachable, prompt stop). Also `tests/read_aloud.rs::an_http_error_surfaces_and_cleans_up_the_session`, `an_unreachable_server_surfaces_and_cleans_up_the_session`, `the_control_socket_reports_buffering_while_the_server_is_slow`. | Verified |
| Cancellation during connection and buffering | **[new]** `tests/acceptance.rs::cancellation_while_connecting_is_prompt_and_leaves_no_resources` (HTTP request demonstrably aborted — server sees disconnect). Also `tests/cancellation.rs::cancelling_while_connecting_aborts_the_http_request_promptly`, `cancelling_while_buffering_aborts_the_http_request_promptly`, `toggle_while_connecting_cancels_and_releases_resources_in_order`, `toggle_while_buffering_cancels_streaming_and_stops_the_pipeline`. | Verified |
| No process after normal completion | **[new]** `tests/acceptance.rs::normal_completion_leaves_no_process_lock_or_socket` (owner exits on its own, ordered teardown ends with lock release, no socket). Binary-level failure-path check: `tests/cli.rs::toggle_exits_promptly_when_the_selection_provider_is_unavailable` (real process exits within a bound). | Verified at session/process level (see §3 for real-device caveat) |
| Locks and sockets safe after a crash | **[new]** `tests/acceptance.rs::a_crashed_owner_leaves_no_lock_and_a_fresh_owner_recovers` spawns a **real child process** that acquires the lock, binds the socket and exits abruptly with code 70 without unwinding. The kernel releases the flock, the stale socket file remains, and a fresh owner removes it and completes. Also `tests/session_lifecycle.rs::a_stale_socket_file_does_not_block_a_new_owner`, `a_client_takes_over_when_the_owner_exits_during_initialization`. | Verified |
| Credentials and selected text absent from logs | **[new]** `tests/acceptance.rs::privacy_no_credentials_or_selected_text_appear_in_logs` runs a full session at `DEBUG` with a process-wide subscriber, proves the Vietnamese text and Bearer key really reach the fake server, then asserts neither (nor fragments) appear in captured logs. Also `tests/logging.rs::startup_log_includes_safe_context_but_not_the_api_key`, `selection_summary_never_logs_the_selected_text`, `tests/selection.rs::reading_a_selection_never_logs_its_text`, `the_success_report_shows_only_a_character_count`, `error_messages_never_contain_selection_text`. | Verified |
| Streaming begins before the whole response arrives | `tests/read_aloud.rs::playback_starts_before_the_whole_response_has_downloaded`, `tests/streaming_audio.rs` (misaligned chunks, bounded buffering, underruns, teardown). | Verified with `Fake` sink |
| Only one session / toggle-to-stop / replacement / status | `tests/session_lifecycle.rs::toggle_becomes_owner_and_a_second_toggle_stops_it`, `only_one_holder_can_acquire_the_lock_at_a_time`; `tests/replacement.rs::read_selection_replaces_a_running_reading_without_overlapping_audio`; `tests/status.rs::status_reports_every_lifecycle_state_from_a_live_owner`; `tests/cancellation.rs::stop_while_playing_stops_audio_immediately_and_the_device_is_released`. | Verified |
| TTS configurable through TOML | `tests/config.rs` (defaults, partial/full config, validation errors, path resolution, API-key env resolution) and `tests/cli.rs::invalid_config_fails_with_an_actionable_error`. | Verified |
| No daemon/service/GUI dependency | Source inspection: no service units, no tray/GUI crates, no background fork; the binary is a single short-lived CLI. ADR-0001 documents the chosen audio backend. | Reviewed (static) |

## 3. Requires the Fedora KDE Wayland target — NOT verified here

These were **not** verified on this host. They are documented in
`docs/installation.md` and must be checked manually on a fresh Fedora KDE
Plasma Wayland installation:

| Criterion | What must be checked on target | Why it cannot be checked here |
| --- | --- | --- |
| KDE Custom Shortcut start/stop | Configure the Global Shortcut as documented; select text, press shortcut → reading starts; press again → stops. | No KDE Plasma, no global shortcut daemon, no Wayland. |
| Playback with no open terminal | Launch only via the KDE shortcut; confirm no terminal window appears and audio plays. | No desktop session; binary-level "process exits" is verified, but not KDE's launch environment. |
| Focus preservation | Select text in Chromium/Zed/OpenCode Desktop/Alacritty/Konsole/Discord, press shortcut, confirm the same app keeps keyboard focus. No window should be raised. | No Wayland compositor or applications. The implementation never activates a window or changes the regular clipboard, but that is a code-level argument, not a runtime proof. |
| Verified application matrix (§2) | Repeat the full workflow for all six applications listed in the spec. | No `wl-paste`, no Wayland, none of those apps. Their PRIMARY selection compatibility was already manually verified upstream, but the complete Murmduck workflow was not. |
| Audible playback | `murmduck test-voice` must produce intelligible audio through PipeWire; MP3 decoder and sink plugins must load. | Headless VM; virtual PulseAudio sink does not drain in real time (ADR-0001). All audio tests use `fakesink`. |
| Real RPM install on a fresh system | Build the tarball, build the RPM, `dnf install`, then run the documented setup using only the installed files. | No dnf/rpmbuild/fresh Fedora system. The spec file and helper are delivered but unexecuted here. |
| API key from the graphical session | Set `~/.config/environment.d/murmduck.conf`, re-login, and confirm authenticated TTS works from the shortcut. | No graphical user session to inherit the variable. |
| Executable discoverable by the graphical session | Confirm the absolute path in the KDE action works; or that a custom `PATH` from `environment.d` is inherited. | No KDE launch environment. |

## 4. Honest limitations of the automated evidence

- The crash test simulates an abrupt owner death with `std::process::exit(70)`
  in a child process, not a `SIGKILL`. Both close the file descriptor and let
  the kernel release the `flock`; both leave the socket file behind. It does
  not exercise kernel-level kill paths or core dumps.
- "No process after normal completion" is proven for the in-process owner
  thread and for the real binary's early-failure path, not for a real
  device-backed playback on Fedora (which could in principle hang in
  GStreamer; the headless hardware difference is why this remains target-only).
- Fake TTS servers model HTTP 200 MP3 streams, stalls and connection refusal.
  They do not model real TTS providers, TLS, HTTP/2, or chunk framing quirks
  beyond what the GStreamer spike already covered.
- The RPM spec has not been built with `rpmbuild` on Fedora here. The helper
  script requires network access to crates.io when generating the vendor tree;
  the resulting build is offline.

## 5. Target verification checklist (for a Fedora KDE machine)

1. `sudo dnf install ./murmduck-0.1.0-1.*.x86_64.rpm`; check `murmduck --version`.
2. Create `~/.config/murmduck/config.toml` from `docs/installation.md`; set up
   the API key in `~/.config/environment.d/` if the server needs one, then
   re-login.
3. `murmduck test-voice` — confirm intelligible audio (audible criterion).
4. Configure the KDE Custom Shortcut to `/usr/bin/murmduck toggle`.
5. For each app in spec §2: highlight a sentence (include Vietnamese/Unicode),
   press the shortcut, confirm audio and unchanged focus; press again to stop.
6. Hammer the shortcut rapidly; confirm only one stream is audible at a time.
7. With the TTS server stopped, press the shortcut; confirm a prompt error and
   that `murmduck status` reports `Inactive` with no leftover process/socket.
8. Select > `max_chars` characters; confirm no request is sent.
