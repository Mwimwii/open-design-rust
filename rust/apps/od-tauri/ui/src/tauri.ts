import { invoke } from "@tauri-apps/api/core";

export interface DaemonInfo {
  url: string;
  version: string;
  data_dir: string;
  web_dist: string | null;
}

export interface DaemonHealth {
  ok: boolean;
  version: string;
}

declare global {
  interface Window {
    __TAURI_INTERNALS__?: unknown;
  }
}

export function isShellAvailable(): boolean {
  return typeof window !== "undefined" && window.__TAURI_INTERNALS__ !== undefined;
}

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

async function command<T>(name: string, args?: Record<string, unknown>): Promise<T> {
  if (!isShellAvailable()) {
    throw new Error(`${name} is unavailable outside the Tauri shell (plain browser dev)`);
  }
  try {
    return await invoke<T>(name, args);
  } catch (error) {
    throw new Error(`${name} failed: ${errorText(error)}`);
  }
}

export function getAppVersion(): Promise<string> {
  return command<string>("app_version");
}

export function getDaemonInfo(): Promise<DaemonInfo> {
  return command<DaemonInfo>("daemon_info");
}

export function getDaemonHealth(): Promise<DaemonHealth> {
  return command<DaemonHealth>("daemon_health");
}

export function pickWorkspaceFolder(): Promise<string | null> {
  return command<string | null>("pick_workspace_folder");
}

export function openExternal(url: string): Promise<void> {
  return command<void>("open_external", { url });
}
