/** Runtime predicates kept in one place so feature modules do not infer
 * platform from the presence of icons or viewport size. */
export function isTauriRuntime(): boolean {
  return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
}

/**
 * Identifies the iOS WebView specifically so native-only file actions can
 * explain where an export was saved instead of navigating to a private path.
 * Keep this separate from isMobileRuntime(): iPadOS may advertise a desktop
 * Macintosh user agent while Tauri metadata still reports iOS.
 */
export function isIosRuntime(): boolean {
  if (typeof window === "undefined") return false;
  const platform = (window as Window & {
    __TAURI_INTERNALS__?: { metadata?: { platform?: string } };
  }).__TAURI_INTERNALS__?.metadata?.platform;
  if (/ios/i.test(platform ?? "")) return true;
  const userAgent = typeof navigator === "undefined" ? "" : navigator.userAgent;
  return /iPhone|iPad|iPod/i.test(userAgent);
}

export function isMobileRuntime(): boolean {
  if (typeof window === "undefined") return false;

  const injected = (window as Window & { __IMAGE_ANNOTATION_MOBILE__?: boolean })
    .__IMAGE_ANNOTATION_MOBILE__;
  if (injected === true) return true;

  if (!isTauriRuntime()) return false;
  const platform = (window as Window & {
    __TAURI_INTERNALS__?: { metadata?: { platform?: string } };
  }).__TAURI_INTERNALS__?.metadata?.platform;
  if (/android|ios/i.test(platform ?? "")) return true;
  const userAgent = typeof navigator === "undefined" ? "" : navigator.userAgent;
  if (/Android|iPhone|iPad|iPod/i.test(userAgent)) return true;

  // WKWebView on recent iPads can expose the desktop Safari/Macintosh user
  // agent.  Tauri's metadata is not guaranteed to identify that case, so use
  // the combination of Apple's Mac platform token and real touch hardware.
  // A normal Mac has no touch points, which keeps desktop Tauri windows out.
  const maxTouchPoints = typeof navigator === "undefined" ? 0 : navigator.maxTouchPoints;
  const navigatorPlatform = typeof navigator === "undefined" ? "" : navigator.platform;
  return /Macintosh|MacIntel/i.test(`${userAgent} ${navigatorPlatform}`) && maxTouchPoints > 1;
}
