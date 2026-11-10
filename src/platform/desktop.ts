import { invoke } from "@tauri-apps/api/core";
import type { MouseEvent } from "react";

export async function runDesktopCommand(command: string) {
  try { await invoke(command); } catch { /* Browser preview and tests have no Tauri command. */ }
}

export function beginDesktopWindowDrag(event: MouseEvent<HTMLElement>) {
  if (event.button !== 0) return;
  const target = event.target instanceof Element ? event.target : null;
  if (target?.closest("[data-no-drag], button, input, textarea, select, a, label")) return;
  void runDesktopCommand("start_drag_window");
}
