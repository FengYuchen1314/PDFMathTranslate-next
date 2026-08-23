import { useCallback, useEffect, useRef, useState } from "react";

import {
  ApiRequestError,
  cancelJob,
  createClientJobId,
  createJob,
  deleteJob,
  eventStreamUrl,
  getJob,
  heartbeatJob,
  releaseJob,
  sendJobBeacon,
} from "./api";
import {
  ACTIVE_JOB_STATES,
  TERMINAL_JOB_STATES,
  type JobDraft,
  type TranslationJob,
} from "./types";

const ACTIVE_JOB_STORAGE_KEY = "pdf2zh.active-job-id";
const HEARTBEAT_INTERVAL_MS = 10_000;
const STATUS_POLL_INTERVAL_MS = 10_000;
const RESTORE_RETRY_INTERVAL_MS = 4_000;
const SUPPORTED_JOB_STATES = new Set([
  "queued",
  "running",
  "cancelling",
  "succeeded",
  "failed",
  "cancelled",
]);

type StreamState = "idle" | "connecting" | "live" | "reconnecting";

function readActiveJobId(): string | null {
  try {
    return window.sessionStorage.getItem(ACTIVE_JOB_STORAGE_KEY);
  } catch {
    return null;
  }
}

function persistActiveJobId(job: TranslationJob | null): void {
  persistActiveJobIdValue(job?.id ?? null);
}

function persistActiveJobIdValue(jobId: string | null): void {
  try {
    if (jobId) {
      window.sessionStorage.setItem(ACTIVE_JOB_STORAGE_KEY, jobId);
    } else {
      window.sessionStorage.removeItem(ACTIVE_JOB_STORAGE_KEY);
    }
  } catch {
    // The in-memory lifecycle remains functional when browser storage is blocked.
  }
}

function normalizeProgress(value: unknown): number | undefined {
  if (typeof value !== "number" || Number.isNaN(value)) {
    return undefined;
  }
  return Math.min(100, Math.max(0, value));
}

export function mergeJobUpdate(
  previous: TranslationJob | null,
  raw: unknown,
  expectedId: string,
  cancelRequested = false,
): TranslationJob | null {
  if (!raw || typeof raw !== "object") {
    return previous;
  }
  if (previous && previous.id !== expectedId) {
    return previous;
  }
  const envelope = raw as Record<string, unknown>;
  const candidate =
    envelope.job && typeof envelope.job === "object"
      ? (envelope.job as Record<string, unknown>)
      : envelope;
  const candidateId =
    typeof candidate.id === "string" ? candidate.id : expectedId;
  if (candidateId !== expectedId) {
    return previous;
  }

  const normalizedCandidate = { ...candidate };
  // Tolerate older servers that briefly exposed their internal startup state.
  if (normalizedCandidate.status === "starting") {
    normalizedCandidate.status = "queued";
  } else if (
    normalizedCandidate.status !== undefined &&
    (typeof normalizedCandidate.status !== "string" ||
      !SUPPORTED_JOB_STATES.has(normalizedCandidate.status))
  ) {
    delete normalizedCandidate.status;
  }

  if (previous) {
    const nextRevision = normalizedCandidate.revision;
    if (
      typeof previous.revision === "number" &&
      typeof nextRevision === "number" &&
      nextRevision < previous.revision
    ) {
      return previous;
    }
    const nextStatus = normalizedCandidate.status;
    if (
      TERMINAL_JOB_STATES.has(previous.status) &&
      typeof nextStatus === "string" &&
      nextStatus !== previous.status
    ) {
      return previous;
    }
  }

  const next = {
    ...(previous ?? { id: expectedId, status: "queued" as const }),
    ...normalizedCandidate,
    id: expectedId,
  } as TranslationJob;
  const normalizedProgress = normalizeProgress(candidate.progress);
  if (normalizedProgress !== undefined) {
    next.progress = normalizedProgress;
  }
  if (
    cancelRequested &&
    (next.status === "queued" || next.status === "running")
  ) {
    next.status = "cancelling";
  }
  return next;
}

function parseEvent(event: MessageEvent<string>): unknown {
  try {
    return JSON.parse(event.data) as unknown;
  } catch {
    return null;
  }
}

