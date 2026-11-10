export const SESSION_CHANGE_EVENT = "image-annotation:session-change";

export type SessionChangeDetail = {
  authenticated: boolean;
  userId?: string | null;
  serverUrl?: string | null;
};

/**
 * Notify the shells that account-scoped caches must be reloaded. Keeping this
 * browser event in the shared layer lets desktop and native mobile auth flows
 * use the same invalidation boundary without sharing their view state.
 */
export function dispatchSessionChange(detail: SessionChangeDetail): void {
  if (typeof window === "undefined") return;
  window.dispatchEvent(new CustomEvent<SessionChangeDetail>(SESSION_CHANGE_EVENT, { detail }));
}
