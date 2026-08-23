import {
  AlertCircle,
  ArrowRightLeft,
  Check,
  CheckCircle2,
  Clock3,
  Download,
  FileText,
  Languages,
  LoaderCircle,
  RefreshCw,
  RotateCcw,
  ShieldCheck,
  Sparkles,
  Square,
  UploadCloud,
  Wifi,
  WifiOff,
  X,
} from "lucide-react";
import {
  type ChangeEvent,
  type CSSProperties,
  type DragEvent,
  type FormEvent,
  useEffect,
  useMemo,
  useRef,
  useState,
} from "react";

import { checkHealth, getConfig, jobFileUrl } from "./api";
import type {
  AppConfig,
  JobDraft,
  JobStatus,
  PersistedDraft,
  TranslationJob,
} from "./types";
import { ACTIVE_JOB_STATES } from "./types";
import { useTranslationJob } from "./useTranslationJob";

const DRAFT_STORAGE_KEY = "pdf2zh.translation-draft";
const FALLBACK_CONFIG: AppConfig = {
  provider: "google",
  maxUploadBytes: 100 * 1024 * 1024,
  languages: [
    { code: "en", name: "英语" },
    { code: "zh", name: "简体中文" },
    { code: "zh-TW", name: "繁體中文" },
    { code: "ja", name: "日语" },
    { code: "ko", name: "韩语" },
    { code: "de", name: "德语" },
    { code: "fr", name: "法语" },
    { code: "es", name: "西班牙语" },
  ],
};

const DEFAULT_DRAFT: JobDraft = {
  sourceLang: "en",
  targetLang: "zh",
  pages: "",
  outputMono: true,
  outputDual: true,
};

const STATUS_COPY: Record<
  JobStatus,
  { label: string; title: string; tone: string }
> = {
  queued: { label: "排队中", title: "任务已进入队列", tone: "waiting" },
  running: { label: "翻译中", title: "正在处理文档", tone: "working" },
  cancelling: { label: "正在取消", title: "正在安全停止任务", tone: "waiting" },
  succeeded: { label: "已完成", title: "译稿已准备好", tone: "success" },
  failed: { label: "失败", title: "这次翻译没有完成", tone: "danger" },
  cancelled: { label: "已取消", title: "任务已停止", tone: "neutral" },
};

