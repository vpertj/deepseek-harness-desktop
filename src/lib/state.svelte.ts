import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import {
  isPermissionGranted,
  requestPermission,
  sendNotification,
} from "@tauri-apps/plugin-notification";
import * as api from "./api";

export interface LogLine {
  stream: "out" | "err";
  line: string;
  ts: number;
}

// All mutable state lives inside one $state object so mutations only touch
// properties (Svelte 5 forbids reassigning exported module state).
export const store = $state({
  kernel: {
    status: { state: "stopped" } as api.KernelStatus["status"],
    kernelDir: null as string | null,
    revision: null as string | null,
    dirty: false,
    valid: false,
    token: null as string | null,
  },
  updateInfo: null as api.UpdateInfo | null,
  checkingUpdate: false,
  updating: false,
  installing: false,
  lastUpdateError: null as string | null,
  logs: [] as LogLine[],
  logPanelOpen: false,
  /** Latest version we've already notified about (dedupe). */
  updateNotified: null as string | null,
});

/** Send a macOS notification (requests permission on first use). */
async function notifyUpdate(title: string, body: string) {
  try {
    let granted = await isPermissionGranted();
    if (!granted) {
      granted = (await requestPermission()) === "granted";
    }
    if (granted) {
      sendNotification({ title, body });
    }
  } catch {
    // notifications are best-effort
  }
}

// Derived helpers live in components ($derived cannot be exported from modules).

const MAX_LOGS = 3000;

// Batch incoming log lines and flush to the reactive store at most every
// LOG_FLUSH_MS. Bursty output (pnpm install etc.) emits dozens of events per
// second; pushing each line individually re-renders the log panel and forces
// a scrollHeight reflow per line. Buffering keeps the DOM update cadence low
// without changing what is displayed.
const LOG_FLUSH_MS = 120;
let logBuf: LogLine[] = [];
let logFlushTimer: ReturnType<typeof setTimeout> | null = null;

function flushLogs() {
  logFlushTimer = null;
  if (logBuf.length === 0) return;
  const batch = logBuf;
  logBuf = [];
  store.logs.push(...batch);
  if (store.logs.length > MAX_LOGS) store.logs.splice(0, store.logs.length - MAX_LOGS);
}

export function appendLog(stream: "out" | "err", line: string) {
  logBuf.push({ stream, line, ts: Date.now() });
  if (logBuf.length >= 200) {
    if (logFlushTimer !== null) {
      clearTimeout(logFlushTimer);
      logFlushTimer = null;
    }
    flushLogs();
    return;
  }
  if (logFlushTimer === null) {
    logFlushTimer = setTimeout(flushLogs, LOG_FLUSH_MS);
  }
}

export function clearLogs() {
  logBuf = [];
  if (logFlushTimer !== null) {
    clearTimeout(logFlushTimer);
    logFlushTimer = null;
  }
  store.logs.length = 0;
}

// ---- Actions --------------------------------------------------------------

export async function refreshStatus() {
  try {
    const s = await api.kernelStatus();
    store.kernel.status = s.status;
    store.kernel.kernelDir = s.kernel_dir;
    store.kernel.revision = s.revision;
    store.kernel.dirty = s.dirty;
    store.kernel.valid = s.valid;
    store.kernel.token = s.token;
  } catch (e) {
    console.error("kernel_status failed", e);
  }
}

export async function startKernel(): Promise<string | null> {
  try {
    const p = await api.kernelStart();
    return p > 0 ? null : "启动失败";
  } catch (e) {
    return String(e);
  }
}

export async function stopKernel(): Promise<string | null> {
  try {
    await api.kernelStop();
    return null;
  } catch (e) {
    return String(e);
  }
}

export async function setDir(dir: string): Promise<string | null> {
  try {
    const s = await api.kernelSetDir(dir);
    store.kernel.status = s.status;
    store.kernel.kernelDir = s.kernel_dir;
    store.kernel.revision = s.revision;
    store.kernel.dirty = s.dirty;
    return null;
  } catch (e) {
    return String(e);
  }
}