async function resolveReservedJob(
  reservedJobId: string,
): Promise<TranslationJob | null> {
  try {
    return await getJob(reservedJobId);
  } catch (error) {
    if (error instanceof ApiRequestError && error.status === 410) {
      return null;
    }
    if (!(error instanceof ApiRequestError) || error.status !== 404) {
      throw error;
    }
  }

  // A 404 is only an observation, not proof that a request whose response was
  // lost cannot commit later. release() shares the backend's reservation gate:
  // it either marks an already-created job as released or durably tombstones
  // this exact UUID before an in-flight create can pass the same gate.
  try {
    await releaseJob(reservedJobId);
  } catch (error) {
    if (!(error instanceof ApiRequestError) || error.status !== 410) {
      throw error;
    }
  }

  try {
    return await getJob(reservedJobId);
  } catch (error) {
    if (error instanceof ApiRequestError && error.status === 410) {
      return null;
    }
    // In particular, never treat a second 404 as proof of absence. Retrying
    // the whole sequence is safe and keeps the UI locked until the server
    // exposes either the job or its durable tombstone.
    throw error;
  }
}

export interface TranslationJobController {
  job: TranslationJob | null;
  restoring: boolean;
  creating: boolean;
  actionError: string | null;
  streamState: StreamState;
  start: (file: File, draft: JobDraft) => Promise<void>;
  cancel: () => Promise<void>;
  clear: () => void;
}

