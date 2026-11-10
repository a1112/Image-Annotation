import { useEffect } from "react";
import { flushOfflineQueue, offlineStatus } from "../../api/mobile";
import { isTauriRuntime } from "../../platform/runtime";
import type { OfflineStatus } from "../../types/mobile";

type SyncTarget = string | undefined;

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function emptyOfflineStatus(lastError: string): OfflineStatus {
  return {
    online: false,
    queuedOperations: 0,
    conflictCount: 0,
    cachedAssets: 0,
    lastSyncAt: null,
    lastError,
  };
}

/** Scheduling only: persistence, retries and conflict decisions stay in Rust. */
export function useForegroundSync(projectId?: string): void {
  useEffect(() => {
    if (!isTauriRuntime()) return;
    let disposed = false;
    let running = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    // The native sync engine owns reachability.  WebView navigator.onLine is
    // only a browser hint and can remain false while native networking works.
    // The boolean value records whether a target was scheduled by a lifecycle
    // or local-mutation event that must bypass a stale hidden WebView state.
    const pending = new Map<SyncTarget, boolean>();

    function dispatchStatus(target: SyncTarget, status: OfflineStatus) {
      if (!disposed) {
        window.dispatchEvent(new CustomEvent("image-annotation:sync-status", {
          detail: { projectId: target, status },
        }));
      }
    }

    async function drain() {
      if (disposed || running) return;
      running = true;
      try {
        while (!disposed && pending.size) {
          let next: [SyncTarget, boolean] | undefined;
          if (document.visibilityState === "hidden") {
            // Periodic work remains foreground-only, but a local mutation or
            // resume signal must still get one native attempt.  This avoids
            // losing the wake-up when a mobile WebView reports stale state.
            next = [...pending.entries()].find(([, force]) => force);
            if (!next) break;
          } else {
            next = pending.entries().next().value as [SyncTarget, boolean] | undefined;
            if (!next) break;
          }
          const [target] = next;
          pending.delete(target);
          try {
            const status = await flushOfflineQueue(target);
            dispatchStatus(target, status);
          } catch (error) {
            // Keep the UI's queue/error state current even when the native
            // command itself fails. The durable outbox remains in Rust and a
            // later lifecycle/interval trigger will retry it.
            const message = errorMessage(error);
            let status: OfflineStatus;
            try {
              status = await offlineStatus(target);
            } catch {
              status = emptyOfflineStatus(message);
            }
            dispatchStatus(target, { ...status, online: false, lastError: message });
          }
        }
      } finally {
        running = false;
      }
    }

    function schedule(target: SyncTarget = projectId, force = false) {
      pending.set(target, (pending.get(target) ?? false) || force);
      if (timer !== undefined) clearTimeout(timer);
      timer = setTimeout(() => { void drain(); }, 400);
    }

    // A visibility/focus/pageshow transition is a recovery hint. Local
    // mutations are forced because the native queue is authoritative even if
    // the WebView lifecycle flags are stale.
    const periodic = () => schedule();
    const recover = () => schedule(projectId, true);
    const mutation = (event: Event) => schedule(
      (event as CustomEvent<{ projectId?: string }>).detail?.projectId,
      true,
    );
    const interval = setInterval(periodic, 15_000);
    document.addEventListener("visibilitychange", periodic);
    window.addEventListener("online", recover);
    window.addEventListener("focus", recover);
    window.addEventListener("pageshow", recover);
    window.addEventListener("image-annotation:local-change", mutation);
    schedule();
    return () => {
      disposed = true;
      if (timer !== undefined) clearTimeout(timer);
      clearInterval(interval);
      document.removeEventListener("visibilitychange", periodic);
      window.removeEventListener("online", recover);
      window.removeEventListener("focus", recover);
      window.removeEventListener("pageshow", recover);
      window.removeEventListener("image-annotation:local-change", mutation);
    };
  }, [projectId]);
}
