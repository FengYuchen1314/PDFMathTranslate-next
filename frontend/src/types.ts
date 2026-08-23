export type JobStatus =
  | "queued"
  | "running"
  | "cancelling"
  | "succeeded"
  | "failed"
  | "cancelled";

export interface LanguageOption {
  code: string;
  name: string;
}

export type ProviderKind = "google" | "deepseek" | "openai_compatible";

/** Provider IDs are configured by the administrator and are not a closed set. */
export type ProviderId = string;

export type ThinkingMode = "default" | "enabled" | "disabled";

export interface ProviderModelOption {
  id: string;
  name: string;
  isDefault?: boolean;
  thinkingModes?: ThinkingMode[];
  reasoningEfforts?: string[];
  defaultReasoningEffort?: string | null;
}

export interface ProviderDescriptor {
  id: ProviderId;
  kind: ProviderKind;
  name: string;
  available: boolean;
  unavailableReason?: "not_configured" | "worker_unavailable" | string;
  models: ProviderModelOption[];
  defaultModelId?: string | null;
  /** Compatibility with the first DeepSeek public-config contract. */
  defaultModel?: string | null;
  thinkingModes?: ThinkingMode[];
  reasoningEfforts?: string[];
  defaultReasoningEffort?: string | null;
}

export interface AppConfig {
  /** Kept while older Rust deployments still return the Google-only marker. */
  provider?: "google";
  providers: ProviderDescriptor[];
  languages: LanguageOption[];
  maxUploadBytes: number;
  /** Present only when this browser has an authenticated administrator session. */
  csrfToken?: string | null;
}

export interface JobFiles {
  mono?: boolean | string | null;
  dual?: boolean | string | null;
  glossary?: boolean | string | null;
}

export interface TranslationJob {
  id: string;
  status: JobStatus;
  revision?: number;
  progress?: number;
  stage?: string | null;
  message?: string | null;
  error?: string | null;
  files?: JobFiles | null;
  options?: {
    providerId?: string | null;
    modelId?: string | null;
  };
  /** Compatibility with pre-options API responses. */
  providerId?: string | null;
  providerName?: string | null;
  modelId?: string | null;
  modelName?: string | null;
}

export interface JobDraft {
  providerId: ProviderId;
  modelId: string;
  thinkingMode: ThinkingMode;
  reasoningEffort: string;
  sourceLang: string;
  targetLang: string;
  pages: string;
  outputMono: boolean;
  outputDual: boolean;
}

/** PersistedDraft accepts the previous DeepSeek-specific shape for migration. */
export interface PersistedDraft extends Partial<JobDraft> {
  fileName?: string;
  provider?: string;
  deepseekModel?: string;
  deepseekThinkingMode?: "" | "enabled" | "disabled";
  deepseekReasoningEffort?: string;
}

export interface AdminSession {
  setupRequired: boolean;
  setupTokenRequired: boolean;
  authenticated: boolean;
  username?: string | null;
  csrfToken?: string | null;
}

export interface AdminModel {
  id: string;
  name: string;
  isDefault: boolean;
}

export interface AdminProvider {
  id: string;
  kind: ProviderKind;
  name: string;
  baseUrl: string;
  enabled: boolean;
  hasApiKey: boolean;
  revision: number;
  models: AdminModel[];
}

export interface AdminProviderWrite {
  kind: "deepseek" | "openai_compatible";
  name: string;
  baseUrl: string;
  enabled: boolean;
  apiKey?: string;
  clearApiKey: boolean;
  expectedRevision?: number;
  models: AdminModel[];
}

export const ACTIVE_JOB_STATES: ReadonlySet<JobStatus> = new Set([
  "queued",
  "running",
  "cancelling",
]);

export const TERMINAL_JOB_STATES: ReadonlySet<JobStatus> = new Set([
  "succeeded",
  "failed",
  "cancelled",
]);
