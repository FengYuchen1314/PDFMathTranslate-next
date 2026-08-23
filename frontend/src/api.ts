import type {
  AdminProvider,
  AdminProviderWrite,
  AdminSession,
  AppConfig,
  JobDraft,
  JobStatus,
  TranslationJob,
} from "./types";

const API_BASE = (import.meta.env.VITE_API_BASE_URL as string | undefined)?.replace(
  /\/$/,
  "",
) ?? "";
const CONTROL_REQUEST_TIMEOUT_MS = 8_000;
const JOB_ADMISSION_RETRY_WINDOW_MS = 240_000;
const JOB_ADMISSION_RETRY_DELAY_MS = 1_000;
const TRANSIENT_JOB_ADMISSION_STATUSES = new Set([409, 429, 503]);

export class ApiRequestError extends Error {
  readonly status: number;

  constructor(message: string, status: number) {
    super(message);
    this.name = "ApiRequestError";
    this.status = status;
  }
}

function apiUrl(path: string): string {
  return `${API_BASE}${path}`;
}

async function fetchWithTimeout(
  input: string,
  init: RequestInit,
  timeoutMs: number,
): Promise<Response> {
  if (timeoutMs <= 0) {
    return fetch(input, init);
  }
  const controller = new AbortController();
  const timer = window.setTimeout(() => controller.abort(), timeoutMs);
  try {
    return await fetch(input, { ...init, signal: controller.signal });
  } finally {
    window.clearTimeout(timer);
  }
}

async function readError(response: Response): Promise<string> {
  try {
    const body = (await response.json()) as {
      detail?: string;
      error?: string;
      message?: string;
    };
    return body.detail ?? body.error ?? body.message ?? `请求失败 (${response.status})`;
  } catch {
    return `请求失败 (${response.status})`;
  }
}

async function requestJson<T>(
  path: string,
  init?: RequestInit,
  timeoutMs = CONTROL_REQUEST_TIMEOUT_MS,
): Promise<T> {
  const headers = new Headers(init?.headers);
  headers.set("X-PDF2ZH-Origin", window.location.origin);
  const response = await fetchWithTimeout(
    apiUrl(path),
    {
      credentials: "same-origin",
      ...init,
      headers,
    },
    timeoutMs,
  );
  if (!response.ok) {
    throw new ApiRequestError(await readError(response), response.status);
  }
  if (response.status === 204) {
    return undefined as T;
  }
  const text = await response.text();
  return (text ? JSON.parse(text) : undefined) as T;
}

function jsonHeaders(csrfToken?: string | null): HeadersInit {
  const headers: Record<string, string> = { "Content-Type": "application/json" };
  if (csrfToken) {
    headers["X-CSRF-Token"] = csrfToken;
  }
  return headers;
}

function waitForJobAdmissionRetry(delayMs: number): Promise<void> {
  return new Promise((resolve) => window.setTimeout(resolve, delayMs));
}

export async function checkHealth(): Promise<boolean> {
  const response = await fetchWithTimeout(
    apiUrl("/api/health"),
    { credentials: "same-origin", cache: "no-store" },
    CONTROL_REQUEST_TIMEOUT_MS,
  );
  return response.ok;
}

export function getConfig(): Promise<AppConfig> {
  return requestJson<AppConfig>("/api/config", { cache: "no-store" });
}

export function getJob(jobId: string): Promise<TranslationJob> {
  return requestJson<TranslationJob>(`/api/jobs/${encodeURIComponent(jobId)}`, {
    cache: "no-store",
  });
}

export async function createJob(
  file: File,
  draft: JobDraft,
  jobId: string,
  csrfToken?: string | null,
): Promise<{ id: string; status: JobStatus }> {
  const form = new FormData();
  form.append("file", file, file.name);
  form.append("source_lang", draft.sourceLang);
  form.append("target_lang", draft.targetLang);
  form.append("pages", draft.pages.trim());
  form.append("output_mono", String(draft.outputMono));
  form.append("output_dual", String(draft.outputDual));
  form.append("provider_id", draft.providerId);
  if (draft.modelId) {
    form.append("model_id", draft.modelId);
  }
  if (draft.thinkingMode !== "default") {
    form.append("thinking_mode", draft.thinkingMode);
  }
  if (draft.thinkingMode === "enabled" && draft.reasoningEffort) {
    form.append("reasoning_effort", draft.reasoningEffort);
  }
  const request: RequestInit = {
    method: "POST",
    headers: {
      "x-job-id": jobId,
      ...(csrfToken ? { "X-CSRF-Token": csrfToken } : {}),
    },
    body: form,
  };
  const retryDeadline = Date.now() + JOB_ADMISSION_RETRY_WINDOW_MS;
  while (true) {
    try {
      return await requestJson<{ id: string; status: JobStatus }>(
        "/api/jobs",
        request,
        0,
      );
    } catch (error) {
      if (
        !(error instanceof ApiRequestError) ||
        !TRANSIENT_JOB_ADMISSION_STATUSES.has(error.status)
      ) {
        throw error;
      }
      const remainingMs = retryDeadline - Date.now();
      if (remainingMs <= 0) {
        throw error;
      }
      await waitForJobAdmissionRetry(
        Math.min(JOB_ADMISSION_RETRY_DELAY_MS, remainingMs),
      );
    }
  }
}

