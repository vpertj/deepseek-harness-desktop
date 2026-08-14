<script lang="ts">
  import { onMount, tick } from "svelte";
  import { open } from "@tauri-apps/plugin-dialog";
  import * as s from "$lib/state.svelte";
  import * as api from "$lib/api";

  let settingsOpen = $state(false);
  let busy: string | null = $state(null);
  let toastMsg = $state<string | null>(null);
  let toastKind = $state<"ok" | "err">("ok");
  let logBody: HTMLDivElement | undefined = $state();

  // Auto-scroll the log panel to the newest line.
  $effect(() => {
    s.store.logs.length;
    if (logBody) {
      tick().then(() => {
        if (logBody) logBody.scrollTop = logBody.scrollHeight;
      });
    }
  });

  const iframeUrl = $derived(
    s.store.kernel.status.state === "running"
      ? `http://127.0.0.1:${(s.store.kernel.status as { state: "running"; port: number }).port}`
      : null,
  );

  const kernelErrorHint = $derived(
    s.store.kernel.status.state === "error"
      ? (s.store.kernel.status as { state: "error"; message: string }).message
      : null,
  );

  const stateLabel = $derived.by(() => {
    switch (s.store.kernel.status.state) {
      case "running":
        return "运行中";
      case "starting":
        return "启动中…";
      case "error":
        return "错误";
      default:
        return "已停止";
    }
  });

  let toastTimer: ReturnType<typeof setTimeout> | undefined;
  function toast(msg: string, kind: "ok" | "err" = "ok") {
    toastMsg = msg;
    toastKind = kind;
    clearTimeout(toastTimer);
    toastTimer = setTimeout(() => (toastMsg = null), 4000);
  }

  async function run(label: string, fn: () => Promise<string | null>) {
    if (busy) return;
    busy = label;
    // Safety net: never let the UI stay stuck if an invoke hangs.
    const timer = setTimeout(() => {
      busy = null;
      toast("操作超时，请重试", "err");
    }, 10 * 60 * 1000);
    const err = await fn();
    clearTimeout(timer);
    busy = null;
    if (err) toast(err, "err");
  }

  async function start() {
    await run("start", async () => {
      const err = await s.startKernel();
      if (!err) {
        toast("内核启动中，请稍候…");
      } else {
        s.appendLog("err", `启动失败: ${err}`);
        s.store.logPanelOpen = true;
      }
      return err;
    });
  }

  async function stop() {
    await run("stop", async () => {
      const err = await s.stopKernel();
      if (!err) toast("内核已停止");
      return err;
    });
  }

  async function check() {
    await run("check", async () => {
      const err = await s.checkUpdate();
      if (!err && s.store.updateInfo?.update_available) {
        toast(`发现新版本 ${s.store.updateInfo.current} → ${s.store.updateInfo.latest}（落后 ${s.store.updateInfo.behind} 个提交）`);
      } else if (!err) {
        toast("已是最新版本");
      }
      return err;
    });
  }

  async function apply() {
    await run("update", async () => {
      if (s.store.kernel.status.state !== "stopped") {
        return "请先停止内核，再进行更新";
      }
      const err = await s.applyUpdate();
      if (!err) toast("内核更新完成，可以启动");
      return err;
    });
  }

  async function pickDir() {
    const picked = await open({ directory: true, title: "选择 deepseek-harness 内核目录" });
    if (typeof picked === "string" && picked) {
      await run("setdir", async () => {
        const err = await s.setDir(picked);
        if (!err) toast("内核目录已设置");
        return err;
      });
    }
  }

  async function install() {
    await run("install", async () => {
      const err = await s.installKernel();
      if (!err) {
        toast("内核安装完成，可以启动");
        settingsOpen = false;
      }
      return err;
    });
  }

  onMount(() => {
    s.wireEvents();
    s.refreshStatus();
  });
</script>

<svelte:head>
  <title>DeepSeek Harness Desktop</title>
