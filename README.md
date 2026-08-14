# DeepSeek Harness Desktop

A desktop shell for [deepseek-ai/deepseek-harness](https://github.com/deepseek-ai/deepseek-harness).

The kernel (`dsh web` service) is fetched from GitHub and can be **updated with one click** (git pull + rebuild). The UI embeds the official web interface.

## Architecture

```
┌─────────────────────────────────────────────┐
│  Tauri 2 + Svelte 5 shell (this repo)       │
│  Top bar: status light / start-stop /       │
│           check update / settings           │
│  Content: iframe embedding the dsh Web UI   │
└──────────────┬──────────────────────────────┘
               │ spawn / kill / git
┌──────────────▼──────────────────────────────┐
│  deepseek-harness kernel (git checkout)     │
│  pnpm dsh web --port <auto-assigned>        │
└─────────────────────────────────────────────┘
```

- Rust backend (`src-tauri/src/`)
  - `kernel.rs` — kernel lifecycle: directory validation, free-port allocation, spawning `dsh web`, HTTP health checks, process-group termination
  - `updater.rs` — `git fetch` diff against remote → `git pull --ff-only` → `pnpm install` → `pnpm run build`; first-time install = `git clone` + build
  - `config.rs` — settings persistence (`~/Library/Application Support/com.deepseekharness.desktop/settings.json`)
- Frontend (`src/`): top-bar controls, live log panel, settings modal, update banner

## Development

```sh
pnpm install        # kernel dependencies (deepseek-harness checkout)
pnpm run build      # kernel build (web client bundle)
npm install         # this repo's dependencies
npm run tauri dev   # development mode
```

Verification triad:

```sh
cd src-tauri && cargo test --lib   # includes a real integration test:
                                   # spawns dsh web + health check + process-group kill
npm run check                     # svelte-check: 0 errors, 0 warnings
npm run build                     # frontend production build
```

Package:

```sh
npm run tauri build
# Artifacts:
#   src-tauri/target/release/bundle/macos/deepseek-harness-desktop.app
#   src-tauri/target/release/bundle/dmg/deepseek-harness-desktop_0.1.0_aarch64.dmg
```

## Usage

1. On first launch: in **Settings**, either point to an existing kernel directory (a deepseek-harness checkout) or use **Install Kernel Online** (auto git clone into the app data directory)
2. Click **Start Kernel** — a free port is assigned automatically and the web UI loads once the service is ready
3. In the embedded UI, go to Settings → Models, enter your DeepSeek API key, pick a workspace, and start a conversation
4. Kernel updates: with the kernel stopped, click **Check Update**; if a new version exists, click **Update Now** (git pull + pnpm install + build, with live logs)

Prerequisites (for online install / kernel start): Node.js ≥ 22 and pnpm (`corepack enable pnpm` or `npm i -g pnpm`).

## Known Limitations

- The kernel must be stopped before updating (avoids locked files / interrupted service)
- Updates are refused while the kernel directory has local modifications (dirty); handle them in a terminal first
- The port is auto-assigned, so it never conflicts with a manually running `dsh web`
