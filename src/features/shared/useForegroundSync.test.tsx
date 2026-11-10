import { act, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { flushOfflineQueue, offlineStatus } from "../../api/mobile";
import { isTauriRuntime } from "../../platform/runtime";
import type { OfflineStatus } from "../../types/mobile";
import { useForegroundSync } from "./useForegroundSync";

vi.mock("../../api/mobile", () => ({
  flushOfflineQueue: vi.fn(),
  offlineStatus: vi.fn(),
}));

vi.mock("../../platform/runtime", () => ({
  isTauriRuntime: vi.fn(),
}));

const status: OfflineStatus = {
  online: true,
  queuedOperations: 0,
  conflictCount: 0,
  cachedAssets: 1,
  lastSyncAt: "2026-10-25T09:00:00Z",
  lastError: null,
};

function setVisibility(value: DocumentVisibilityState) {
  Object.defineProperty(document, "visibilityState", {
    configurable: true,
    value,
  });
}

function setOnline(value: boolean) {
  Object.defineProperty(window.navigator, "onLine", {
    configurable: true,
    value,
  });
}

async function advanceInitialSchedule() {
  await act(async () => {
    await vi.advanceTimersByTimeAsync(400);
  });
}

beforeEach(() => {
  vi.useFakeTimers();
  vi.clearAllMocks();
  vi.mocked(isTauriRuntime).mockReturnValue(true);
  vi.mocked(flushOfflineQueue).mockResolvedValue(status);
  vi.mocked(offlineStatus).mockResolvedValue(status);
  setVisibility("visible");
  setOnline(true);
});

afterEach(() => {
  vi.useRealTimers();
  setVisibility("visible");
  setOnline(true);
});

describe("useForegroundSync", () => {
  it("attempts native sync when navigator reports offline", async () => {
    setOnline(false);
    renderHook(() => useForegroundSync("project-1"));

    await advanceInitialSchedule();

    expect(flushOfflineQueue).toHaveBeenCalledTimes(1);
    expect(flushOfflineQueue).toHaveBeenCalledWith("project-1");
  });

  it("forces local mutations through a hidden WebView and publishes failures", async () => {
    setVisibility("hidden");
    vi.mocked(flushOfflineQueue).mockRejectedValue(new Error("native network unavailable"));
    vi.mocked(offlineStatus).mockResolvedValue({
      ...status,
      online: false,
      queuedOperations: 2,
    });
    const events: OfflineStatus[] = [];
    const receive = (event: Event) => {
      events.push((event as CustomEvent<{ status: OfflineStatus }>).detail.status);
    };
    window.addEventListener("image-annotation:sync-status", receive);
    renderHook(() => useForegroundSync("project-1"));

    await advanceInitialSchedule();
    expect(flushOfflineQueue).not.toHaveBeenCalled();

    await act(async () => {
      window.dispatchEvent(new CustomEvent("image-annotation:local-change", {
        detail: { projectId: "project-1" },
      }));
      await vi.advanceTimersByTimeAsync(400);
    });

    expect(flushOfflineQueue).toHaveBeenCalledWith("project-1");
    expect(offlineStatus).toHaveBeenCalledWith("project-1");
    expect(events[events.length - 1]).toMatchObject({
      online: false,
      queuedOperations: 2,
      lastError: "native network unavailable",
    });
    window.removeEventListener("image-annotation:sync-status", receive);
  });

  it("cancels timers and listeners on unmount", async () => {
    const { unmount } = renderHook(() => useForegroundSync("project-1"));
    unmount();

    await act(async () => {
      window.dispatchEvent(new CustomEvent("image-annotation:local-change", {
        detail: { projectId: "project-1" },
      }));
      window.dispatchEvent(new Event("focus"));
      await vi.advanceTimersByTimeAsync(15_400);
    });

    expect(flushOfflineQueue).not.toHaveBeenCalled();
  });

  it("does not run concurrent drains when lifecycle signals overlap", async () => {
    let active = 0;
    let maxActive = 0;
    const resolvers: Array<(value: OfflineStatus) => void> = [];
    vi.mocked(flushOfflineQueue).mockImplementation(() => new Promise((resolve) => {
      active += 1;
      maxActive = Math.max(maxActive, active);
      resolvers.push((value) => {
        active -= 1;
        resolve(value);
      });
    }));
    renderHook(() => useForegroundSync("project-1"));

    await advanceInitialSchedule();
    expect(flushOfflineQueue).toHaveBeenCalledTimes(1);

    await act(async () => {
      window.dispatchEvent(new Event("focus"));
      window.dispatchEvent(new Event("pageshow"));
      window.dispatchEvent(new CustomEvent("image-annotation:local-change", {
        detail: { projectId: "project-1" },
      }));
      await vi.advanceTimersByTimeAsync(400);
    });

    expect(flushOfflineQueue).toHaveBeenCalledTimes(1);
    expect(maxActive).toBe(1);

    await act(async () => {
      resolvers.shift()?.(status);
      await Promise.resolve();
    });
    expect(maxActive).toBe(1);
    if (resolvers.length) {
      await act(async () => {
        resolvers.shift()?.(status);
        await Promise.resolve();
      });
    }
    expect(active).toBe(0);
  });
});
