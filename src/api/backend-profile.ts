export type RemoteRole = "reader" | "editor" | "admin";

export type LocalBackendProfile = {
  mode: "local";
};

export type RemoteBackendProfile = {
  mode: "remote";
  baseUrl: string;
  apiBaseUrl: string;
  token: string;
  role: RemoteRole;
};

export type BackendProfile = LocalBackendProfile | RemoteBackendProfile;

type ApiEnvelope<T> = {
  ok?: boolean;
  data?: T;
  error?: { code?: string; message?: string };
  requestId?: string;
};

let activeProfile: BackendProfile = { mode: "local" };

export function getBackendProfile(): BackendProfile {
  return activeProfile;
}

export async function connectRemoteBackend(
  rawBaseUrl: string,
  rawToken: string,
): Promise<RemoteBackendProfile> {
  const { baseUrl, apiBaseUrl } = normalizeRemoteUrl(rawBaseUrl);
  const token = rawToken.trim();
  if (!token) {
    throw new Error("远程访问令牌不能为空");
  }

  const response = await fetch(`${apiBaseUrl}/session`, {
    method: "GET",
    headers: { Authorization: `Bearer ${token}` },
  });
  const envelope = (await response.json()) as ApiEnvelope<{ role: RemoteRole }>;
  const role = envelope.data?.role;
  if (!response.ok || envelope.ok === false || !isRemoteRole(role)) {
    throw new Error(envelope.error?.message ?? `远程服务连接失败 (HTTP ${response.status})`);
  }

  const profile: RemoteBackendProfile = {
    mode: "remote",
    baseUrl,
    apiBaseUrl,
    token,
    role,
  };
  activeProfile = profile;
  return profile;
}

export function disconnectRemoteBackend(): void {
  activeProfile = { mode: "local" };
}

function normalizeRemoteUrl(rawUrl: string): { baseUrl: string; apiBaseUrl: string } {
  const value = rawUrl.trim();
  if (!value) {
    throw new Error("远程服务地址不能为空");
  }
  let url: URL;
  try {
    url = new URL(value);
  } catch {
    throw new Error("远程服务地址无效");
  }
  if (!['http:', 'https:'].includes(url.protocol) || url.username || url.password) {
    throw new Error("远程服务地址必须使用 HTTP 或 HTTPS，且不能包含凭据");
  }
  if (url.search || url.hash) {
    throw new Error("远程服务地址不能包含查询参数或片段");
  }

  const pathname = url.pathname.replace(/\/+$/, "");
  const apiPath = pathname.endsWith("/api/v1") ? pathname : `${pathname}/api/v1`;
  const basePath = apiPath.slice(0, -"/api/v1".length);
  const origin = url.origin;
  return {
    baseUrl: `${origin}${basePath}`,
    apiBaseUrl: `${origin}${apiPath}`,
  };
}

function isRemoteRole(value: unknown): value is RemoteRole {
  return value === "reader" || value === "editor" || value === "admin";
}