function loadDraft(): JobDraft {
  try {
    const stored = JSON.parse(
      window.localStorage.getItem(DRAFT_STORAGE_KEY) ?? "null",
    ) as Partial<PersistedDraft> | null;
    const draft = stored ? { ...DEFAULT_DRAFT, ...stored } : DEFAULT_DRAFT;
    const fallbackCodes = new Set(
      FALLBACK_CONFIG.languages.map((language) => language.code),
    );
    return {
      ...draft,
      sourceLang: fallbackCodes.has(draft.sourceLang)
        ? draft.sourceLang
        : DEFAULT_DRAFT.sourceLang,
      targetLang:
        draft.targetLang !== "auto" && fallbackCodes.has(draft.targetLang)
          ? draft.targetLang
          : DEFAULT_DRAFT.targetLang,
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
  if (!trimmed) {
    return true;
  }
  if (!/^\d+(?:-\d+)?(?:\s*,\s*\d+(?:-\d+)?)*$/.test(trimmed)) {
    return false;
  }
  return trimmed.split(",").every((part) => {
    const [start, end = start] = part.trim().split("-").map(Number);
    return start > 0 && end >= start;
  });
}

function normalizedProgress(job: TranslationJob): number {
  if (job.status === "succeeded") {
    return 100;
  }
  const progress = typeof job.progress === "number" ? job.progress : 0;
  return Math.min(100, Math.max(0, progress));
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
  if (!error) {
    return "服务暂时无法完成这份文档，请稍后重试。";
  }
  return typeof error === "string" ? error : String(error);
}

interface StatusPanelProps {
  job: TranslationJob | null;
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

function StatusPanel({
  job,
  restoring,
  streamState,
  draft,
  hasRetryFile,
  actionError,
  busy,
  onCancel,
  onRetry,
  onReset,
}: StatusPanelProps) {
  if (restoring) {
    return (
      <section className="status-panel status-panel--empty" aria-live="polite">
        <LoaderCircle className="status-loader" aria-hidden="true" />
        <p className="eyebrow">正在恢复</p>
        <h2>读取上次任务…</h2>
        <p>刷新页面不会丢失已提交的任务。</p>
        {actionError && (
          <div className="inline-alert inline-alert--warning" role="status">
            <RefreshCw className="spin-slow" aria-hidden="true" />
            <span>{actionError}</span>
          </div>
        )}
      </section>
    );
  }

  if (!job) {
    return (
      <section className="status-panel status-panel--empty" aria-live="polite">
        <div className="empty-orbit" aria-hidden="true">
          <FileText />
        </div>
        <p className="eyebrow">准备就绪</p>
        <h2>让论文更容易阅读</h2>
        <p>
          上传 PDF 并选择输出方式。任务运行时，这里会实时显示解析、翻译与排版进度。
        </p>
        {actionError && (
          <div className="inline-alert" role="alert">
            <AlertCircle aria-hidden="true" />
            <span>{actionError}</span>
          </div>
        )}
        <div className="empty-points" aria-label="处理特性">
          <span>
            <Check aria-hidden="true" /> 保留公式与版式
          </span>
          <span>
            <Check aria-hidden="true" /> 实时进度与可取消队列
          </span>
        </div>
      </section>
    );
  }

  const meta = STATUS_COPY[job.status];
  const progress = normalizedProgress(job);
  const active = ACTIVE_JOB_STATES.has(job.status);
  const ringStyle = { "--progress": `${progress * 3.6}deg` } as CSSProperties;

  return (
    <section
      className={`status-panel status-panel--${meta.tone}`}
      aria-live="polite"
      aria-label="翻译任务状态"
    >
      <div className="status-topline">
        <span className={`status-badge status-badge--${meta.tone}`}>
          <span aria-hidden="true" />
          {meta.label}
        </span>
        <span className="job-id" title={job.id}>
          任务 {job.id.slice(0, 8)}
        </span>
      </div>

      <div className="progress-hero">
        <div
          className={`progress-ring progress-ring--${meta.tone}`}
          style={ringStyle}
          role="progressbar"
          aria-label="翻译进度"
          aria-valuemin={0}
          aria-valuemax={100}
          aria-valuenow={Math.round(progress)}
        >
          <div>
            {job.status === "succeeded" ? (
              <CheckCircle2 aria-hidden="true" />
            ) : job.status === "failed" ? (
              <AlertCircle aria-hidden="true" />
            ) : job.status === "cancelled" ? (
              <Square aria-hidden="true" />
            ) : (
              <strong>{Math.round(progress)}%</strong>
            )}
          </div>
        </div>
        <div className="progress-copy">
          <p className="eyebrow">{job.stage || "任务状态"}</p>
          <h2>{meta.title}</h2>
          <p>{job.message || "状态更新后会自动显示在这里。"}</p>
        </div>
      </div>

      {active && (
        <div className="live-row">
          <span>
            {streamState === "live" ? (
              <Wifi aria-hidden="true" />
            ) : (
              <RefreshCw
                className={streamState !== "idle" ? "spin-slow" : ""}
                aria-hidden="true"
              />
            )}
            {streamState === "live"
              ? "实时同步中"
              : streamState === "reconnecting"
                ? "正在重连进度流"
                : "正在连接进度流"}
          </span>
          <span>
            <Clock3 aria-hidden="true" /> 离开页面将安全停止
          </span>
        </div>
      )}

      {job.status === "failed" && (
        <div className="failure-box" role="alert">
          <strong>失败原因</strong>
          <p>{errorText(job.error)}</p>
        </div>
      )}

      {actionError && (
        <div className="inline-alert" role="alert">
          <AlertCircle aria-hidden="true" />
          <span>{actionError}</span>
        </div>
      )}

      {job.status === "succeeded" && (
        <div className="download-list" aria-label="译稿下载">
          {hasFile(job, "mono", draft.outputMono) && (
            <a className="download-button" href={jobFileUrl(job.id, "mono")} download>
              <span>
                <FileText aria-hidden="true" />
                <span>
                  <strong>下载纯译文</strong>
                  <small>Mono PDF</small>
                </span>
              </span>
              <Download aria-hidden="true" />
            </a>
          )}
          {hasFile(job, "dual", draft.outputDual) && (
            <a className="download-button" href={jobFileUrl(job.id, "dual")} download>
              <span>
                <Languages aria-hidden="true" />
                <span>
                  <strong>下载双语对照</strong>
                  <small>Dual PDF</small>
                </span>
              </span>
              <Download aria-hidden="true" />
            </a>
          )}
          {hasFile(job, "glossary", false) && (
            <a
              className="download-button download-button--subtle"
              href={jobFileUrl(job.id, "glossary")}
              download
            >
              <span>
                <Sparkles aria-hidden="true" />
                <span>
                  <strong>下载术语表</strong>
                  <small>Glossary</small>
                </span>
              </span>
              <Download aria-hidden="true" />
            </a>
          )}
        </div>
      )}

      <div className="status-actions">
        {(job.status === "queued" || job.status === "running") && (
          <button className="button button--danger" type="button" onClick={onCancel}>
            <Square aria-hidden="true" />
            取消任务
          </button>
        )}
        {job.status === "cancelling" && (
          <button className="button button--muted" type="button" disabled>
            <LoaderCircle className="spin" aria-hidden="true" />
            正在回收资源
          </button>
        )}
        {(job.status === "failed" || job.status === "cancelled") && (
          <>
            <button
              className="button button--primary"
              type="button"
              onClick={onRetry}
              disabled={!hasRetryFile || busy}
            >
              {busy ? (
                <LoaderCircle className="spin" aria-hidden="true" />
              ) : (
                <RotateCcw aria-hidden="true" />
              )}
              {busy ? "正在上传" : "重新提交"}
            </button>
            {!hasRetryFile && <small>请先重新选择原 PDF。</small>}
          </>
        )}
        {job.status === "succeeded" && (
          <button className="button button--muted" type="button" onClick={onReset}>
            <UploadCloud aria-hidden="true" />
            翻译另一份
          </button>
        )}
      </div>
    </section>
  );
}

export default function App() {
  const [config, setConfig] = useState<AppConfig>(FALLBACK_CONFIG);
  const [health, setHealth] = useState<"checking" | "online" | "offline">(
    "checking",
  );
  const [configWarning, setConfigWarning] = useState<string | null>(null);
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
  } = useTranslationJob();
  const active = Boolean(job && ACTIVE_JOB_STATES.has(job.status));

  useEffect(() => {
    let disposed = false;
    void getConfig()
      .then((serverConfig) => {
        if (!disposed) {
          setConfig(serverConfig);
          setConfigWarning(null);
          setDraft((current) => {
            const codes = new Set(serverConfig.languages.map((language) => language.code));
            const sourceLang = codes.has(current.sourceLang)
              ? current.sourceLang
              : codes.has("auto")
                ? "auto"
                : serverConfig.languages[0]?.code ?? "en";
            const targetLang =
              codes.has(current.targetLang) && current.targetLang !== "auto"
                ? current.targetLang
                : codes.has("zh")
                  ? "zh"
                  : codes.has("zh-CN")
                    ? "zh-CN"
                    : serverConfig.languages.find(
                          (language) =>
                            language.code !== "auto" && language.code !== sourceLang,
                        )?.code ??
                      serverConfig.languages.find(
                        (language) => language.code !== "auto",
                      )?.code ??
                      "zh";
            return {
              ...current,
              sourceLang,
              targetLang,
            };
          });
        }
      })
      .catch(() => {
        if (!disposed) {
          setConfigWarning("暂时无法读取服务配置，已显示默认语言列表。");
        }
      });
    return () => {
      disposed = true;
    };
  }, []);

  useEffect(() => {
    let disposed = false;
    const probe = () => {
      void checkHealth()
        .then((ok) => {
          if (!disposed) {
            setHealth(ok ? "online" : "offline");
          }
        })
        .catch(() => {
          if (!disposed) {
            setHealth("offline");
          }
        });
    };
    probe();
    const timer = window.setInterval(probe, 30_000);
    return () => {
      disposed = true;
      window.clearInterval(timer);
    };
  }, []);

  useEffect(() => {
    const persisted: PersistedDraft = {
      ...draft,
      fileName: file?.name,
    };
    window.localStorage.setItem(DRAFT_STORAGE_KEY, JSON.stringify(persisted));
  }, [draft, file?.name]);

  const sourceName = useMemo(
    () => config.languages.find((item) => item.code === draft.sourceLang)?.name,
    [config.languages, draft.sourceLang],
  );
  const targetName = useMemo(
    () => config.languages.find((item) => item.code === draft.targetLang)?.name,
    [config.languages, draft.targetLang],
  );

  const acceptFile = (nextFile: File | undefined) => {
    setFormError(null);
    if (!nextFile) {
      return;
    }
    if (
      nextFile.type !== "application/pdf" &&
      !nextFile.name.toLowerCase().endsWith(".pdf")
    ) {
      setFile(null);
      setFormError("只能上传 PDF 文件。");
      return;
    }
    if (nextFile.size > config.maxUploadBytes) {
      setFile(null);
      setFormError(`文件超过 ${formatBytes(config.maxUploadBytes)} 上限。`);
      return;
    }
    setFile(nextFile);
  };

  const validateForm = (): string | null => {
    if (!file) {
      return "请先选择要翻译的 PDF。";
    }
    if (file.size > config.maxUploadBytes) {
      return `文件超过 ${formatBytes(config.maxUploadBytes)} 上限。`;
    }
    if (draft.sourceLang === draft.targetLang) {
      return "源语言和目标语言不能相同。";
    }
    if (!validatePages(draft.pages)) {
      return "页码范围格式不正确，请使用如 1-3, 5, 8-10。";
    }
    if (!draft.outputMono && !draft.outputDual) {
      return "请至少选择一种输出方式。";
    }
    return null;
  };

  const submit = async () => {
    const validationError = validateForm();
    setFormError(validationError);
    if (validationError || !file) {
      return;
    }
    await start(file, draft);
  };

  const handleSubmit = (event: FormEvent) => {
    event.preventDefault();
    void submit();
  };

  const handleFileChange = (event: ChangeEvent<HTMLInputElement>) => {
    acceptFile(event.target.files?.[0]);
    event.target.value = "";
  };

  const handleDrop = (event: DragEvent<HTMLLabelElement>) => {
    event.preventDefault();
    setDragging(false);
    if (!active) {
      acceptFile(event.dataTransfer.files?.[0]);
    }
  };

  const resetForAnother = () => {
    clear();
    setFile(null);
    setFormError(null);
    fileInputRef.current?.focus();
  };

  return (
    <main className="app-shell">
      <div className="ambient ambient--one" aria-hidden="true" />
      <div className="ambient ambient--two" aria-hidden="true" />

      <section className="workbench" aria-labelledby="app-title">
        <header className="app-header">
          <div className="brand-lockup">
            <div className="brand-mark" aria-hidden="true">
              <Languages />
            </div>
            <div>
              <div className="brand-line">
                <h1 id="app-title">PDF 译稿</h1>
                <span className="provider-pill">Google</span>
              </div>
              <p>保留公式与版式的学术 PDF 翻译</p>
            </div>
          </div>
          <div
            className={`health-chip health-chip--${health}`}
            title={health === "online" ? "服务连接正常" : "正在检查服务连接"}
            aria-live="polite"
          >
            {health === "online" ? (
              <Wifi aria-hidden="true" />
            ) : health === "offline" ? (
              <WifiOff aria-hidden="true" />
            ) : (
              <LoaderCircle className="spin" aria-hidden="true" />
            )}
            <span>
              {health === "online"
                ? "服务正常"
                : health === "offline"
                  ? "连接异常"
                  : "正在连接"}
            </span>
          </div>
        </header>

        <div className="workbench-grid">
          <form className="translation-form" onSubmit={handleSubmit} noValidate>
            <fieldset disabled={active || creating}>
              <legend className="sr-only">翻译设置</legend>

              <div className="section-heading">
                <span>01</span>
                <div>
                  <h2>选择文档</h2>
                  <p>文件上限 {formatBytes(config.maxUploadBytes)}</p>
                </div>
              </div>

              <label
                className={`drop-zone${dragging ? " drop-zone--dragging" : ""}${
                  file ? " drop-zone--selected" : ""
                }`}
                onDragEnter={(event) => {
                  event.preventDefault();
                  if (!active) setDragging(true);
                }}
                onDragOver={(event) => event.preventDefault()}
                onDragLeave={(event) => {
                  if (!event.currentTarget.contains(event.relatedTarget as Node)) {
                    setDragging(false);
                  }
                }}
                onDrop={handleDrop}
              >
                <input
                  ref={fileInputRef}
                  type="file"
                  accept="application/pdf,.pdf"
                  aria-label="选择 PDF 文件"
                  onChange={handleFileChange}
                  aria-describedby="upload-help"
                />
                {file ? (
                  <>
                    <div className="file-icon" aria-hidden="true">
                      <FileText />
                    </div>
                    <span className="file-copy">
                      <strong>{file.name}</strong>
                      <small>{formatBytes(file.size)} · PDF</small>
                    </span>
                    <button
                      className="icon-button"
                      type="button"
                      aria-label={`移除 ${file.name}`}
                      onClick={(event) => {
                        event.preventDefault();
                        setFile(null);
                      }}
                    >
                      <X aria-hidden="true" />
                    </button>
                  </>
                ) : (
                  <>
                    <div className="upload-symbol" aria-hidden="true">
                      <UploadCloud />
                    </div>
                    <span className="file-copy">
                      <strong>拖入 PDF，或点击选择</strong>
                      <small id="upload-help">单个 PDF 文件</small>
                    </span>
                  </>
                )}
              </label>

              <div className="section-heading section-heading--spaced">
                <span>02</span>
                <div>
                  <h2>翻译设置</h2>
                  <p>请确认语言和页码范围</p>
                </div>
              </div>

              <div className="language-row">
                <label className="field">
                  <span>源语言</span>
                  <select
                    aria-label="源语言"
                    value={draft.sourceLang}
                    onChange={(event) =>
                      setDraft((current) => ({
                        ...current,
                        sourceLang: event.target.value,
                      }))
                    }
                  >
                    {config.languages.map((language) => (
                      <option key={language.code} value={language.code}>
                        {language.name}
                      </option>
                    ))}
                  </select>
                </label>
                <button
                  className="swap-button"
                  type="button"
                  aria-label={`交换语言：${sourceName ?? draft.sourceLang} 与 ${targetName ?? draft.targetLang}`}
                  disabled={draft.sourceLang === "auto"}
                  title={draft.sourceLang === "auto" ? "自动检测不能作为目标语言" : undefined}
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
                    aria-label="目标语言"
                    value={draft.targetLang}
                    onChange={(event) =>
                      setDraft((current) => ({
                        ...current,
                        targetLang: event.target.value,
                      }))
                    }
                  >
                    {config.languages
                      .filter((language) => language.code !== "auto")
                      .map((language) => (
                        <option key={language.code} value={language.code}>
                          {language.name}
                        </option>
                      ))}
                  </select>
                </label>
              </div>

              <label className="field field--full">
                <span>页码范围 <em>可选</em></span>
                <input
                  type="text"
                  inputMode="numeric"
                  value={draft.pages}
                  placeholder="留空翻译全部，如 1-3, 5, 8-10"
                  aria-describedby="pages-help"
                  onChange={(event) =>
                    setDraft((current) => ({ ...current, pages: event.target.value }))
                  }
                />
                <small id="pages-help">支持单页、连续范围和逗号分隔。</small>
              </label>

              <div className="output-heading">
                <span>输出文件</span>
                <small>至少选择一项</small>
              </div>
              <div className="output-options">
                <label className={`output-card${draft.outputMono ? " is-checked" : ""}`}>
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
                  <span className="check-box" aria-hidden="true">
                    <Check />
                  </span>
                  <span>
                    <strong>纯译文</strong>
                    <small>适合专注阅读</small>
                  </span>
                </label>
                <label className={`output-card${draft.outputDual ? " is-checked" : ""}`}>
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
                  <span className="check-box" aria-hidden="true">
                    <Check />
                  </span>
                  <span>
                    <strong>双语对照</strong>
                    <small>便于对照原文</small>
                  </span>
                </label>
              </div>
            </fieldset>

            {(formError || configWarning) && (
              <div className="form-messages" aria-live="assertive">
                {formError && (
                  <div className="inline-alert" role="alert">
                    <AlertCircle aria-hidden="true" />
                    <span>{formError}</span>
                  </div>
                )}
                {configWarning && (
                  <div className="inline-alert inline-alert--warning">
                    <AlertCircle aria-hidden="true" />
                    <span>{configWarning}</span>
                  </div>
                )}
              </div>
            )}

            <button
              className="submit-button"
              type="submit"
              disabled={active || creating || restoring}
            >
              {creating ? (
                <LoaderCircle className="spin" aria-hidden="true" />
              ) : (
                <Sparkles aria-hidden="true" />
              )}
              <span>{creating ? "正在上传…" : active ? "任务进行中" : "开始翻译"}</span>
            </button>

            <p className="form-footnote">
              <ShieldCheck aria-hidden="true" />
              页面关闭时会通知服务端释放或取消任务，避免占用队列。
            </p>
          </form>

          <StatusPanel
            job={job}
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
      </section>

      <footer className="page-footer">
        <span>翻译渠道：Google</span>
        <span aria-hidden="true">·</span>
        <span>任务进度会自动保存</span>
      </footer>
    </main>
  );
}
