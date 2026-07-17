# InferenceHub Desktop

A lightweight native desktop client for [InferenceHub](https://inferencehub.tech) chat
(`chat.inferencehub.tech`). Sign in once with your InferenceHub account and chat with frontier
(Claude) and open-weight models — web search, deep research, and project-file RAG included.

Built with [Tauri 2](https://tauri.app) (Rust + system WebView) — no bundled Chromium, a few-MB
download. **No telemetry**: the app phones home to nothing; your chats go only to the InferenceHub
backend that serves them.

## Download

Grab the latest installer from the
[**Releases page**](https://github.com/InferenceHub/inferencehub-desktop/releases/latest):

| Platform | File |
| --- | --- |
| macOS (Apple Silicon) | `InferenceHub_<version>_aarch64.dmg` |
| Windows (x64) | `InferenceHub_<version>_x64-setup.exe` |

Current builds are **unsigned preview builds** — your OS will warn on first run. Code signing
(Apple notarization, Windows Authenticode) is in progress; until then:

**macOS** — drag **InferenceHub.app** into **/Applications**, then either open
**System Settings → Privacy & Security** and click **Open Anyway** next to the "InferenceHub was
blocked" notice (macOS 15+), or clear the quarantine flag from a terminal:

```sh
xattr -dr com.apple.quarantine /Applications/InferenceHub.app
```

**Windows** — run the setup `.exe` (per-user install, no admin prompt). SmartScreen shows
"Windows protected your PC" → click **More info → Run anyway**. If the WebView2 Runtime is missing
(rare), the installer fetches it automatically.

> **Windows Defender note:** unsigned, low-prevalence apps sometimes trip Defender's
> machine-learning heuristics (detections like `Trojan:Win32/Bearfoos.B!ml`). This is a known
> false-positive pattern for unsigned Tauri apps — the app is built in public CI from the source
> in this repository. We report each affected release to Microsoft for false-positive review, and
> signed builds are on the roadmap. If Defender quarantines the app, you can restore it and add an
> exclusion, or wait for the cleared/signed build.

## Features

| | macOS | Windows |
| --- | --- | --- |
| Chat (frontier + open-weight), web search, deep research, projects | ✅ | ✅ |
| Single sign-on via your system browser | ✅ | ✅ |
| Tray with live plan budget / runway | ✅ (menu-bar title + menu) | ✅ (tray menu) |
| Live transcription (on-device whisper.cpp) | ✅ | — |
| Stealth Mode (hide window from screen shares, ⌘.) | ✅ | — |
| Window opacity / always-on-top | ✅ | AoT only |

## Build from source

Requires [Rust](https://rustup.rs) (stable) and [Bun](https://bun.sh).

```sh
cd desktop
bun install
bun run tauri dev     # run locally
bun run tauri build   # package for your platform
```

On macOS, build the live-transcription helper once before packaging:

```sh
./src-tauri/helper/build-helper.sh
```

Windows installers are packaged by CI (`.github/workflows/build-windows.yml`) — NSIS packaging
needs a native Windows runner. See [`desktop/README.md`](desktop/README.md) for development notes.

## Privacy

- No analytics, no crash reporters, no update pings to third parties.
- Sign-in uses your default browser against the InferenceHub gateway; the app holds a session for
  `chat.inferencehub.tech` and a read-only status token for the tray budget display.
- Live transcription runs entirely on-device (whisper.cpp, Metal).

## License

[Apache-2.0](LICENSE). The Tauri shell derives from the
[Onyx](https://github.com/onyx-dot-app/onyx) desktop client (MIT); live transcription links
[whisper.cpp](https://github.com/ggml-org/whisper.cpp) (MIT); the project began as a fork of
[Jan](https://github.com/janhq/jan) (Apache-2.0). See [NOTICE](NOTICE) and
[THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md).
