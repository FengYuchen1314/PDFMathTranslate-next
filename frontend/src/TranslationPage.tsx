import {
  ArrowRightLeft,
  Download,
  LoaderCircle,
  RefreshCw,
  RotateCcw,
  Square,
  X,
} from "lucide-react";
import {
  type ChangeEvent,
  type DragEvent,
  type FormEvent,
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
} from "react";

import { getConfig, jobFileUrl } from "./api";
import type {
  AppConfig,
  JobDraft,
  JobStatus,
  PersistedDraft,
  ProviderDescriptor,
  ProviderKind,
  ProviderModelOption,
  ThinkingMode,
  TranslationJob,
} from "./types";
import { ACTIVE_JOB_STATES } from "./types";
import { useTranslationJob } from "./useTranslationJob";

const DRAFT_STORAGE_KEY = "pdf2zh.translation-draft";
const DEFAULT_MAX_UPLOAD_BYTES = 100 * 1024 * 1024;
const DEFAULT_DRAFT: JobDraft = {
  providerId: "google",
  modelId: "",
  thinkingMode: "default",
  reasoningEffort: "high",
  sourceLang: "en",
  targetLang: "zh",
  pages: "",
  outputMono: true,
  outputDual: true,
};

const STATUS_COPY: Record<JobStatus, { label: string; title: string }> = {
  queued: { label: "排队中", title: "任务已进入队列" },
  running: { label: "翻译中", title: "正在处理文档" },
  cancelling: { label: "正在取消", title: "正在停止任务" },
  succeeded: { label: "已完成", title: "翻译完成" },
  failed: { label: "失败", title: "翻译未完成" },
  cancelled: { label: "已取消", title: "任务已停止" },
};

function providerKind(provider: Partial<ProviderDescriptor>): ProviderKind {
  if (provider.kind) return provider.kind;
  if (provider.id === "google") return "google";
  if (provider.id === "deepseek") return "deepseek";
  return "openai_compatible";
}

function normalizeProviders(config: AppConfig | null): ProviderDescriptor[] {
  return (config?.providers ?? []).map((provider) => ({
    ...provider,
    kind: providerKind(provider),
    models: provider.models ?? [],
  }));
}

function defaultModel(provider: ProviderDescriptor | undefined): string {
  if (!provider) return "";
  const requested = provider.defaultModelId ?? provider.defaultModel;
  if (requested && provider.models.some((model) => model.id === requested)) {
    return requested;
  }
  return provider.models.find((model) => model.isDefault)?.id ?? provider.models[0]?.id ?? "";
}

function isThinkingMode(value: unknown): value is ThinkingMode {
  return value === "default" || value === "enabled" || value === "disabled";
}

function loadDraft(): JobDraft {
  try {
    const stored = JSON.parse(
      window.localStorage.getItem(DRAFT_STORAGE_KEY) ?? "null",
    ) as PersistedDraft | null;
    if (!stored) return DEFAULT_DRAFT;
    const legacyThinking = stored.deepseekThinkingMode;
    const thinkingMode = isThinkingMode(stored.thinkingMode)
      ? stored.thinkingMode
      : legacyThinking === "enabled" || legacyThinking === "disabled"
        ? legacyThinking
        : "default";
    return {
      providerId:
        typeof stored.providerId === "string"
          ? stored.providerId
          : typeof stored.provider === "string"
            ? stored.provider
            : DEFAULT_DRAFT.providerId,
      modelId:
        typeof stored.modelId === "string"
          ? stored.modelId
          : typeof stored.deepseekModel === "string"
            ? stored.deepseekModel
            : "",
      thinkingMode,
      reasoningEffort:
        typeof stored.reasoningEffort === "string"
          ? stored.reasoningEffort
          : typeof stored.deepseekReasoningEffort === "string"
            ? stored.deepseekReasoningEffort
            : DEFAULT_DRAFT.reasoningEffort,
      sourceLang:
        typeof stored.sourceLang === "string"
          ? stored.sourceLang
          : DEFAULT_DRAFT.sourceLang,
      targetLang:
        typeof stored.targetLang === "string"
          ? stored.targetLang
          : DEFAULT_DRAFT.targetLang,
      pages: typeof stored.pages === "string" ? stored.pages : "",
      outputMono:
        typeof stored.outputMono === "boolean" ? stored.outputMono : true,
      outputDual:
        typeof stored.outputDual === "boolean" ? stored.outputDual : true,
    };
  } catch {
    return DEFAULT_DRAFT;
  }
}

