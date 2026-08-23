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

export interface AppConfig {
  provider: "google";
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
