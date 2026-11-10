import "@testing-library/jest-dom/vitest";
import { cleanup } from "@testing-library/react";
import { afterEach } from "vitest";

function createMemoryStorage(): Storage {
  const values = new Map<string, string>();
  return {
    get length() {
      return values.size;
    },
    clear() {
      values.clear();
    },
    getItem(key: string) {
      return values.get(String(key)) ?? null;
    },
    key(index: number) {
      return [...values.keys()][index] ?? null;
    },
    removeItem(key: string) {
      values.delete(String(key));
    },
    setItem(key: string, value: string) {
      values.set(String(key), String(value));
    },
  };
}

function ensureTestStorage(): Storage {
  let storage: Storage | undefined;
  try {
    storage = window.localStorage;
  } catch {
    // jsdom can throw for an opaque origin; the in-memory fallback below is
    // sufficient for component tests.
  }
  if (!storage) {
    storage = createMemoryStorage();
    Object.defineProperty(window, "localStorage", {
      configurable: true,
      value: storage,
    });
  }
  // Node 26 exposes an experimental global WebStorage object which can mask
  // jsdom's window storage. Bind both names to the same deterministic store.
  try {
    Object.defineProperty(globalThis, "localStorage", {
      configurable: true,
      value: storage,
    });
  } catch {
    // A host may make the global non-configurable; window.localStorage still
    // provides the storage used by the application.
  }
  return storage;
}

const testStorage = ensureTestStorage();

afterEach(() => {
  cleanup();
  testStorage.clear();
  window.sessionStorage.clear();
});