function formatBytes(bytes: number): string {
  if (bytes < 1024 * 1024) {
    return `${Math.max(1, Math.round(bytes / 1024))} KB`;
  }
  return `${(bytes / 1024 / 1024).toFixed(bytes < 10 * 1024 * 1024 ? 1 : 0)} MB`;
}

function validatePages(value: string): boolean {
  const trimmed = value.trim();
  if (!trimmed) return true;
  if (!/^\d+(?:-\d+)?(?:\s*,\s*\d+(?:-\d+)?)*$/.test(trimmed)) {
    return false;
  }
  return trimmed.split(",").every((part) => {
    const [start, end = start] = part.trim().split("-").map(Number);
    return start > 0 && end >= start;
  });
}

function normalizedProgress(job: TranslationJob): number {
  if (job.status === "succeeded") return 100;
  return Math.min(100, Math.max(0, job.progress ?? 0));
}

function hasFile(
  job: TranslationJob,
  kind: "mono" | "dual" | "glossary",
  requested: boolean,
): boolean {
  const value = job.files?.[kind];
  return value === undefined || value === null ? requested : Boolean(value);
}

function errorText(error: TranslationJob["error"]): string {
  if (!error) return "服务暂时无法完成这份文档，请稍后重试。";
  return typeof error === "string" ? error : String(error);
}

interface JobPanelProps {
  job: TranslationJob | null;
  providers: ProviderDescriptor[];
  restoring: boolean;
  streamState: "idle" | "connecting" | "live" | "reconnecting";
  draft: JobDraft;
  hasRetryFile: boolean;
  actionError: string | null;
  busy: boolean;
  onCancel: () => void;
  onRetry: () => void;
  onReset: () => void;
}