export function createClientJobId(): string {
  if (typeof globalThis.crypto?.randomUUID === "function") {
    return globalThis.crypto.randomUUID();
  }
  const bytes = new Uint8Array(16);
  if (typeof globalThis.crypto?.getRandomValues === "function") {
    globalThis.crypto.getRandomValues(bytes);
  } else {
    for (let index = 0; index < bytes.length; index += 1) {
      bytes[index] = Math.floor(Math.random() * 256);
    }
  }
  bytes[6] = (bytes[6] & 0x0f) | 0x40;
  bytes[8] = (bytes[8] & 0x3f) | 0x80;
  const hex = Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0"));
  return `${hex.slice(0, 4).join("")}-${hex.slice(4, 6).join("")}-${hex
    .slice(6, 8)
    .join("")}-${hex.slice(8, 10).join("")}-${hex.slice(10).join("")}`;
}

export function cancelJob(jobId: string): Promise<TranslationJob | undefined> {
  return requestJson<TranslationJob | undefined>(
    `/api/jobs/${encodeURIComponent(jobId)}/cancel`,
    { method: "POST" },
  );
}

export async function heartbeatJob(jobId: string): Promise<void> {
  await requestJson<void>(`/api/jobs/${encodeURIComponent(jobId)}/heartbeat`, {
    method: "POST",
  });
}

export async function releaseJob(jobId: string): Promise<void> {
  await requestJson<void>(`/api/jobs/${encodeURIComponent(jobId)}/release`, {
    method: "POST",
  });
}

export async function deleteJob(jobId: string): Promise<void> {
  await requestJson<void>(`/api/jobs/${encodeURIComponent(jobId)}`, {
    method: "DELETE",
  });
}

export function eventStreamUrl(jobId: string): string {
  return apiUrl(`/api/jobs/${encodeURIComponent(jobId)}/events`);
}

export function jobFileUrl(
  jobId: string,
  kind: "mono" | "dual" | "glossary",
): string {
  return apiUrl(`/api/jobs/${encodeURIComponent(jobId)}/files/${kind}`);
}

export function sendJobBeacon(
  jobId: string,
  action: "cancel" | "heartbeat" | "release",
): boolean {
  const url = apiUrl(`/api/jobs/${encodeURIComponent(jobId)}/${action}`);
  if (typeof navigator.sendBeacon === "function" && navigator.sendBeacon(url)) {
    return true;
  }
  void fetch(url, {
    method: "POST",
    credentials: "same-origin",
    keepalive: true,
  }).catch(() => {
    // The backend lease timeout is the final fallback if the page is already gone.
  });
  return false;
}

export function getAdminSession(): Promise<AdminSession> {
  return requestJson<AdminSession>("/api/admin/session", { cache: "no-store" });
}

export function setupAdmin(input: {
  setupToken?: string;
  username: string;
  password: string;
}): Promise<AdminSession> {
  return requestJson<AdminSession>("/api/admin/setup", {
    method: "POST",
    headers: jsonHeaders(),
    body: JSON.stringify(input),
  });
}

export function loginAdmin(input: {
  username: string;
  password: string;
}): Promise<AdminSession> {
  return requestJson<AdminSession>("/api/admin/login", {
    method: "POST",
    headers: jsonHeaders(),
    body: JSON.stringify(input),
  });
}

export async function logoutAdmin(csrfToken: string): Promise<void> {
  await requestJson<void>("/api/admin/logout", {
    method: "POST",
    headers: jsonHeaders(csrfToken),
  });
}

export async function getAdminProviders(): Promise<AdminProvider[]> {
  const response = await requestJson<{ providers: AdminProvider[] }>(
    "/api/admin/providers",
    { cache: "no-store" },
  );
  return response.providers;
}

export function putDeepSeekProvider(
  input: AdminProviderWrite,
  csrfToken: string,
): Promise<AdminProvider> {
  return requestJson<AdminProvider>("/api/admin/providers/deepseek", {
    method: "PUT",
    headers: jsonHeaders(csrfToken),
    body: JSON.stringify(input),
  });
}

export function createAdminProvider(
  input: AdminProviderWrite,
  csrfToken: string,
): Promise<AdminProvider> {
  return requestJson<AdminProvider>("/api/admin/providers", {
    method: "POST",
    headers: jsonHeaders(csrfToken),
    body: JSON.stringify(input),
  });
}

export function updateAdminProvider(
  providerId: string,
  input: AdminProviderWrite,
  csrfToken: string,
): Promise<AdminProvider> {
  return requestJson<AdminProvider>(
    `/api/admin/providers/${encodeURIComponent(providerId)}`,
    {
      method: "PUT",
      headers: jsonHeaders(csrfToken),
      body: JSON.stringify(input),
    },
  );
}

export async function deleteAdminProvider(
  providerId: string,
  expectedRevision: number,
  csrfToken: string,
): Promise<void> {
  const query = new URLSearchParams({
    expectedRevision: String(expectedRevision),
  });
  await requestJson<void>(
    `/api/admin/providers/${encodeURIComponent(providerId)}?${query.toString()}`,
    {
      method: "DELETE",
      headers: jsonHeaders(csrfToken),
    },
  );
}
