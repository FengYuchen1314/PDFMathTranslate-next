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

export type ProviderId = "google" | "deepseek";

export type DeepSeekModel = "deepseek-v4-flash" | "deepseek-v4-pro";

export type DeepSeekThinkingMode = "" | "enabled" | "disabled";

export type DeepSeekReasoningEffort = "low" | "high" | "max";

export interface ProviderModelOption {
  id: string;
  name: string;
}

export interface ProviderDescriptor {
  id: ProviderId;
  name: string;
  available: boolean;
  unavailableReason?: "not_configured" | "worker_unavailable";
  models?: ProviderModelOption[];
  defaultModel?: string | null;
  thinkingModes?: DeepSeekThinkingMode[];
  reasoningEfforts?: DeepSeekReasoningEffort[];
  defaultReasoningEffort?: DeepSeekReasoningEffort | null;
}

export interface AppConfig {
  /** Kept for compatibility with the original Google-only configuration. */
  provider: "google";
  providers?: ProviderDescriptor[];
  languages: LanguageOption[];
  maxUploadBytes: number;
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
}

export interface JobDraft {
  provider: ProviderId;
  deepseekModel: DeepSeekModel;
  deepseekThinkingMode: DeepSeekThinkingMode;
  deepseekReasoningEffort: DeepSeekReasoningEffort;
  sourceLang: string;
  targetLang: string;
  pages: string;
  outputMono: boolean;
  outputDual: boolean;
}

export interface PersistedDraft extends JobDraft {
  fileName?: string;
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