export async function checkUpdate(): Promise<string | null> {
  store.checkingUpdate = true;
  store.lastUpdateError = null;
  try {
    store.updateInfo = await api.updateCheck();
    return null;
  } catch (e) {
    store.lastUpdateError = String(e);
    return String(e);
  } finally {
    store.checkingUpdate = false;
  }
}

export async function applyUpdate(): Promise<string | null> {
  store.updating = true;
  store.lastUpdateError = null;
  try {
    await api.updateApply();
    return null;
  } catch (e) {
    store.lastUpdateError = String(e);
    return String(e);
  } finally {
    store.updating = false;
  }
}

export async function installKernel(): Promise<string | null> {
  store.installing = true;
  store.lastUpdateError = null;
  try {
    await api.kernelInstall();
    // Kernel dir was set server-side — refresh UI state so the
    // welcome card switches to the "启动内核" branch.
    await refreshStatus();
    return null;
  } catch (e) {
    store.lastUpdateError = String(e);
    return String(e);
  } finally {
    store.installing = false;
  }
}

// ---- Event wiring ---------------------------------------------------------

// If the running event arrives without a token (the token parse can lag the
// port probe), poll kernel_status briefly until the token lands so the
// iframe URL switches to the authenticated one and the 401 page goes away.
let tokenPollTimer: ReturnType<typeof setTimeout> | null = null;
function pollForToken() {
  if (tokenPollTimer !== null) return;
  let tries = 0;
  const tick = async () => {
    tries++;
    await refreshStatus();
    if (store.kernel.token !== null || store.kernel.status.state !== "running" || tries >= 20) {
      tokenPollTimer = null;
      return;
    }
    tokenPollTimer = setTimeout(tick, 500);
  };
  tokenPollTimer = setTimeout(tick, 500);
}

let unlistenFns: UnlistenFn[] = [];
let wired = false;

export async function wireEvents() {
  if (wired) return;
  wired = true;
  unlistenFns.push(
    await listen("kernel-status", (e) => {
      const payload = e.payload as Record<string, unknown>;
      if (payload.state === "running") {
        store.kernel.status = { state: "running", port: Number(payload.port) };
        if (typeof payload.token === "string") {
          store.kernel.token = payload.token;
        } else if (store.kernel.token === null) {
          pollForToken();
        }
      } else if (payload.state === "error") {
        store.kernel.status = { state: "error", message: String(payload.message ?? "") };
      } else if (payload.state === "stopped") {
        store.kernel.status = { state: "stopped" };
      }
    }),
  );
  unlistenFns.push(
    await listen("kernel-log", (e) => {
      const payload = e.payload as { stream?: string; line?: string };
      appendLog(payload.stream === "err" ? "err" : "out", payload.line ?? "");
    }),
  );
  unlistenFns.push(
    await listen("update-status", (e) => {
      const payload = e.payload as Record<string, unknown>;
      if (payload.phase === "update_available" || payload.phase === "up_to_date") {
        store.updateInfo = {
          current: (payload.current as string) ?? null,
          latest: (payload.latest as string) ?? null,
          behind: Number(payload.behind ?? 0),
          update_available: payload.phase === "update_available",
          dirty: false,
          error: null,
        };
        if (payload.phase === "update_available" && store.updateNotified !== store.updateInfo.latest) {
          store.updateNotified = store.updateInfo.latest;
          void notifyUpdate(
            `内核有更新 ${store.updateInfo.current} → ${store.updateInfo.latest}`,
            `落后 ${store.updateInfo.behind} 个提交，可在「内核」中一键更新。`,
          );
        }
      } else if (payload.phase === "done") {
        store.lastUpdateError = null;
        refreshStatus();
      }
    }),
  );
}

export function disposeEvents() {
  for (const fn of unlistenFns) fn();
  unlistenFns = [];
  wired = false;
}
