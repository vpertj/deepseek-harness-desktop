import { invoke } from "@tauri-apps/api/core";

export type KernelState =
  | "stopped"
  | "starting"
  | "running"
  | "error";

export interface KernelStatus {
  status:
    | { state: "stopped" }
    | { state: "starting" }
    | { state: "running"; port: number }
    | { state: "error"; message: string };
  kernelDir: string | null;
  revision: string | null;
  dirty: boolean;
}

export interface UpdateInfo {
  current: string | null;
  latest: string | null;
  behind: number;
  updateAvailable: boolean;
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
