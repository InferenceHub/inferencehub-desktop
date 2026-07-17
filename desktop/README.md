# InferenceHub Desktop — development

The app is a thin Tauri 2 shell around the hosted InferenceHub chat: it navigates the system
WebView to `https://chat.inferencehub.tech` (override with the `IH_SERVER_URL` env var), signs the
user in via a system-browser SSO flow with a loopback redirect, and adds native affordances on
top (tray budget display, live transcription, Stealth Mode, window controls).

There is no frontend build: `frontendDist` points at the static `src/` splash, and the real UI is
the hosted chat.

## Layout

```
desktop/
├── src/                  # static splash the WebView boots into
└── src-tauri/
    ├── src/main.rs       # shell: windows, tray, menus, SSO, settings, STT bridge
    ├── helper/           # ih-stt-helper: whisper.cpp + Swift capture sidecar (macOS)
    ├── tauri.conf.json   # base config (macOS values)
    └── tauri.windows.conf.json  # Windows overrides (merged per-platform at build)
```

## Develop

Requires [Rust](https://rustup.rs) (stable) and [Bun](https://bun.sh) — the package manager here
is Bun, not npm/yarn.

```sh
bun install
bun run tauri dev
```

macOS-only features (live transcription, Stealth Mode, opacity, menu-bar budget title) are
`cfg`-gated; the shell builds and runs on Windows without them.

## Live-transcription helper (macOS)

`src-tauri/helper/build-helper.sh` clones + statically builds whisper.cpp (Metal), compiles the
Swift capture wrapper, and codesigns the result to `src-tauri/resources/ih-stt-helper`. Run it
once before `tauri build` on macOS — the bundle config expects the binary to exist. CI caches the
whisper.cpp build keyed on its pinned version.

## Release packaging

- **macOS**: `bun run tauri build --target aarch64-apple-darwin` (CI: `build-macos.yml`).
- **Windows**: packaged only in CI (`build-windows.yml`, NSIS needs native Windows).
  `bun run build:windows` (cargo-xwin) cross-*compiles* from macOS as a check but cannot package.

Tag `v*` and push — both workflows build and publish a shared GitHub release automatically.

## CI

`ci.yml` gates PRs (cargo check on macOS + Windows). The build workflows also run
`codesign --verify` on macOS bundles so a broken signature can't ship.
