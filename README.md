# DeepSeek Harness Desktop

DeepSeek Harness（[deepseek-ai/deepseek-harness](https://github.com/deepseek-ai/deepseek-harness)）的桌面壳应用。

内核（dsh web 服务）从 GitHub 获取并**随时可一键更新**（git pull + 重建），UI 内嵌官方 Web 界面。

## 架构

```
┌─────────────────────────────────────────────┐
│  Tauri 2 + Svelte 5 壳（本仓库）              │
│  顶栏：状态灯 / 启动停止 / 检查更新 / 设置     │
│  内容区：iframe 嵌入 dsh Web UI               │
└──────────────┬──────────────────────────────┘
               │ spawn / kill / git
┌──────────────▼──────────────────────────────┐
│  deepseek-harness 内核（git 仓库目录）         │
│  pnpm dsh web --port <自动分配>               │
└─────────────────────────────────────────────┘
```

- Rust 后端（`src-tauri/src/`）
  - `kernel.rs`：内核生命周期——目录校验、自动分配端口、spawn `dsh web`、HTTP 健康检查、进程组终止
  - `updater.rs`：`git fetch` 对比远端 → `git pull --ff-only` → `pnpm install` → `pnpm run build`；首次安装 = `git clone` + 构建
  - `config.rs`：设置持久化（`~/Library/Application Support/com.deepseekharness.desktop/settings.json`）
- 前端（`src/`）：顶栏控制条、日志面板（实时流）、设置弹窗、更新横幅

## 开发

```sh
pnpm install        # 内核依赖（deepseek-harness 仓库）
pnpm run build      # 内核构建（web 客户端 bundle）
npm install         # 本仓库依赖
npm run tauri dev   # 开发模式
```

验证三件套：

```sh
cd src-tauri && cargo test --lib   # 含真实集成测试：拉起 dsh web + 健康检查 + 进程组 kill
npm run check                     # svelte-check 0 errors 0 warnings
npm run build                     # 前端生产构建
```

打包：

```sh
npm run tauri build
# 产物：
#   src-tauri/target/release/bundle/macos/deepseek-harness-desktop.app
#   src-tauri/target/release/bundle/dmg/deepseek-harness-desktop_0.1.0_aarch64.dmg
```

## 使用

1. 首次打开：设置里「选择已有内核目录」（指向 deepseek-harness 仓库）或「在线安装内核」（自动 clone 到应用数据目录）
2. 点「启动内核」——自动分配端口并等待服务就绪，就绪后主区加载 Web UI
3. 在 Web UI 的 设置 → Models 填入 DeepSeek API Key，选择工作区后开始对话
4. 内核更新：内核停止时点「检查更新」，有新版则「立即更新」（git pull + pnpm install + build，日志实时可见）

前置要求（在线安装 / 启动内核需要）：Node.js ≥ 22 与 pnpm（`corepack enable pnpm` 或 `npm i -g pnpm`）。

## 已知限制

- 更新前需停止内核（防止构建时文件被占用/服务中断）
- 内核目录有本地改动（dirty）时拒绝更新，需先在终端处理
- 端口自动分配，与手动运行的 `dsh web` 不冲突