</svelte:head>

<main class="app">
  <!-- ============ 顶栏 ============ -->
  <header class="topbar">
    <div class="brand">
      <span class="logo">DSH</span>
      <div class="brand-text">
        <span class="name">DeepSeek Harness Desktop</span>
        <span class="sub">
          {#if s.store.kernel.kernelDir}
            {s.store.kernel.revision ? `v${s.store.kernel.revision}` : "内核"} · {s.store.kernel.kernelDir}
            {s.store.kernel.dirty ? " · 有本地改动" : ""}
          {:else}
            未配置内核
          {/if}
        </span>
      </div>
    </div>

    <div class="actions">
      <span class={`pill pill-${s.store.kernel.status.state}`} title={kernelErrorHint}>
        <span class="dot"></span>
        {stateLabel}
      </span>

      {#if s.store.kernel.status.state === "running"}
        <button class="btn" onclick={stop} disabled={busy !== null}>停止</button>
      {:else if s.store.kernel.status.state === "starting"}
        <button class="btn" disabled>启动中…</button>
      {:else}
        <button class="btn btn-primary" onclick={start} disabled={busy !== null || !s.store.kernel.kernelDir}>
          启动内核
        </button>
      {/if}

      <button class="btn" onclick={check} disabled={busy !== null || !s.store.kernel.kernelDir || s.store.kernel.status.state !== "stopped"}
        title="从 GitHub 检查内核更新">
        {s.store.checkingUpdate ? "检查中…" : "检查更新"}
      </button>

      <button class="btn" onclick={() => (settingsOpen = true)}>设置</button>
      <button class="btn btn-icon" onclick={() => (s.store.logPanelOpen = !s.store.logPanelOpen)} title="内核日志">
        {s.store.logPanelOpen ? "隐藏日志" : "日志"}
      </button>
    </div>
  </header>

  {#if kernelErrorHint}
    <div class="error-banner">{kernelErrorHint}</div>
  {/if}

  {#if s.store.updateInfo?.update_available && s.store.kernel.status.state === "stopped"}
    <div class="update-banner">
      <span>
        内核有更新：{s.store.updateInfo.current} → {s.store.updateInfo.latest}（落后 {s.store.updateInfo.behind} 个提交）
      </span>
      <button class="btn btn-primary btn-sm" onclick={apply} disabled={busy !== null || s.store.updating}>
        {s.store.updating ? "更新中…" : "立即更新"}
      </button>
    </div>
  {/if}

  <!-- ============ 内容区 ============ -->
  <section class="content">
    {#if iframeUrl}
      <iframe title="DeepSeek Harness" src={iframeUrl} class="frame" allow="clipboard-write"></iframe>
    {:else}
      <div class="welcome">
        <div class="welcome-card">
          <h1>DeepSeek Harness Desktop</h1>
          <p class="lead">
            本地运行 deepseek-harness 内核的桌面壳。内核从
            <code>github.com/deepseek-ai/deepseek-harness</code>
            获取，可随时一键更新。
          </p>

          {#if !s.store.kernel.kernelDir}
            <div class="welcome-actions">
              <button class="btn btn-primary" onclick={pickDir} disabled={busy !== null}>
                选择已有内核目录
              </button>
              <button class="btn" onclick={install} disabled={busy !== null || s.store.installing}>
                {s.store.installing ? "安装中…（克隆 + 构建，需要几分钟）" : "在线安装内核"}
              </button>
            </div>
            <p class="hint">需要本机已安装 Node.js ≥ 22 与 pnpm（corepack enable pnpm）。</p>
          {:else}
            <div class="welcome-actions">
              <button class="btn btn-primary" onclick={start} disabled={busy !== null}>
                启动内核
              </button>
            </div>
            <p class="hint">启动后在此处加载 DeepSeek Harness Web UI。</p>
          {/if}

          <div class="guide">
            <h3>使用步骤</h3>
            <ol>
              <li>配置内核：选择已 clone 的仓库目录，或在线安装</li>
              <li>启动内核（自动分配端口）</li>
              <li>在界面中 设置 → Models 填入 DeepSeek API Key，选择工作区后开始对话</li>
              <li>内核更新：点顶栏「检查更新」，有新版时一键拉取 + 重建</li>
            </ol>
          </div>
        </div>
      </div>
    {/if}
  </section>

  <!-- ============ 日志面板 ============ -->
  {#if s.store.logPanelOpen}
    <section class="log-panel" aria-label="内核日志">
      <div class="log-head">
        <span>内核日志</span>
        <span class="log-actions">
          <button class="btn btn-sm" onclick={() => s.clearLogs()}>清空</button>
          <button class="btn btn-sm" onclick={() => (s.store.logPanelOpen = false)}>收起</button>
        </span>
      </div>
      <div class="log-body" bind:this={logBody}>
        {#if s.store.logs.length === 0}
          <div class="log-empty">暂无日志</div>
        {:else}
          {#each s.store.logs as log (log.ts + log.line)}
            <div class={`log-line log-${log.stream}`}>{log.line}</div>
          {/each}
        {/if}
      </div>
    </section>
  {/if}

  <!-- ============ 设置弹窗 ============ -->
  {#if settingsOpen}
    <div class="modal-mask" role="presentation" onclick={(e) => { if (e.target === e.currentTarget) settingsOpen = false; }}>
      <div class="modal" role="dialog" aria-modal="true" aria-label="设置">
        <h2>设置</h2>

        <div class="field">
          <label for="kernel-dir">内核目录（deepseek-harness 仓库）</label>
          <div class="row-inline">
            <input id="kernel-dir" type="text" readonly value={s.store.kernel.kernelDir ?? ""} placeholder="尚未设置" />
            <button class="btn" onclick={pickDir} disabled={busy !== null}>选择目录</button>
          </div>
          <p class="hint">选择一个已 clone 的 deepseek-harness 仓库（需含 package.json 与 pnpm-workspace.yaml）。</p>
        </div>

        <div class="field">
          <label for="install-btn">在线安装</label>
          <button id="install-btn" class="btn" onclick={install} disabled={busy !== null || s.store.installing}>
            {s.store.installing ? "安装中…" : "克隆并安装内核到应用数据目录"}
          </button>
          <p class="hint">自动 git clone + pnpm install + build，无需手动准备目录。</p>
        </div>

        <div class="field">
          <label for="check-btn">内核更新</label>
          <div class="row-inline">
            <button id="check-btn" class="btn" onclick={check} disabled={busy !== null || !s.store.kernel.kernelDir || s.store.kernel.status.state !== "stopped"}>
              {s.store.checkingUpdate ? "检查中…" : "检查更新"}
            </button>
            {#if s.store.updateInfo}
              <span class="hint">
                {#if s.store.updateInfo.update_available}
                  当前 {s.store.updateInfo.current} → 最新 {s.store.updateInfo.latest}（落后 {s.store.updateInfo.behind}）
                {:else}
                  当前 {s.store.updateInfo.current}，已是最新
                {/if}
              </span>
            {/if}
          </div>
        </div>

        {#if s.store.lastUpdateError}
          <div class="error-box">{s.store.lastUpdateError}</div>
        {/if}

        <div class="modal-foot">
          <button class="btn" onclick={() => (settingsOpen = false)}>关闭</button>
          <button
            class="btn btn-primary"
            onclick={async () => {
              await start();
              if (s.store.kernel.status.state !== "starting" && s.store.kernel.status.state !== "running") return;
              settingsOpen = false;
            }}
            disabled={busy !== null || !s.store.kernel.kernelDir}
          >
            {s.store.kernel.status.state === "running" ? "内核运行中" : "启动内核"}
          </button>
        </div>
      </div>
    </div>
  {/if}

  {#if toastMsg}
    <div class={`toast toast-${toastKind}`}>{toastMsg}</div>
  {/if}
</main>

<style>
  :global(body) {
    margin: 0;
    font-family: -apple-system, "PingFang SC", "Microsoft YaHei", system-ui, sans-serif;
    background: #0f1115;
    color: #e6e8ee;
  }

  .app {
    display: flex;
    flex-direction: column;
    height: 100vh;
    overflow: hidden;
  }

  /* ---- 顶栏 ---- */
  .topbar {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    padding: 8px 14px;
    background: #161a22;
    border-bottom: 1px solid #262c3a;
    flex-shrink: 0;
  }
  .brand {
    display: flex;
    align-items: center;
    gap: 10px;
    min-width: 0;
  }
  .logo {
    width: 30px;
    height: 30px;
    border-radius: 8px;
    background: linear-gradient(135deg, #4d7cfe, #1e4fd8);
    color: #fff;
    font-weight: 700;
    font-size: 12px;
    display: flex;
    align-items: center;
    justify-content: center;
    flex-shrink: 0;
  }
  .brand-text {
    display: flex;
    flex-direction: column;
    min-width: 0;
  }
  .name {
    font-weight: 600;
    font-size: 13px;
  }
  .sub {
    font-size: 11px;
    color: #8b93a7;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    max-width: 420px;
  }
  .actions {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-shrink: 0;
  }

  /* ---- 按钮 ---- */
  .btn {
    background: #212734;
    color: #dfe3ec;
    border: 1px solid #333c4e;
    border-radius: 7px;
    padding: 6px 12px;
    font-size: 12.5px;
    cursor: pointer;
    transition: background 0.15s;
  }
  .btn:hover:not(:disabled) {
    background: #2a3242;
  }
  .btn:disabled {
    opacity: 0.45;
    cursor: not-allowed;
  }
  .btn-primary {
    background: #2f6bff;
    border-color: #2f6bff;
    color: #fff;
  }
  .btn-primary:hover:not(:disabled) {
    background: #4680ff;
  }
  .btn-sm {
    padding: 3px 8px;
    font-size: 11.5px;
  }
  .btn-icon {
    min-width: 52px;
  }

  /* ---- 状态灯 ---- */
  .pill {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    font-size: 12px;
    padding: 4px 10px;
    border-radius: 999px;
    border: 1px solid #333c4e;
    background: #1b2029;
  }
  .dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: #6b7280;
  }
  .pill-running .dot {
    background: #34d399;
    box-shadow: 0 0 6px #34d39988;
  }
  .pill-starting .dot {
    background: #fbbf24;
    animation: pulse 1s infinite;
  }
  .pill-error .dot {
    background: #f87171;
  }
  @keyframes pulse {
    50% {
      opacity: 0.3;
    }
  }

  /* ---- 横幅 ---- */
  .error-banner,
  .update-banner {
    padding: 8px 14px;
    font-size: 12.5px;
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 10px;
  }
  .error-banner {
    background: #3a1618;
    color: #fca5a5;
    border-bottom: 1px solid #5c2326;
  }
  .update-banner {
    background: #1c2a12;
    color: #bef264;
    border-bottom: 1px solid #33471f;
  }

  /* ---- 内容区 ---- */
  .content {
    flex: 1;
    min-height: 0;
    position: relative;
  }
  .frame {
    width: 100%;
    height: 100%;
    border: none;
    background: #0f1115;
  }

  .welcome {
    height: 100%;
    display: flex;
    align-items: center;
    justify-content: center;
    padding: 24px;
    overflow: auto;
  }
  .welcome-card {
    max-width: 560px;
    background: #161a22;
    border: 1px solid #262c3a;
    border-radius: 14px;
    padding: 32px 36px;
  }
  .welcome-card h1 {
    margin: 0 0 8px;
    font-size: 20px;
  }
  .lead {
    color: #aab2c4;
    font-size: 13.5px;
    line-height: 1.6;
    margin: 0 0 20px;
  }
  .lead code {
    background: #222836;
    padding: 1px 6px;
    border-radius: 4px;
    font-size: 12px;
  }
  .welcome-actions {
    display: flex;
    gap: 10px;
    flex-wrap: wrap;
  }
  .hint {
    color: #7c8498;
    font-size: 12px;
    margin: 12px 0 0;
  }
  .guide {
    margin-top: 24px;
    border-top: 1px solid #262c3a;
    padding-top: 16px;
  }
  .guide h3 {
    margin: 0 0 8px;
    font-size: 13px;
    color: #9aa3b8;
  }
  .guide ol {
    margin: 0;
    padding-left: 18px;
    font-size: 12.5px;
    color: #aab2c4;
    line-height: 1.9;
  }

  /* ---- 日志面板 ---- */
  .log-panel {
    height: 220px;
    border-top: 1px solid #262c3a;
    background: #0d1016;
    display: flex;
    flex-direction: column;
    flex-shrink: 0;
  }
  .log-head {
    display: flex;
    justify-content: space-between;
    align-items: center;
    padding: 5px 10px;
    font-size: 12px;
    color: #8b93a7;
    background: #141821;
    border-bottom: 1px solid #262c3a;
  }
  .log-actions {
    display: flex;
    gap: 6px;
  }
  .log-body {
    flex: 1;
    overflow: auto;
    padding: 6px 10px;
    font-family: ui-monospace, "SF Mono", Menlo, monospace;
    font-size: 11.5px;
    line-height: 1.55;
  }
  .log-line {
    white-space: pre-wrap;
    word-break: break-all;
    color: #c3cadb;
  }
  .log-err {
    color: #fca5a5;
  }
  .log-empty {
    color: #5c6374;
  }

  /* ---- 弹窗 ---- */
  .modal-mask {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.55);
    backdrop-filter: blur(4px);
    display: flex;
    align-items: center;
    justify-content: center;
    z-index: 50;
  }
  .modal {
    width: 480px;
    max-width: calc(100vw - 40px);
    background: #161a22;
    border: 1px solid #2c3345;
    border-radius: 14px;
    padding: 24px 26px;
    box-shadow: 0 24px 60px rgba(0, 0, 0, 0.5);
  }
  .modal h2 {
    margin: 0 0 18px;
    font-size: 16px;
  }
  .field {
    margin-bottom: 18px;
  }
  .field label {
    display: block;
    font-size: 12.5px;
    color: #9aa3b8;
    margin-bottom: 6px;
  }
  .field input {
    flex: 1;
    min-width: 0;
    background: #0f1219;
    border: 1px solid #2c3345;
    border-radius: 7px;
    color: #dfe3ec;
    padding: 7px 10px;
    font-size: 12.5px;
  }
  .row-inline {
    display: flex;
    gap: 8px;
    align-items: center;
  }
  .error-box {
    background: #3a1618;
    border: 1px solid #5c2326;
    color: #fca5a5;
    border-radius: 8px;
    padding: 10px 12px;
    font-size: 12.5px;
    margin-bottom: 16px;
    white-space: pre-wrap;
    word-break: break-all;
  }
  .modal-foot {
    display: flex;
    justify-content: flex-end;
    margin-top: 8px;
  }

  /* ---- toast ---- */
  .toast {
    position: fixed;
    bottom: 24px;
    left: 50%;
    transform: translateX(-50%);
    background: #1d2432;
    border: 1px solid #333c4e;
    color: #e6e8ee;
    padding: 10px 18px;
    border-radius: 10px;
    font-size: 13px;
    box-shadow: 0 10px 30px rgba(0, 0, 0, 0.45);
    z-index: 100;
  }
  .toast-err {
    background: #3a1618;
    border-color: #5c2326;
    color: #fca5a5;
  }
</style>
