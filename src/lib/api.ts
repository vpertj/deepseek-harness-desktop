import { invoke } from "@tauri-apps/api/core";

export type KernelState =
  | "stopped"
  | "starting"
  | "running"
  | "error";

// NOTE: tauri command RETURN values keep Rust field names (snake_case).
// Only invoke ARGUMENTS are auto-converted to camelCase.
export interface KernelStatus {
  status:
    | { state: "stopped" }
    | { state: "starting" }
    | { state: "running"; port: number }
    | { state: "error"; message: string };
  kernel_dir: string | null;
  revision: string | null;
  dirty: boolean;
}

export interface UpdateInfo {
  current: string | null;
  latest: string | null;
  behind: number;
  update_available: boolean;
  dirty: boolean;
  error: string | null;
}

export function kernelStatus(): Promise<KernelStatus> {
  return invoke("kernel_status");
}

export function kernelSetDir(dir: string): Promise<KernelStatus> {
  return invoke("kernel_set_dir", { dir });
}

export function kernelStart(): Promise<number> {
  return invoke("kernel_start");
}

export function kernelStop(): Promise<void> {
  return invoke("kernel_stop");
}

export function updateCheck(): Promise<UpdateInfo> {
  return invoke("update_check");
}

export function updateApply(): Promise<void> {
  return invoke("update_apply");
}

export function kernelInstall(): Promise<void> {
  return invoke("kernel_install");
}

// ---- Theme (follows the kernel UI's appearance setting) -------------------

export interface ThemeDto {
  preference: string | null;
  source: string;
}

export function getTheme(): Promise<ThemeDto> {
  return invoke("get_theme");
}

// ---- Environment (node / pnpm detection & auto-install) -------------------

export interface ToolStatusDto {
  present: boolean;
  version: string | null;
  ok: boolean;
}

export interface EnvStatusDto {
  node: ToolStatusDto;
  pnpm: ToolStatusDto;
  node_path: string | null;
  pnpm_path: string | null;
  mise: boolean;
  brew: boolean;
  corepack: boolean;
  ready: boolean;
}

export function checkEnv(): Promise<EnvStatusDto> {
  return invoke("check_env");
}

export function installEnv(): Promise<EnvStatusDto> {
  return invoke("install_env");
}

// ---- App settings ----------------------------------------------------------

export interface SettingsDto {
  kernel_dir: string | null;
  auto_start: boolean;
  persist_logs: boolean;
}

export function getSettings(): Promise<SettingsDto> {
  return invoke("get_settings");
}

export function setAutoStart(autoStart: boolean): Promise<SettingsDto> {
  return invoke("set_auto_start", { autoStart });
}

export function setPersistLogs(persistLogs: boolean): Promise<SettingsDto> {
  return invoke("set_persist_logs", { persistLogs });
}