function JobPanel({
  job,
  providers,
  restoring,
  streamState,
  draft,
  hasRetryFile,
  actionError,
  busy,
  onCancel,
  onRetry,
  onReset,
}: JobPanelProps) {
  if (restoring) {
    return (
      <section className="panel job-panel" aria-labelledby="job-title">
        <h2 id="job-title">当前任务</h2>
        <div className="empty-state" role="status">
          <LoaderCircle className="spin" aria-hidden="true" />
          <p>正在恢复上次任务…</p>
        </div>
        {actionError && <div className="alert alert-warning">{actionError}</div>}
      </section>
    );
  }

  if (!job) {
    return (
      <section className="panel job-panel" aria-labelledby="job-title">
        <h2 id="job-title">当前任务</h2>
        <div className="empty-state">
          <p>尚未提交任务。</p>
          <span>提交后可在这里查看进度、取消任务和下载结果。</span>
        </div>
        {actionError && (
          <div className="alert alert-error" role="alert">
            {actionError}
          </div>
        )}
      </section>
    );
  }

  const meta = STATUS_COPY[job.status];
  const progress = normalizedProgress(job);
  const active = ACTIVE_JOB_STATES.has(job.status);
  const providerId = job.options?.providerId ?? job.providerId ?? draft.providerId;
  const modelId = job.options?.modelId ?? job.modelId ?? draft.modelId;
  const jobProvider = providers.find((provider) => provider.id === providerId);
  const jobModel = jobProvider?.models.find((model) => model.id === modelId);
  const providerName = job.providerName ?? jobProvider?.name ?? providerId;
  const modelName = job.modelName ?? jobModel?.name ?? modelId;

  return (
    <section className="panel job-panel" aria-labelledby="job-title">
      <div className="panel-heading">
        <h2 id="job-title">当前任务</h2>
        <span className={`status status-${job.status}`}>{meta.label}</span>
      </div>

      <dl className="job-summary">
        <div>
          <dt>任务编号</dt>
          <dd title={job.id}>{job.id.slice(0, 12)}</dd>
        </div>
        {(providerName || modelName) && (
          <div>
            <dt>渠道 / 模型</dt>
            <dd>{[providerName, modelName].filter(Boolean).join(" / ")}</dd>
          </div>
        )}
      </dl>

      <div className="progress-block">
        <div className="progress-label">
          <strong>{meta.title}</strong>
          <span>{Math.round(progress)}%</span>
        </div>
        <progress max={100} value={progress} aria-label="翻译进度">
          {Math.round(progress)}%
        </progress>
        <p className="job-stage">{job.stage || "任务状态"}</p>
        <p className="job-message" aria-live="polite">
          {job.message || "等待状态更新。"}
        </p>
        {active && (
          <p className="connection-state">
            {streamState === "live"
              ? "进度实时连接正常"
              : streamState === "reconnecting"
                ? "进度连接中断，正在重连并轮询状态"
                : "正在连接进度服务"}
          </p>
        )}
      </div>

      {job.status === "failed" && (
        <div className="alert alert-error" role="alert">
          <strong>失败原因：</strong> {errorText(job.error)}
        </div>
      )}
      {actionError && (
        <div className="alert alert-error" role="alert">
          {actionError}
        </div>
      )}

      {job.status === "succeeded" && (
        <div className="download-actions" aria-label="译稿下载">
          {hasFile(job, "mono", draft.outputMono) && (
            <a className="button button-primary" href={jobFileUrl(job.id, "mono")} download>
              <Download aria-hidden="true" /> 下载纯译文
            </a>
          )}
          {hasFile(job, "dual", draft.outputDual) && (
            <a className="button button-secondary" href={jobFileUrl(job.id, "dual")} download>
              <Download aria-hidden="true" /> 下载双语对照
            </a>
          )}
          {hasFile(job, "glossary", false) && (
            <a className="button button-secondary" href={jobFileUrl(job.id, "glossary")} download>
              <Download aria-hidden="true" /> 下载术语表
            </a>
          )}
        </div>
      )}

      <div className="job-actions">
        {(job.status === "queued" || job.status === "running") && (
          <button className="button button-danger" type="button" onClick={onCancel}>
            <Square aria-hidden="true" /> 取消任务
          </button>
        )}
        {job.status === "cancelling" && (
          <button className="button button-secondary" type="button" disabled>
            <LoaderCircle className="spin" aria-hidden="true" /> 正在取消
          </button>
        )}
        {(job.status === "failed" || job.status === "cancelled") && (
          <>
            <button
              className="button button-primary"
              type="button"
              onClick={onRetry}
              disabled={!hasRetryFile || busy}
            >
              {busy ? (
                <LoaderCircle className="spin" aria-hidden="true" />
              ) : (
                <RotateCcw aria-hidden="true" />
              )}
              {busy ? "正在提交" : "重新提交"}
            </button>
            {!hasRetryFile && <span className="help-text">请重新选择原 PDF。</span>}
          </>
        )}
        {job.status === "succeeded" && (
          <button className="button button-secondary" type="button" onClick={onReset}>
            翻译另一份
          </button>
        )}
      </div>
    </section>
  );
}

function unavailableText(provider: ProviderDescriptor): string {
  if (provider.available) return "";
  return provider.unavailableReason === "not_configured"
    ? "（未配置）"
    : "（暂不可用）";
}

