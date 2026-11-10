import { afterEach, describe, expect, it } from "vitest";
import { isIosRuntime, isMobileRuntime } from "./runtime";

function setTauriRuntime(platform = "macos") {
  Object.defineProperty(window, "__TAURI_INTERNALS__", {
    configurable: true,
    value: { metadata: { platform } },
  });
}

function setNavigator({ userAgent, platform, maxTouchPoints }: {
  userAgent: string;
  platform: string;
  maxTouchPoints: number;
}) {
  Object.defineProperty(window.navigator, "userAgent", { configurable: true, value: userAgent });
  Object.defineProperty(window.navigator, "platform", { configurable: true, value: platform });
  Object.defineProperty(window.navigator, "maxTouchPoints", { configurable: true, value: maxTouchPoints });
}

afterEach(() => {
  delete (window as Window & { __TAURI_INTERNALS__?: unknown }).__TAURI_INTERNALS__;
  setNavigator({ userAgent: "Mozilla/5.0", platform: "Linux x86_64", maxTouchPoints: 0 });
});

describe("isMobileRuntime", () => {
  it("recognizes an iPad using the desktop Macintosh user agent when touch hardware is present", () => {
    setTauriRuntime();
    setNavigator({
      userAgent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 Version/18.0 Mobile/15E148 Safari/604.1",
      platform: "MacIntel",
      maxTouchPoints: 5,
    });

    expect(isMobileRuntime()).toBe(true);
  });

  it("does not classify a normal Mac desktop as mobile", () => {
    setTauriRuntime();
    setNavigator({
      userAgent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_6) AppleWebKit/605.1.15 Safari/605.1.15",
      platform: "MacIntel",
      maxTouchPoints: 0,
    });

    expect(isMobileRuntime()).toBe(false);
  });
});

describe("isIosRuntime", () => {
  it("recognizes the iOS Tauri metadata used by WKWebView", () => {
    setTauriRuntime("ios");
    setNavigator({ userAgent: "Mozilla/5.0", platform: "MacIntel", maxTouchPoints: 5 });

    expect(isIosRuntime()).toBe(true);
  });

  it("does not classify a desktop Tauri window as iOS", () => {
    setTauriRuntime("macos");
    setNavigator({
      userAgent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_6)",
      platform: "MacIntel",
      maxTouchPoints: 0,
    });

    expect(isIosRuntime()).toBe(false);
  });
});