export function useTranslationJob(csrfToken?: string | null): TranslationJobController {
  const [job, setJob] = useState<TranslationJob | null>(null);
  const [restoring, setRestoring] = useState(true);
  const [creating, setCreating] = useState(false);
  const [actionError, setActionError] = useState<string | null>(null);
  const [streamState, setStreamState] = useState<StreamState>("idle");
  const jobRef = useRef<TranslationJob | null>(null);
  const cancelRequestedRef = useRef<string | null>(null);
  const operationRef = useRef(0);
  const creatingRef = useRef(false);
  const pendingJobIdRef = useRef<string | null>(null);
  const mountedRef = useRef(true);

  useEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
      // Stop any pending-create recovery loop without clearing its persisted ID.
      // A later mount can resume recovery from sessionStorage.
      operationRef.current += 1;
    };
  }, []);

  const commitJob = useCallback(
    (
      next:
        | TranslationJob
        | null
        | ((previous: TranslationJob | null) => TranslationJob | null),
    ) => {
      const value = typeof next === "function" ? next(jobRef.current) : next;
      jobRef.current = value;
      persistActiveJobId(value);
      setJob(value);
    },
    [],
  );

  const applyUpdate = useCallback(
    (raw: unknown, expectedId: string) => {
      commitJob((previous) =>
        mergeJobUpdate(
          previous,
          raw,
          expectedId,
          cancelRequestedRef.current === expectedId,
        ),
      );
    },
    [commitJob],
  );

  useEffect(() => {
    const storedJobId = readActiveJobId();
    if (!storedJobId) {
      pendingJobIdRef.current = null;
      setRestoring(false);
      return;
    }
    // Restoration is also an unresolved ownership lookup. Keep start() locked
    // until this exact persisted ID is found or definitively absent.
    pendingJobIdRef.current = storedJobId;

    let disposed = false;
    let retryTimer: number | null = null;
    const restore = async () => {
      try {
        const restoredJob = await resolveReservedJob(storedJobId);
        if (!disposed) {
          if (pendingJobIdRef.current === storedJobId) {
            pendingJobIdRef.current = null;
          }
          if (restoredJob) {
            applyUpdate(restoredJob, storedJobId);
            setActionError(null);
          } else {
            persistActiveJobId(null);
            setActionError("上次任务已过期，已为你清理恢复记录。");
          }
          setRestoring(false);
        }
      } catch (error) {
        if (disposed) {
          return;
        }
        setActionError("暂时无法恢复上次任务，正在等待服务重新连接…");
        retryTimer = window.setTimeout(
          () => void restore(),
          RESTORE_RETRY_INTERVAL_MS,
        );
      }
    };
    void restore();
    return () => {
      disposed = true;
      if (retryTimer !== null) {
        window.clearTimeout(retryTimer);
      }
    };
  }, [applyUpdate]);

  useEffect(() => {
    if (!job || !ACTIVE_JOB_STATES.has(job.status)) {
      setStreamState("idle");
      return;
    }

    const jobId = job.id;
    let disposed = false;
    let source: EventSource | null = null;

    if (typeof EventSource !== "undefined") {
      setStreamState("connecting");
      source = new EventSource(eventStreamUrl(jobId), { withCredentials: true });
      source.onopen = () => {
        if (!disposed) {
          setStreamState("live");
        }
      };
      const handleMessage = (event: MessageEvent<string>) => {
        if (!disposed) {
          applyUpdate(parseEvent(event), jobId);
        }
      };
      source.onmessage = handleMessage;
      // The public contract uses ordinary data messages. Keep compatibility
      // with deployments that emitted the same payload as a named `job` event.
      source.addEventListener("job", handleMessage as EventListener);
      source.onerror = () => {
        if (!disposed) {
          setStreamState("reconnecting");
        }
      };
    }

    const poll = () => {
      void getJob(jobId)
        .then((latest) => {
          if (!disposed) {
            applyUpdate(latest, jobId);
          }
        })
        .catch(() => {
          // SSE reconnect and the next poll remain available.
        });
    };
    const pollTimer = window.setInterval(poll, STATUS_POLL_INTERVAL_MS);

    return () => {
      disposed = true;
      if (source?.onmessage) {
        source.removeEventListener("job", source.onmessage as EventListener);
      }
      source?.close();
      window.clearInterval(pollTimer);
    };
  }, [applyUpdate, job?.id, job?.status]);

  useEffect(() => {
    if (!job) {
      return;
    }
    const jobId = job.id;
    const beat = () => {
      void heartbeatJob(jobId).catch(() => {
        // A missed heartbeat is recoverable. Terminal pages keep renewing too,
        // so a late release from an older document cannot delete their files.
      });
    };
    beat();
    const heartbeatTimer = window.setInterval(beat, HEARTBEAT_INTERVAL_MS);
    return () => window.clearInterval(heartbeatTimer);
  }, [job?.id, job?.status]);

  useEffect(() => {
    const handlePageHide = (event: PageTransitionEvent) => {
      if (event.persisted) {
        // BFCache keeps this document alive; pageshow will resume its lease.
        return;
      }
      const current = jobRef.current;
      const jobId = pendingJobIdRef.current ?? current?.id ?? readActiveJobId();
      if (jobId) {
        sendJobBeacon(jobId, "release");
      }
    };
    const handlePageShow = () => {
      const current = jobRef.current;
      const jobId = pendingJobIdRef.current ?? current?.id ?? readActiveJobId();
      if (jobId) {
        // Heartbeat also revokes a terminal job's pagehide grace release.
        sendJobBeacon(jobId, "heartbeat");
      }
    };
    window.addEventListener("pagehide", handlePageHide);
    window.addEventListener("pageshow", handlePageShow);
    return () => {
      window.removeEventListener("pagehide", handlePageHide);
      window.removeEventListener("pageshow", handlePageShow);
    };
  }, []);

  const start = useCallback(
    async (file: File, draft: JobDraft) => {
      const current = jobRef.current;
      if (
        creatingRef.current ||
        pendingJobIdRef.current !== null ||
        (current && ACTIVE_JOB_STATES.has(current.status))
      ) {
        return;
      }
      creatingRef.current = true;
      const operation = ++operationRef.current;
      setCreating(true);
      setActionError(null);
      cancelRequestedRef.current = null;
      const reservedJobId = createClientJobId();
      pendingJobIdRef.current = reservedJobId;
      persistActiveJobIdValue(reservedJobId);

      const isCurrentAttempt = () =>
        mountedRef.current &&
        operationRef.current === operation &&
        pendingJobIdRef.current === reservedJobId;

      const adoptReservedJob = (confirmedJob: TranslationJob): boolean => {
        if (!isCurrentAttempt() || confirmedJob.id !== reservedJobId) {
          return false;
        }
        pendingJobIdRef.current = null;
        if (current && current.id !== reservedJobId) {
          void releaseJob(current.id).catch(() => {
            // Server TTL remains the fallback when an old terminal job cannot release.
          });
        }
        commitJob(confirmedJob);
        setActionError(null);
        return true;
      };

      try {
        const created = await createJob(file, draft, reservedJobId, csrfToken);
        if (!isCurrentAttempt()) {
          return;
        }
        if (created.id !== reservedJobId) {
          throw new Error("服务返回了不一致的任务编号");
        }
        const shell: TranslationJob = {
          id: reservedJobId,
          status: created.status,
          progress: 0,
          stage: "已提交",
          message: "正在为任务分配资源…",
        };
        if (!adoptReservedJob(shell)) {
          return;
        }
        try {
          const detail = await getJob(reservedJobId);
          if (operationRef.current === operation) {
            applyUpdate(detail, reservedJobId);
          }
        } catch {
          // The newly-created shell can continue through SSE even if this GET races.
        }
      } catch (error) {
        const originalMessage =
          error instanceof Error ? error.message : "创建翻译任务未确认";
        if (isCurrentAttempt()) {
          setActionError(
            `${originalMessage}；正在使用原任务编号确认服务端状态…`,
          );
        }

        // The POST response can be lost while the server is still before its
        // reservation gate. A plain GET 404 cannot unlock this UUID: first
        // linearize it through release/tombstone, then require either a job or
        // HTTP 410 before allowing a new submission (which gets a fresh UUID).
        while (isCurrentAttempt()) {
          try {
            const detail = await resolveReservedJob(reservedJobId);
            if (!detail) {
              pendingJobIdRef.current = null;
              persistActiveJobId(current);
              setActionError(
                `${originalMessage}；服务已安全退休原任务编号，可以重新提交。`,
              );
              break;
            }
            if (detail.id !== reservedJobId) {
              throw new Error("服务返回了不一致的任务编号");
            }
            adoptReservedJob(detail);
            break;
          } catch {
            if (!isCurrentAttempt()) {
              break;
            }
          }

          await new Promise<void>((resolve) =>
            window.setTimeout(resolve, RESTORE_RETRY_INTERVAL_MS),
          );
        }
      } finally {
        if (
          mountedRef.current &&
          operationRef.current === operation &&
          pendingJobIdRef.current !== reservedJobId
        ) {
          creatingRef.current = false;
          setCreating(false);
        }
      }
    },
    [applyUpdate, commitJob, csrfToken],
  );

  const cancel = useCallback(async () => {
    const current = jobRef.current;
    if (
      !current ||
      (current.status !== "queued" && current.status !== "running")
    ) {
      return;
    }
    const jobId = current.id;
    if (cancelRequestedRef.current === jobId) {
      return;
    }
    cancelRequestedRef.current = jobId;
    setActionError(null);
    commitJob((previous) =>
      previous?.id === jobId
        ? {
            ...previous,
            status: "cancelling",
            message: "正在停止子进程并回收资源…",
          }
        : previous,
    );
    try {
      const response = await cancelJob(jobId);
      if (response) {
        applyUpdate(response, jobId);
      }
    } catch (error) {
      setActionError(
        error instanceof Error
          ? `取消请求未确认：${error.message}`
          : "取消请求未确认。",
      );
      // Do not strand the UI in an irreversible optimistic state when the
      // request never reached the server. Reconcile, then allow a safe retry.
      cancelRequestedRef.current = null;
      try {
        applyUpdate(await getJob(jobId), jobId);
      } catch {
        commitJob((previous) =>
          previous?.id === jobId
            ? {
                ...previous,
                status: current.status,
                message: "取消请求未确认，请重试。",
              }
            : previous,
        );
      }
    }
  }, [applyUpdate, commitJob]);

  const clear = useCallback(() => {
    const current = jobRef.current;
    if (
      creatingRef.current ||
      pendingJobIdRef.current !== null ||
      (current && !TERMINAL_JOB_STATES.has(current.status))
    ) {
      return;
    }
    cancelRequestedRef.current = null;
    setActionError(null);
    if (current) {
      void deleteJob(current.id).catch(() => {
        // The backend's terminal-job TTL remains the deletion fallback.
      });
    }
    commitJob(null);
  }, [commitJob]);

  return {
    job,
    restoring,
    creating,
    actionError,
    streamState,
    start,
    cancel,
    clear,
  };
}