function modelThinkingModes(
  provider: ProviderDescriptor | undefined,
  model: ProviderModelOption | undefined,
): ThinkingMode[] {
  const modes = model?.thinkingModes ?? provider?.thinkingModes ?? [];
  const supported = modes.filter(isThinkingMode);
  return supported.length ? Array.from(new Set<ThinkingMode>(["default", ...supported])) : [];
}

export default function TranslationPage() {
  const [config, setConfig] = useState<AppConfig | null>(null);
  const [configLoading, setConfigLoading] = useState(true);
  const [configError, setConfigError] = useState<string | null>(null);
  const [draft, setDraft] = useState<JobDraft>(loadDraft);
  const [file, setFile] = useState<File | null>(null);
  const [dragging, setDragging] = useState(false);
  const [formError, setFormError] = useState<string | null>(null);
  const fileInputRef = useRef<HTMLInputElement>(null);
  const {
    job,
    restoring,
    creating,
    actionError,
    streamState,
    start,
    cancel,
    clear,
  } = useTranslationJob(config?.csrfToken);
  const active = Boolean(job && ACTIVE_JOB_STATES.has(job.status));

  const refreshConfig = useCallback(async () => {
    try {
      const serverConfig = await getConfig();
      setConfig({ ...serverConfig, providers: serverConfig.providers ?? [] });
      setConfigError(null);
    } catch {
      setConfigError("无法读取服务配置。请检查服务连接后重试。");
    } finally {
      setConfigLoading(false);
    }
  }, []);

  useEffect(() => {
    void refreshConfig();
    const timer = window.setInterval(() => void refreshConfig(), 30_000);
    return () => window.clearInterval(timer);
  }, [refreshConfig]);

  const providers = useMemo(() => normalizeProviders(config), [config]);
  const selectedProvider = providers.find(
    (provider) => provider.id === draft.providerId,
  );
  const selectedModel = selectedProvider?.models.find(
    (model) => model.id === draft.modelId,
  );
  const thinkingModes = modelThinkingModes(selectedProvider, selectedModel);
  const reasoningEfforts =
    selectedModel?.reasoningEfforts ?? selectedProvider?.reasoningEfforts ?? [];

  useEffect(() => {
    if (!config) return;
    setDraft((current) => {
      const provider =
        providers.find((item) => item.id === current.providerId) ??
        providers.find((item) => item.available) ??
        providers[0];
      const modelId = provider?.models.some((model) => model.id === current.modelId)
        ? current.modelId
        : defaultModel(provider);
      const model = provider?.models.find((item) => item.id === modelId);
      const modes = modelThinkingModes(provider, model);
      const thinkingMode = modes.length && modes.includes(current.thinkingMode)
        ? current.thinkingMode
        : "default";
      const efforts = model?.reasoningEfforts ?? provider?.reasoningEfforts ?? [];
      const reasoningEffort = efforts.includes(current.reasoningEffort)
        ? current.reasoningEffort
        : model?.defaultReasoningEffort ??
          provider?.defaultReasoningEffort ??
          efforts[0] ??
          "high";
      const languageCodes = new Set(config.languages.map((language) => language.code));
      const sourceLang = languageCodes.has(current.sourceLang)
        ? current.sourceLang
        : languageCodes.has("auto")
          ? "auto"
          : config.languages[0]?.code ?? "en";
      const targetLang =
        languageCodes.has(current.targetLang) && current.targetLang !== "auto"
          ? current.targetLang
          : config.languages.find(
                (language) => language.code !== "auto" && language.code !== sourceLang,
              )?.code ?? "zh";
      return {
        ...current,
        providerId: provider?.id ?? "",
        modelId,
        thinkingMode,
        reasoningEffort,
        sourceLang,
        targetLang,
      };
    });
  }, [config, providers]);

  useEffect(() => {
    try {
      const persisted: PersistedDraft = { ...draft, fileName: file?.name };
      window.localStorage.setItem(DRAFT_STORAGE_KEY, JSON.stringify(persisted));
    } catch {
      // Translation remains usable when browser storage is blocked.
    }
  }, [draft, file?.name]);

  const maxUploadBytes = config?.maxUploadBytes ?? DEFAULT_MAX_UPLOAD_BYTES;

  const acceptFile = (nextFile: File | undefined) => {
    setFormError(null);
    if (!nextFile) return;
    if (
      nextFile.type !== "application/pdf" &&
      !nextFile.name.toLowerCase().endsWith(".pdf")
    ) {
      setFile(null);
      setFormError("只能上传 PDF 文件。");
      return;
    }
    if (nextFile.size > maxUploadBytes) {
      setFile(null);
      setFormError(`文件超过 ${formatBytes(maxUploadBytes)} 上限。`);
      return;
    }
    setFile(nextFile);
  };

  const validateForm = (): string | null => {
    if (!config) return "服务配置尚未就绪。";
    if (!file) return "请先选择要翻译的 PDF。";
    if (!selectedProvider) return "请选择翻译渠道。";
    if (!selectedProvider.available) {
      return selectedProvider.unavailableReason === "not_configured"
        ? `${selectedProvider.name} 尚未由管理员配置。`
        : `${selectedProvider.name} 当前不可用，请稍后重试。`;
    }
    if (selectedProvider.models.length && !selectedModel) return "请选择翻译模型。";
    if (draft.sourceLang === draft.targetLang) return "源语言和目标语言不能相同。";
    if (!validatePages(draft.pages)) {
      return "页码范围格式不正确，请使用如 1-3, 5, 8-10。";
    }
    if (!draft.outputMono && !draft.outputDual) return "请至少选择一种输出方式。";
    return null;
  };

  const submit = async () => {
    const validationError = validateForm();
    setFormError(validationError);
    if (validationError || !file) return;
    await start(file, draft);
  };

  const handleSubmit = (event: FormEvent) => {
    event.preventDefault();
    void submit();
  };

  const handleFileChange = (event: ChangeEvent<HTMLInputElement>) => {
    acceptFile(event.target.files?.[0]);
  };

  const handleDrop = (event: DragEvent<HTMLDivElement>) => {
    event.preventDefault();
    setDragging(false);
    if (!active && !creating) acceptFile(event.dataTransfer.files?.[0]);
  };

  const selectProvider = (providerId: string) => {
    const provider = providers.find((item) => item.id === providerId);
    setDraft((current) => ({
      ...current,
      providerId,
      modelId: defaultModel(provider),
      thinkingMode: "default",
      reasoningEffort: provider?.defaultReasoningEffort ?? "high",
    }));
  };

  const resetForAnother = () => {
    clear();
    setFile(null);
    setFormError(null);
    if (fileInputRef.current) fileInputRef.current.value = "";
    fileInputRef.current?.focus();
  };

  const sourceName = config?.languages.find(
    (language) => language.code === draft.sourceLang,
  )?.name;
  const targetName = config?.languages.find(
    (language) => language.code === draft.targetLang,
  )?.name;
  const formDisabled = active || creating || restoring || !config;

  return (
    <div className="app-page">
      <a className="skip-link" href="#main-content">跳到主要内容</a>
      <header className="site-header">
        <div className="site-header-inner">
          <a className="site-title" href="/">PDF 译稿</a>
          <nav aria-label="主要导航">
            <a href="/admin">管理后台</a>
          </nav>
        </div>
      </header>

      <main id="main-content" className="page-content">
        <div className="page-intro">
          <div>
            <h1>PDF 翻译</h1>
            <p>上传文档，选择翻译渠道和输出格式。</p>
          </div>
          <span className={`service-state ${config ? "is-online" : "is-offline"}`} role="status">
            {configLoading ? "正在连接" : config ? "服务已连接" : "服务不可用"}
          </span>
        </div>

        {configError && (
          <div className="alert alert-warning config-alert" role="alert">
            <span>{configError}</span>
            <button className="link-button" type="button" onClick={() => void refreshConfig()}>
              <RefreshCw aria-hidden="true" /> 重新加载
            </button>
          </div>
        )}

        <div className="translation-layout">
          <section className="panel settings-panel" aria-labelledby="settings-title">
            <h2 id="settings-title">翻译设置</h2>
            <form onSubmit={handleSubmit} noValidate>
              <fieldset disabled={formDisabled}>
                <legend className="sr-only">翻译设置</legend>

                <div className="form-section">
                  <h3>文档</h3>
                  <div
                    className={`upload-field${dragging ? " is-dragging" : ""}`}
                    onDragEnter={(event) => {
                      event.preventDefault();
                      setDragging(true);
                    }}
                    onDragOver={(event) => event.preventDefault()}
                    onDragLeave={(event) => {
                      if (!event.currentTarget.contains(event.relatedTarget as Node)) {
                        setDragging(false);
                      }
                    }}
                    onDrop={handleDrop}
                  >
                    <label htmlFor="pdf-file">PDF 文件</label>
                    <input
                      ref={fileInputRef}
                      id="pdf-file"
                      type="file"
                      accept="application/pdf,.pdf"
                      aria-describedby="file-help"
                      onChange={handleFileChange}
                    />
                    <small id="file-help">
                      单个 PDF，最大 {formatBytes(maxUploadBytes)}。也可拖放到此处。
                    </small>
                    {file && (
                      <div className="selected-file">
                        <span>
                          <strong>{file.name}</strong>
                          <small>{formatBytes(file.size)}</small>
                        </span>
                        <button
                          className="icon-button"
                          type="button"
                          aria-label={`移除 ${file.name}`}
                          onClick={() => {
                            setFile(null);
                            if (fileInputRef.current) fileInputRef.current.value = "";
                          }}
                        >
                          <X aria-hidden="true" />
                        </button>
                      </div>
                    )}
                  </div>
                </div>

                <div className="form-section">
                  <h3>渠道与模型</h3>
                  <div className="form-grid">
                    <label className="field">
                      <span>翻译渠道</span>
                      <select
                        value={draft.providerId}
                        onChange={(event) => selectProvider(event.target.value)}
                      >
                        {!providers.length && <option value="">没有可用渠道</option>}
                        {providers.map((provider) => (
                          <option
                            key={provider.id}
                            value={provider.id}
                            disabled={!provider.available}
                          >
                            {provider.name}{unavailableText(provider)}
                          </option>
                        ))}
                      </select>
                    </label>

                    {selectedProvider?.models.length ? (
                      <label className="field">
                        <span>模型</span>
                        <select
                          value={draft.modelId}
                          onChange={(event) =>
                            setDraft((current) => ({
                              ...current,
                              modelId: event.target.value,
                              thinkingMode: "default",
                            }))
                          }
                        >
                          {selectedProvider.models.map((model) => (
                            <option key={model.id} value={model.id}>{model.name}</option>
                          ))}
                        </select>
                      </label>
                    ) : null}
                  </div>

                  {thinkingModes.length > 0 && (
                    <details className="advanced-settings">
                      <summary>模型高级设置</summary>
                      <div className="form-grid">
                        <label className="field">
                          <span>Thinking 模式</span>
                          <select
                            value={draft.thinkingMode}
                            onChange={(event) =>
                              setDraft((current) => ({
                                ...current,
                                thinkingMode: event.target.value as ThinkingMode,
                              }))
                            }
                          >
                            {thinkingModes.map((mode) => (
                              <option key={mode} value={mode}>
                                {mode === "default"
                                  ? "使用服务器默认"
                                  : mode === "enabled"
                                    ? "启用"
                                    : "关闭"}
                              </option>
                            ))}
                          </select>
                        </label>
                        {draft.thinkingMode === "enabled" && reasoningEfforts.length > 0 && (
                          <label className="field">
                            <span>推理强度</span>
                            <select
                              value={draft.reasoningEffort}
                              onChange={(event) =>
                                setDraft((current) => ({
                                  ...current,
                                  reasoningEffort: event.target.value,
                                }))
                              }
                            >
                              {reasoningEfforts.map((effort) => (
                                <option key={effort} value={effort}>{effort}</option>
                              ))}
                            </select>
                          </label>
                        )}
                      </div>
                    </details>
                  )}
                </div>

                <div className="form-section">
                  <h3>语言与页码</h3>
                  <div className="language-grid">
                    <label className="field">
                      <span>源语言</span>
                      <select
                        value={draft.sourceLang}
                        onChange={(event) =>
                          setDraft((current) => ({ ...current, sourceLang: event.target.value }))
                        }
                      >
                        {(config?.languages ?? []).map((language) => (
                          <option key={language.code} value={language.code}>{language.name}</option>
                        ))}
                      </select>
                    </label>
                    <button
                      className="swap-button"
                      type="button"
                      aria-label={`交换语言：${sourceName ?? draft.sourceLang} 与 ${targetName ?? draft.targetLang}`}
                      disabled={draft.sourceLang === "auto"}
                      onClick={() =>
                        setDraft((current) => ({
                          ...current,
                          sourceLang: current.targetLang,
                          targetLang: current.sourceLang,
                        }))
                      }
                    >
                      <ArrowRightLeft aria-hidden="true" />
                    </button>
                    <label className="field">
                      <span>目标语言</span>
                      <select
                        value={draft.targetLang}
                        onChange={(event) =>
                          setDraft((current) => ({ ...current, targetLang: event.target.value }))
                        }
                      >
                        {(config?.languages ?? [])
                          .filter((language) => language.code !== "auto")
                          .map((language) => (
                            <option key={language.code} value={language.code}>{language.name}</option>
                          ))}
                      </select>
                    </label>
                  </div>
                  <label className="field field-full">
                    <span>页码范围（可选）</span>
                    <input
                      type="text"
                      inputMode="numeric"
                      value={draft.pages}
                      placeholder="留空翻译全部，例如 1-3, 5, 8-10"
                      aria-describedby="pages-help"
                      onChange={(event) =>
                        setDraft((current) => ({ ...current, pages: event.target.value }))
                      }
                    />
                    <small id="pages-help">支持单页、连续范围和逗号分隔。</small>
                  </label>
                </div>

                <div className="form-section">
                  <fieldset className="checkbox-group">
                    <legend>输出文件</legend>
                    <label>
                      <input
                        type="checkbox"
                        checked={draft.outputMono}
                        onChange={(event) =>
                          setDraft((current) => ({
                            ...current,
                            outputMono: event.target.checked,
                          }))
                        }
                      />
                      纯译文
                    </label>
                    <label>
                      <input
                        type="checkbox"
                        checked={draft.outputDual}
                        onChange={(event) =>
                          setDraft((current) => ({
                            ...current,
                            outputDual: event.target.checked,
                          }))
                        }
                      />
                      双语对照
                    </label>
                  </fieldset>
                </div>
              </fieldset>

              {formError && (
                <div className="alert alert-error" role="alert">{formError}</div>
              )}

              <button
                className="button button-primary submit-button"
                type="submit"
                disabled={formDisabled || !selectedProvider?.available}
              >
                {creating && <LoaderCircle className="spin" aria-hidden="true" />}
                {creating ? "正在提交" : active ? "任务进行中" : "开始翻译"}
              </button>
              <p className="form-note">关闭页面时会通知服务端释放任务；刷新后可恢复当前进度。</p>
            </form>
          </section>

          <JobPanel
            job={job}
            providers={providers}
            restoring={restoring}
            streamState={streamState}
            draft={draft}
            hasRetryFile={Boolean(file)}
            actionError={actionError}
            busy={creating}
            onCancel={() => void cancel()}
            onRetry={() => void submit()}
            onReset={resetForAnother}
          />
        </div>
      </main>
    </div>
  );
}
