<script lang="ts">
  import { onMount, tick } from "svelte";
  import { listen } from "@tauri-apps/api/event";
  import { open } from "@tauri-apps/plugin-dialog";
  import { openUrl } from "@tauri-apps/plugin-opener";
  import * as s from "$lib/state.svelte";
  import * as api from "$lib/api";

  let settingsOpen = $state(false);
  let busy: string | null = $state(null);
  let toastMsg = $state<string | null>(null);
  let toastKind = $state<"ok" | "err">("ok");
  let logBody: HTMLDivElement | undefined = $state();
  let envStatus: api.EnvStatusDto | null = $state(null);
  let envSetupPhase: api.EnvSetupStep | null = $state(null);
  let envSetupError: string | null = $state(null);
  let envSetupRunning = $state(false);
  let appSettings: api.SettingsDto | null = $state(null);
  let plugins = $state<string[]>([]);
  let pluginName = $state("");
  let pluginVersion = $state("");
  let appVersion = $state("0.1.0");
  let appUpdate: api.AppUpdateInfo | null = $state(null);
  let appCheckBusy = $state(false);
  let appUpdating = $state(false);
  let appUpdatePhase = $state<string | null>(null);
  let modalTab = $state<"status" | "kernel" | "plugins" | "prefs" | "about">("status");

  // Auto-scroll the log panel to the newest line.
  $effect(() => {
    s.store.logs.length;
    tick().then(() => {
      if (logBody) logBody.scrollTop = logBody.scrollHeight;
    });
  });

  // The iframe loads the shell proxy (fixed port 54001), which forwards
  // everything to the kernel but serves host.pickDirectory with the app's
  // native dialog (the kernel's osascript chooser cannot show in-process).
  const PROXY_PORT = 54001;
  const iframeUrl = $derived(
    s.store.kernel.status.state === "running"
      ? `http://127.0.0.1:${PROXY_PORT}`
      : null,
  );

  const port = $derived(
    s.store.kernel.status.state === "running"
      ? (s.store.kernel.status as { state: "running"; port: number }).port
      : null,
  );

  // Boot status copy shown on the welcome page while the kernel comes up.
  const bootStatusText = $derived.by(() => {
    switch (s.store.kernel.status.state) {
      case "starting":
        return "正在启动内核…";
      case "running":
        return "内核已就绪，正在加载界面…";
      case "error":
        return "内核启动失败，请查看日志";
      default:
        return s.store.logs.length > 0 ? "正在启动…" : "准备启动…";
    }
  });

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
      // Environment is installed automatically at launch; this is a guard for
      // the case where setup is still in progress or failed silently.
      if (!envStatus?.ready) {
        if (envSetupRunning) {
          return "环境准备中，请稍候…";
        }
        toast("检测到环境不完整，正在自动安装…");
        try {
          envStatus = await api.envSetupAuto();
          envSetupPhase = "done";
          if (!envStatus.ready) {
            return "环境安装后仍不满足：node=" + (envStatus.node.version ?? "缺失") + " pnpm=" + (envStatus.pnpm.version ?? "缺失");
          }
        } catch (e) {
          envSetupPhase = "error";
          envSetupError = String(e);
          return String(e);
        }
      }
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

  const envStepOrder = ["checking", "installing-node", "installing-pnpm", "verifying"];
  const envStepLabels: Record<string, string> = {
    checking: "检测环境",
    "installing-node": "安装 node",
    "installing-pnpm": "安装 pnpm",
    verifying: "验证完成",
  };
  const envSteps = $derived(Object.entries(envStepLabels).map(([id, label]) => ({ id, label })));

  function envStepState(id: string): "done" | "active" | "pending" {
    if (!envSetupPhase) return "pending";
    const cur = envStepOrder.indexOf(envSetupPhase);
    const idx = envStepOrder.indexOf(id);
    if (idx < cur) return "done";
    if (idx === cur) return "active";
    return "pending";
  }

  async function runEnvSetup() {
    if (envSetupRunning) return;
    envSetupRunning = true;
    envSetupPhase = "checking";
    envSetupError = null;
    try {
      envStatus = await api.envSetupAuto();
      envSetupPhase = "done";
      toast("环境已就绪", "ok");
    } catch (e) {
      envSetupPhase = "error";
      envSetupError = String(e);
      toast(String(e), "err");
    } finally {
      envSetupRunning = false;
    }
  }

  /// One-shot first-run flow: make sure the runtime env is ready, install the
  /// kernel if none is configured, then start it so the main UI shows up.
  async function bootstrap() {
    try {
      // 1. Environment: auto-install anything that's missing.
      if (!envStatus?.ready) {
        await runEnvSetup();
      }
      if (!envStatus?.ready) {
        console.warn("bootstrap skipped: 环境未就绪");
        return;
      }

      // 2. Kernel: auto-install when none is configured or the configured dir
      // is not a valid checkout (valid works even without git).
      await s.refreshStatus();
      if (!s.store.kernel.kernelDir || !s.store.kernel.valid) {
        toast("环境已就绪，正在自动安装内核…");
        s.store.logPanelOpen = true;
        s.appendLog("out", "== 自动安装内核（首次运行）==");
        const err = await s.installKernel();
        if (err) {
          toast("内核自动安装失败", "err");
          s.appendLog("err", `内核安装失败: ${err}`);
          return;
        }
        toast("内核安装完成，正在启动…");
      }

      // 3. Start the kernel (if not already running).
      if (s.store.kernel.status.state === "stopped") {
        const err = await s.startKernel();
        if (err) {
          s.appendLog("err", `启动失败: ${err}`);
          s.store.logPanelOpen = true;
        }
      }
    } catch (e) {
      console.error("bootstrap failed", e);
    }
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

  function openSettings() {
    settingsOpen = true;
    api.pluginList().then((p) => (plugins = p)).catch(() => {});
  }

  async function checkAppUpdate(silent = true) {
    appCheckBusy = true;
    try {
      const info = await api.appUpdateCheck();
      appUpdate = info;
      appVersion = info.current;
      if (info.update_available) {
        if (!silent) toast(`发现新版本 v${info.latest}`, "ok");
        // System notification so the user sees it even with the window hidden.
        try {
          const { isPermissionGranted, requestPermission, sendNotification } = await import(
            "@tauri-apps/plugin-notification"
          );
          let granted = await isPermissionGranted();
          if (!granted) granted = (await requestPermission()) === "granted";
          if (granted) {
            sendNotification({
              title: "DeepSeek Harness Desktop 有新版本",
              body: `v${info.latest} 已发布，点击打开下载页。`,
            });
          }
        } catch {
          // notifications are best-effort
        }
      }
    } catch {
      if (!silent) toast("检查应用更新失败", "err");
    } finally {
      appCheckBusy = false;
    }
  }

  function goDownload() {
    if (appUpdate?.url && !appUpdating) void openUrl(appUpdate.url);
  }

  async function doAutoUpdate() {
    if (!appUpdate?.update_available || appUpdating) return;
    appUpdating = true;
    appUpdatePhase = "downloading";
    try {
      await api.appDownloadUpdate();
      appUpdatePhase = "ready";
      toast("更新已就绪，应用即将重启…", "ok");
      // Closing the window quits the app; the detached install script then
      // swaps the .app and relaunches the new version automatically.
      setTimeout(() => {
        void import("@tauri-apps/api/window").then(({ getCurrentWindow }) =>
          getCurrentWindow().close(),
        );
      }, 1500);
    } catch (e) {
      appUpdating = false;
      appUpdatePhase = null;
      toast(String(e), "err");
    }
  }

  async function installPlugin() {
    if (pluginName.trim() === "") return;
    await run("plugin", async () => {
      try {
        await api.pluginInstall(pluginName.trim(), pluginVersion.trim() || undefined);
        pluginName = "";
        pluginVersion = "";
        plugins = await api.pluginList();
        toast("插件已安装，重启内核后生效");
        return null;
      } catch (e) {
        s.store.logPanelOpen = true;
        return String(e);
      }
    });
  }

  async function removePlugin(name: string) {
    await run("plugin", async () => {
      try {
        await api.pluginRemove(name);
        plugins = await api.pluginList();
        toast("插件已卸载，重启内核后生效");
        return null;
      } catch (e) {
        s.store.logPanelOpen = true;
        return String(e);
      }
    });
  }

  onMount(() => {
    try {
      s.wireEvents();
    } catch (e) {
      console.error("wireEvents failed", e);
    }
    s.refreshStatus().catch((e) => console.error("refreshStatus failed", e));
    api.checkEnv().then((e) => (envStatus = e)).catch((e) => console.error("checkEnv failed", e));
    api.getSettings().then((st) => (appSettings = st)).catch((e) => console.error("getSettings failed", e));
    syncTheme();
    // Auto-check for kernel updates shortly after launch (needs kernel dir),
    // then re-check periodically while the app runs.
    setTimeout(() => {
      if (s.store.kernel.kernelDir) void s.checkUpdate();
    }, 3000);
    updateTimer = setInterval(() => {
      if (s.store.kernel.kernelDir) void s.checkUpdate();
    }, 30 * 60 * 1000);
    // Full automatic bootstrap: env setup → kernel install → auto start.
    void bootstrap();
    // Auto-check for app (shell) updates shortly after launch.
    setTimeout(() => {
      void checkAppUpdate(false);
    }, 5000);
    // Rust watches ~/.dsh/settings.yaml (100ms) and pushes theme-changed;
    // keep a slow fallback poll in case the event is missed.
    themeTimer = setInterval(syncTheme, 2000);
    const un = listen("theme-changed", (e) => {
      const pref = (e.payload as { preference?: string | null }).preference ?? "system";
      applyTheme(pref);
    });
    const unProg = listen("app-update-progress", (e) => {
      const p = e.payload as { phase?: string; percent?: number };
      if (p.phase) appUpdatePhase = p.phase === "ready" ? "ready" : p.phase;
    });
    const unEnv = listen("env-setup-progress", (e) => {
      const p = e.payload as api.EnvSetupProgressEvent;
      if (!p) return;
      envSetupPhase = p.step;
      if (p.step === "error") envSetupError = p.message ?? String(e.payload);
    });
    return () => {
      clearInterval(themeTimer);
      clearInterval(updateTimer);
      un.then((fn) => fn()).catch(() => {});
      unProg.then((fn) => fn()).catch(() => {});
      unEnv.then((fn) => fn()).catch(() => {});
    };
  });

  // ---- Theme sync: follow the kernel UI's appearance (dsh settings.yaml) ----
  let themeTimer: ReturnType<typeof setInterval> | undefined;
  let updateTimer: ReturnType<typeof setInterval> | undefined;

  function applyTheme(pref: string) {
    const dark =
      pref === "dark" ||
      (pref === "system" && window.matchMedia("(prefers-color-scheme: dark)").matches);
    const root = document.documentElement;
    root.classList.toggle("theme-light", !dark);
    root.classList.toggle("theme-dark", dark);
  }

  async function syncTheme() {
    try {
      const t = await api.getTheme();
      applyTheme(t.preference ?? "system");
    } catch {
      // keep current theme on transient failures
    }
  }
</script>

<svelte:head>
  <title>DeepSeek Harness Desktop</title>
</svelte:head>

<main class="app">
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
        <div class="welcome-inner">
          <h1>DeepSeek Harness</h1>

          {#if envSetupPhase && envSetupPhase !== "done" && envSetupPhase !== "error"}
            <div class="env-steps" aria-label="环境自动安装进度">
              {#each envSteps as step (step.id)}
                {@const st = envStepState(step.id)}
                <div class="env-step" class:st-done={st === "done"} class:st-active={st === "active"}>
                  <span class="env-step-badge">
                    {#if st === "done"}✓{:else if st === "active"}<span class="env-step-spinner"></span>{:else}○{/if}
                  </span>
                  <span class="env-step-label">{step.label}</span>
                </div>
              {/each}
              {#if envSetupError}
                <p class="env-step-error">{envSetupError}</p>
              {/if}
            </div>
          {:else if envStatus}
            <p class={`env-status ${envStatus.ready ? "env-ok" : "env-warn"}`}>
              {#if envStatus.ready}
                Node {envStatus.node.version} · pnpm {envStatus.pnpm.version}
              {:else if envSetupError}
                环境安装失败：{envSetupError}
              {:else}
                需要 Node ≥ 22.19 与 pnpm，启动内核时将自动安装
              {/if}
            </p>
          {/if}

          {#if !s.store.kernel.kernelDir}
            <button class="btn-hero" onclick={pickDir} disabled={busy !== null}>
              选择内核目录
            </button>
            <button class="link" onclick={install} disabled={busy !== null || s.store.installing}>
              {s.store.installing ? "安装中…" : "或在线安装内核"}
            </button>
          {:else}
            <div class="boot">
              <div class={`boot-ring ${s.store.kernel.status.state === "running" ? "boot-ring-done" : ""}`} aria-hidden="true">
                <span class="boot-ring-core"></span>
              </div>
              <p class="boot-status">{bootStatusText}</p>
              <div class="boot-dots" aria-hidden="true">
                <span></span><span></span><span></span>
              </div>
            </div>
            <button class="link" onclick={openSettings}>内核管理</button>
          {/if}
        </div>
      </div>
    {/if}
  </section>

  <!-- ============ 底部栏 ============ -->
  <footer class="bottombar">
    <div class="bar-left">
      <div class={`status-pill pill-${s.store.kernel.status.state}`} title={kernelErrorHint ?? ""}>
        <span class={`dot dot-${s.store.kernel.status.state}`}></span>
        <span class="status-text">{stateLabel}</span>
      </div>
    </div>

    <div class="bar-right">
      <button class="btn btn-gear" onclick={openSettings} title="内核管理">
        <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
          <circle cx="12" cy="12" r="3"></circle>
          <path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 1 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 1 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1-1.51V3a2 2 0 1 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 1 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z"></path>
        </svg>
        <span>内核</span>
      </button>
    </div>
  </footer>

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
      <div class="modal modal-wide" role="dialog" aria-modal="true" aria-label="内核管理">
        <h2>内核管理</h2>

        <div class="modal-body">
          <nav class="modal-nav" aria-label="内核管理分区">
            <button class:nav-active={modalTab === "status"} onclick={() => (modalTab = "status")}>
              <span class="nav-dot" class:nav-dot-on={port !== null}></span>状态
            </button>
            <button class:nav-active={modalTab === "kernel"} onclick={() => (modalTab = "kernel")}>内核</button>
            <button class:nav-active={modalTab === "plugins"} onclick={() => (modalTab = "plugins")}>插件</button>
            <button class:nav-active={modalTab === "prefs"} onclick={() => (modalTab = "prefs")}>偏好</button>
            <button class:nav-active={modalTab === "about"} onclick={() => (modalTab = "about")}>关于</button>
          </nav>

          <section class="modal-pane">
            {#if modalTab === "status"}
              <div class={`status-card ${port ? "status-on" : ""}`}>
                <span class="status-card-dot"></span>
                <div class="status-card-info">
                  <p class="status-card-title">
                    {port ? "内核运行中" : "内核未运行"}
                  </p>
                  <p class="status-card-sub">
                    {#if port}
                      服务地址 <code>http://127.0.0.1:{port}</code>
                    {:else}
                      点击下方「启动内核」开始使用
                    {/if}
                  </p>
                </div>
              </div>
              {#if s.store.kernel.status.state === "error"}
                <div class="error-box">
                  {(s.store.kernel.status as { message: string }).message}
                </div>
              {/if}
              <div class="field">
                <span class="field-label">当前配置</span>
                <p class="hint" style="margin: 0">
                  {s.store.kernel.kernelDir ?? "未设置"}
                  {#if s.store.kernel.revision}
                    <span class="mono"> · {s.store.kernel.revision.slice(0, 7)}</span>
                  {/if}
                </p>
              </div>
            {:else if modalTab === "kernel"}
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
            {:else if modalTab === "plugins"}
              <div class="field">
                {#if plugins.length === 0}
                  <p class="hint" style="margin-top: 0">未安装插件。输入 npm 包名安装（如 dsh-better-sidebar），装完重启内核生效。</p>
                {:else}
                  <div class="plugin-list">
                    {#each plugins as p (p)}
                      <div class="plugin-row">
                        <span class="plugin-name">{p}</span>
                        <button class="btn btn-sm" onclick={() => removePlugin(p.split("@")[0])} disabled={busy !== null}>
                          卸载
                        </button>
                      </div>
                    {/each}
                  </div>
                {/if}
                <div class="row-inline" style="margin-top: 8px">
                  <input type="text" placeholder="插件包名（如 dsh-better-sidebar）" bind:value={pluginName} />
                  <input type="text" placeholder="版本（可选）" bind:value={pluginVersion} style="max-width: 100px" />
                  <button class="btn btn-primary" onclick={installPlugin} disabled={busy !== null || pluginName.trim() === ""}>
                    安装
                  </button>
                </div>
              </div>
            {:else if modalTab === "prefs"}
              <div class="field">
                <label class="check">
                  <input
                    type="checkbox"
                    checked={appSettings?.auto_start ?? false}
                    onchange={(e) => {
                      const v = (e.target as HTMLInputElement).checked;
                      api.setAutoStart(v).then((st) => (appSettings = st)).catch(() => {});
                    }}
                  />
                  启动时自动运行内核
                </label>
                <label class="check">
                  <input
                    type="checkbox"
                    checked={appSettings?.persist_logs ?? false}
                    onchange={(e) => {
                      const v = (e.target as HTMLInputElement).checked;
                      api.setPersistLogs(v).then((st) => (appSettings = st)).catch(() => {});
                    }}
                  />
                  内核日志写入文件
                </label>
                <p class="hint">日志文件保存在 ~/Library/Logs/deepseek-harness-desktop/kernel.log。</p>
              </div>
            {:else}
              {#if appUpdate?.update_available}
                <div class="update-banner">
                  <div>
                    <strong>发现新版本 v{appUpdate.latest}</strong>
                    <p>
                      {appUpdating
                        ? appUpdatePhase === "ready"
                          ? "更新已就绪，正在重启应用…"
                          : "正在下载更新，请稍候…"
                        : `当前版本 v${appUpdate.current}，可一键自动更新。`}
                    </p>
                  </div>
                  <button
                    class="btn btn-primary"
                    onclick={doAutoUpdate}
                    disabled={appUpdating}
                  >
                    {appUpdating ? (appUpdatePhase === "ready" ? "准备重启…" : "下载中…") : "立即更新"}
                  </button>
                </div>
              {/if}
              <div class="field">
                <div class="row-inline">
                  <span class="hint">DeepSeek Harness Desktop v{appVersion}</span>
                  <button class="btn btn-sm" onclick={() => checkAppUpdate(false)} disabled={appCheckBusy}>
                    {appCheckBusy ? "检查中…" : "检查更新"}
                  </button>
                </div>
                <button class="link link-sm" onclick={() => openUrl("https://github.com/vpertj/deepseek-harness-desktop/releases")}>
                  GitHub Releases ›
                </button>
                <p class="hint">新版本发布在 GitHub Releases，下载 dmg 安装即可。</p>
              </div>
            {/if}
          </section>
        </div>

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
  :root {
    --bg: #151517;
    --surface: #1b1b1c;
    --surface-2: #19191a;
    --surface-3: #131314;
    --text: #f9fafb;
    --text-dim: rgba(249, 250, 251, 0.5);
    --text-faint: rgba(249, 250, 251, 0.45);
    --border: rgba(255, 255, 255, 0.08);
    --border-strong: rgba(255, 255, 255, 0.1);
    --btn-bg: rgba(255, 255, 255, 0.06);
    --btn-hover: rgba(255, 255, 255, 0.12);
    --input-bg: rgba(255, 255, 255, 0.04);
    --accent: #679efe;
    --accent-hover: #7fadff;
    --accent-text: #151517;
  }
  :global(html.theme-light) {
    --bg: #f7f7f8;
    --surface: #ffffff;
    --surface-2: #f0f0f1;
    --surface-3: #e9e9ea;
    --text: #171717;
    --text-dim: rgba(0, 0, 0, 0.55);
    --text-faint: rgba(0, 0, 0, 0.45);
    --border: rgba(0, 0, 0, 0.08);
    --border-strong: rgba(0, 0, 0, 0.12);
    --btn-bg: rgba(0, 0, 0, 0.05);
    --btn-hover: rgba(0, 0, 0, 0.09);
    --input-bg: rgba(0, 0, 0, 0.04);
    --accent: #2f6bff;
    --accent-hover: #4680ff;
    --accent-text: #ffffff;
  }
  :global(body) {
    margin: 0;
    font-family: -apple-system, "PingFang SC", "Microsoft YaHei", system-ui, sans-serif;
    background: var(--bg);
    color: var(--text);
  }

  .app {
    display: flex;
    flex-direction: column;
    height: 100vh;
    overflow: hidden;
  }

  /* ---- 底部栏 ---- */
  .bottombar {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    padding: 7px 14px;
    background: var(--surface);
    border-top: 1px solid var(--border);
    flex-shrink: 0;
    user-select: none;
  }
  .bar-left,
  .bar-right {
    display: flex;
    align-items: center;
    gap: 10px;
    min-width: 0;
  }
  /* 状态胶囊徽章 */
  .status-pill {
    display: inline-flex;
    align-items: center;
    gap: 7px;
    padding: 3px 10px;
    border-radius: 999px;
    background: var(--btn-bg);
    border: 1px solid var(--border);
    cursor: default;
    transition: background 0.2s;
  }
  .pill-running {
    background: rgba(52, 211, 153, 0.1);
    border-color: rgba(52, 211, 153, 0.25);
  }
  .pill-starting {
    background: rgba(251, 191, 36, 0.1);
    border-color: rgba(251, 191, 36, 0.25);
  }
  .pill-error {
    background: rgba(248, 113, 113, 0.1);
    border-color: rgba(248, 113, 113, 0.25);
  }
  .dot {
    width: 7px;
    height: 7px;
    border-radius: 50%;
    background: #6b7280;
    flex-shrink: 0;
  }
  .dot-running {
    background: #34d399;
    box-shadow: 0 0 6px rgba(52, 211, 153, 0.7);
  }
  .dot-starting {
    background: #fbbf24;
    box-shadow: 0 0 6px rgba(251, 191, 36, 0.6);
    animation: pulse 1s infinite;
  }
  .dot-error {
    background: #f87171;
    box-shadow: 0 0 6px rgba(248, 113, 113, 0.7);
  }
  @keyframes pulse {
    50% {
      opacity: 0.3;
    }
  }
  .status-text {
    font-size: 12px;
    font-weight: 500;
    color: var(--text);
    white-space: nowrap;
  }
  /* 齿轮按钮 */
  .btn-gear {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    padding: 4px 12px;
  }
  .btn-gear svg {
    opacity: 0.75;
    transition: opacity 0.15s;
  }
  .btn-gear:hover svg {
    opacity: 1;
  }

  /* ---- 按钮（幽灵风，对齐内核 UI） ---- */
  .btn {
    background: var(--btn-bg);
    color: var(--text);
    border: 1px solid var(--border);
    border-radius: 8px;
    padding: 5px 12px;
    font-size: 12.5px;
    cursor: pointer;
    transition: background 0.15s;
  }
  .btn:hover:not(:disabled) {
    background: var(--btn-hover);
  }
  .btn:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }
  .btn-primary {
    background: var(--accent);
    border-color: var(--accent);
    color: var(--accent-text);
  }
  .btn-primary:hover:not(:disabled) {
    background: var(--accent-hover);
  }
  .btn-sm {
    padding: 3px 8px;
    font-size: 11.5px;
  }
  .btn-icon {
    min-width: 52px;
  }

  /* ---- 欢迎页 ---- */
  .welcome {
    height: 100%;
    display: flex;
    align-items: center;
    justify-content: center;
    padding: 24px;
  }
  .welcome-inner {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 14px;
    text-align: center;
    max-width: 480px;
  }
  .welcome-inner h1 {
    margin: 0;
    font-size: 26px;
    font-weight: 600;
    letter-spacing: -0.01em;
  }
  .lead {
    color: var(--text-dim);
    font-size: 13.5px;
    margin: 0 0 10px;
  }
  .env-status {
    font-size: 12px;
    margin: 0;
  }
  .env-ok {
    color: rgba(52, 211, 153, 0.85);
  }
  .env-warn {
    color: #fbbf24;
  }
  .env-steps {
    display: flex;
    flex-direction: column;
    gap: 8px;
    text-align: left;
    min-width: 320px;
    background: var(--btn-bg);
    border: 1px solid var(--border);
    border-radius: 12px;
    padding: 16px 18px;
  }
  .env-step {
    display: flex;
    align-items: center;
    gap: 10px;
    font-size: 13px;
    color: var(--text-faint);
  }
  .env-step.st-done {
    color: rgba(52, 211, 153, 0.9);
  }
  .env-step.st-active {
    color: var(--text);
  }
  .env-step-badge {
    width: 18px;
    height: 18px;
    border-radius: 50%;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    font-size: 11px;
    background: var(--btn-bg);
    border: 1px solid var(--border);
    flex-shrink: 0;
  }
  .env-step.st-done .env-step-badge {
    background: rgba(52, 211, 153, 0.15);
    border-color: rgba(52, 211, 153, 0.4);
    color: #34d399;
  }
  .env-step.st-active .env-step-badge {
    background: rgba(103, 158, 254, 0.15);
    border-color: rgba(103, 158, 254, 0.5);
  }
  .env-step-spinner {
    display: block;
    width: 10px;
    height: 10px;
    border-radius: 50%;
    border: 2px solid rgba(103, 158, 254, 0.3);
    border-top-color: var(--accent);
    animation: boot-spin 0.8s linear infinite;
  }
  .env-step-error {
    color: #fca5a5;
    font-size: 12px;
    margin: 4px 0 0;
  }
  .boot {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 14px;
    padding: 18px 0 8px;
  }
  .boot-ring {
    position: relative;
    width: 46px;
    height: 46px;
    border-radius: 50%;
    background: conic-gradient(from 0deg, rgba(103, 158, 254, 0), #679efe 85%, rgba(103, 158, 254, 0));
    -webkit-mask: radial-gradient(farthest-side, transparent calc(100% - 3px), #000 calc(100% - 2px));
    mask: radial-gradient(farthest-side, transparent calc(100% - 3px), #000 calc(100% - 2px));
    animation: boot-spin 0.9s linear infinite;
  }
  .boot-ring-core {
    position: absolute;
    inset: 13px;
    border-radius: 50%;
    background: rgba(103, 158, 254, 0.25);
    animation: boot-breathe 1.6s ease-in-out infinite;
  }
  .boot-ring-done {
    background: conic-gradient(from 0deg, rgba(52, 211, 153, 0), #34d399 85%, rgba(52, 211, 153, 0));
    animation: none;
  }
  .boot-ring-done .boot-ring-core {
    background: rgba(52, 211, 153, 0.3);
    animation: none;
  }
  @keyframes boot-spin {
    to {
      transform: rotate(360deg);
    }
  }
  @keyframes boot-breathe {
    0%,
    100% {
      transform: scale(0.85);
      opacity: 0.5;
    }
    50% {
      transform: scale(1);
      opacity: 1;
    }
  }
  .boot-status {
    margin: 0;
    font-size: 14.5px;
    font-weight: 500;
    color: var(--text);
    letter-spacing: 0.01em;
  }
  .boot-dots {
    display: flex;
    gap: 7px;
  }
  .boot-dots span {
    width: 6px;
    height: 6px;
    border-radius: 50%;
    background: var(--accent);
    animation: boot-bounce 1.2s ease-in-out infinite;
  }
  .boot-dots span:nth-child(2) {
    animation-delay: 0.15s;
  }
  .boot-dots span:nth-child(3) {
    animation-delay: 0.3s;
  }
  @keyframes boot-bounce {
    0%,
    80%,
    100% {
      transform: translateY(0);
      opacity: 0.35;
    }
    40% {
      transform: translateY(-4px);
      opacity: 1;
    }
  }
  .btn-hero {
    background: var(--accent);
    border: none;
    border-radius: 10px;
    color: var(--accent-text);
    font-size: 14.5px;
    font-weight: 600;
    padding: 10px 36px;
    cursor: pointer;
    transition: background 0.15s, transform 0.1s;
  }
  .btn-hero:hover:not(:disabled) {
    background: var(--accent-hover);
  }
  .btn-hero:active:not(:disabled) {
    transform: scale(0.98);
  }
  .btn-hero:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }
  .link {
    background: none;
    border: none;
    color: var(--text-dim);
    font-size: 12.5px;
    cursor: pointer;
    padding: 4px 8px;
    border-radius: 6px;
    transition: color 0.15s;
  }
  .link:hover:not(:disabled) {
    color: var(--text);
    background: var(--btn-bg);
  }
  .link-sm {
    font-size: 12px;
    padding: 3px 6px;
  }
  .link:disabled {
    opacity: 0.5;
    cursor: not-allowed;
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
    background: rgba(248, 113, 113, 0.12);
    color: #fca5a5;
    border-bottom: 1px solid rgba(248, 113, 113, 0.2);
  }
  .update-banner {
    background: rgba(52, 211, 153, 0.1);
    color: #86efac;
    border-bottom: 1px solid rgba(52, 211, 153, 0.2);
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
    background: var(--bg);
  }

  .hint {
    color: var(--text-faint);
    font-size: 12px;
    margin: 12px 0 0;
  }

  /* ---- 日志面板 ---- */
  .log-panel {
    height: 220px;
    border-top: 1px solid var(--border);
    background: var(--surface-3);
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
    color: var(--text-faint);
    background: var(--surface-2);
    border-bottom: 1px solid var(--border);
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
    color: var(--text-dim);
  }
  .log-err {
    color: #fca5a5;
  }
  .log-empty {
    color: var(--text-faint);
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
    max-height: 82vh;
    display: flex;
    flex-direction: column;
    background: var(--surface);
    border: 1px solid var(--border-strong);
    border-radius: 14px;
    padding: 24px 26px;
    box-shadow: 0 24px 60px rgba(0, 0, 0, 0.5);
  }
  .modal-wide {
    width: 700px;
  }
  .modal-body {
    display: flex;
    gap: 18px;
    min-height: 360px;
    flex: 1;
    overflow: hidden;
  }
  .modal-nav {
    display: flex;
    flex-direction: column;
    gap: 2px;
    width: 108px;
    flex-shrink: 0;
    border-right: 1px solid var(--border);
    padding-right: 14px;
  }
  .modal-nav button {
    display: flex;
    align-items: center;
    gap: 8px;
    background: transparent;
    border: none;
    border-radius: 8px;
    padding: 8px 10px;
    font-size: 13px;
    color: var(--text-dim);
    cursor: pointer;
    text-align: left;
    transition: background 0.12s, color 0.12s;
  }
  .modal-nav button:hover {
    background: var(--btn-bg);
    color: var(--text);
  }
  .modal-nav button.nav-active {
    background: rgba(103, 158, 254, 0.12);
    color: var(--accent);
    font-weight: 500;
  }
  .nav-dot {
    width: 7px;
    height: 7px;
    border-radius: 50%;
    background: #6b7280;
    flex-shrink: 0;
  }
  .nav-dot.nav-dot-on {
    background: #34d399;
    box-shadow: 0 0 5px rgba(52, 211, 153, 0.6);
  }
  .modal-pane {
    flex: 1;
    overflow-y: auto;
    padding-right: 4px;
  }
  .status-card {
    display: flex;
    align-items: center;
    gap: 12px;
    background: var(--btn-bg);
    border: 1px solid var(--border);
    border-radius: 12px;
    padding: 16px;
    margin-bottom: 16px;
  }
  .status-card.status-on {
    background: rgba(52, 211, 153, 0.08);
    border-color: rgba(52, 211, 153, 0.3);
  }
  .status-card-dot {
    width: 12px;
    height: 12px;
    border-radius: 50%;
    background: #6b7280;
    flex-shrink: 0;
  }
  .status-card.status-on .status-card-dot {
    background: #34d399;
    box-shadow: 0 0 8px rgba(52, 211, 153, 0.8);
  }
  .status-card-info {
    min-width: 0;
  }
  .status-card-title {
    margin: 0;
    font-size: 14.5px;
    font-weight: 600;
    color: var(--text);
  }
  .status-card-sub {
    margin: 3px 0 0;
    font-size: 12px;
    color: var(--text-dim);
  }
  .status-card.status-on .status-card-title,
  .status-card.status-on .status-card-sub {
    color: #34d399;
  }
  .status-card code {
    background: rgba(52, 211, 153, 0.12);
    border: 1px solid rgba(52, 211, 153, 0.3);
    border-radius: 5px;
    padding: 0 5px;
    font-size: 11px;
  }
  .update-banner {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    background: rgba(103, 158, 254, 0.1);
    border: 1px solid rgba(103, 158, 254, 0.35);
    border-radius: 12px;
    padding: 14px 16px;
    margin-bottom: 16px;
  }
  .update-banner strong {
    font-size: 13.5px;
    color: var(--accent);
  }
  .update-banner p {
    margin: 3px 0 0;
    font-size: 12px;
    color: var(--text-dim);
  }
  .mono {
    font-family: ui-monospace, "SF Mono", Menlo, monospace;
    font-size: 11.5px;
    color: var(--text-dim);
  }
  .modal h2 {
    margin: 0 0 16px;
    font-size: 16px;
  }
  .field {
    margin-bottom: 18px;
  }
  .field label,
  .field-label {
    display: block;
    font-size: 12.5px;
    color: var(--text-dim);
    margin-bottom: 6px;
  }
  .field input {
    flex: 1;
    min-width: 0;
    background: var(--input-bg);
    border: 1px solid var(--border-strong);
    border-radius: 8px;
    color: var(--text);
    padding: 7px 10px;
    font-size: 12.5px;
  }
  .field input:focus {
    outline: none;
    border-color: var(--accent);
  }
  .row-inline {
    display: flex;
    gap: 8px;
    align-items: center;
  }
  .check {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    font-size: 12.5px;
    color: var(--text);
    cursor: pointer;
    margin: 0;
  }
  .check input {
    accent-color: var(--accent);
  }
  .plugin-list {
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  .plugin-row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
    background: var(--btn-bg);
    border: 1px solid var(--border);
    border-radius: 8px;
    padding: 6px 10px;
    font-size: 12.5px;
  }
  .plugin-name {
    font-family: ui-monospace, "SF Mono", Menlo, monospace;
    font-size: 12px;
    word-break: break-all;
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
    background: var(--surface);
    border: 1px solid var(--border-strong);
    color: var(--text);
    padding: 10px 18px;
    border-radius: 10px;
    font-size: 13px;
    box-shadow: 0 10px 30px rgba(0, 0, 0, 0.45);
    z-index: 100;
  }
  .toast-err {
    background: rgba(248, 113, 113, 0.15);
    border-color: rgba(248, 113, 113, 0.3);
    color: #fca5a5;
  }
</style>
