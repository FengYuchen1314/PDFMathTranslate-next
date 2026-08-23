import {
  act,
  fireEvent,
  render,
  renderHook,
  screen,
  waitFor,
} from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { StrictMode } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import App from "./App";
import type { JobDraft } from "./types";
import { mergeJobUpdate, useTranslationJob } from "./useTranslationJob";

const CONFIG = {
  provider: "google",
  maxUploadBytes: 10_000_000,
  languages: [
    { code: "en", name: "英语" },
    { code: "zh", name: "简体中文" },
  ],
};

const TEST_DRAFT: JobDraft = {
  sourceLang: "en",
  targetLang: "zh",
  pages: "",
  outputMono: true,
  outputDual: true,
};

function testPdf(): File {
  return new File(["pdf"], "paper.pdf", { type: "application/pdf" });
}

function reservedJobId(init?: RequestInit): string {
  const jobId = new Headers(init?.headers).get("x-job-id");
  if (!jobId) {
    throw new Error("Missing x-job-id in create request");
  }
  return jobId;
}

async function flushAsyncWork(): Promise<void> {
  await act(async () => {
    for (let index = 0; index < 12; index += 1) {
      await Promise.resolve();
    }
  });
}

class FakeEventSource {
  static instances: FakeEventSource[] = [];
  readonly url: string;
  onopen: ((event: Event) => void) | null = null;
  onmessage: ((event: MessageEvent<string>) => void) | null = null;
  onerror: ((event: Event) => void) | null = null;
  close = vi.fn();
  private readonly jobListeners = new Set<(event: MessageEvent<string>) => void>();

  constructor(url: string) {
    this.url = url;
    FakeEventSource.instances.push(this);
  }

  addEventListener(
    type: string,
    listener: EventListenerOrEventListenerObject | null,
  ) {
    if (type === "job" && typeof listener === "function") {
      this.jobListeners.add(listener as (event: MessageEvent<string>) => void);
    }
  }

  removeEventListener(
    type: string,
    listener: EventListenerOrEventListenerObject | null,
  ) {
    if (type === "job" && typeof listener === "function") {
      this.jobListeners.delete(listener as (event: MessageEvent<string>) => void);
    }
  }

  emit(data: unknown, type: "message" | "job" = "message") {
    const event = new MessageEvent<string>(type, {
      data: JSON.stringify(data),
    });
    if (type === "job") {
      this.jobListeners.forEach((listener) => listener(event));
    } else {
      this.onmessage?.(event);
    }
  }
}

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json" },
  });
}

function emptyResponse(status = 204): Response {
  return new Response(null, { status });
}

function installFetch(job: Record<string, unknown>) {
  const fetchMock = vi.fn(
    async (input: RequestInfo | URL, init?: RequestInit): Promise<Response> => {
      const url = String(input);
      if (url === "/api/health") return jsonResponse({ ok: true });
      if (url === "/api/config") return jsonResponse(CONFIG);
      if (url.endsWith("/heartbeat")) return emptyResponse();
      if (url.endsWith("/cancel") && init?.method === "POST") {
        return jsonResponse({ ...job, status: "cancelling" });
      }
      if (url === `/api/jobs/${job.id}` && init?.method === "DELETE") {
        return emptyResponse();
      }
      if (url === `/api/jobs/${job.id}`) return jsonResponse(job);
      throw new Error(`Unexpected request: ${init?.method ?? "GET"} ${url}`);
    },
  );
  vi.stubGlobal("fetch", fetchMock);
  return fetchMock;
}

describe("PDF translation workbench", () => {
  beforeEach(() => {
    FakeEventSource.instances = [];
    vi.stubGlobal("EventSource", FakeEventSource);
  });

  afterEach(() => {
    vi.useRealTimers();
    vi.unstubAllGlobals();
  });

  it("restores a running job and renders live SSE progress", async () => {
    window.sessionStorage.setItem("pdf2zh.active-job-id", "job-running");
    installFetch({
      id: "job-running",
      status: "running",
      progress: 21,
      stage: "解析页面",
      message: "正在识别文档结构",
    });

    render(
      <StrictMode>
        <App />
      </StrictMode>,
    );

    expect(await screen.findByText("正在处理文档")).toBeInTheDocument();
    expect(screen.getByRole("progressbar")).toHaveAttribute("aria-valuenow", "21");
    expect(FakeEventSource.instances).toHaveLength(1);

    FakeEventSource.instances[0].emit(
      {
        id: "job-running",
        status: "running",
        progress: 73,
        stage: "翻译段落",
        message: "已完成 73%",
      },
      "job",
    );

    await waitFor(() =>
      expect(screen.getByRole("progressbar")).toHaveAttribute(
        "aria-valuenow",
        "73",
      ),
    );
    expect(screen.getByText("翻译段落")).toBeInTheDocument();
  });

  it("optimistically enters cancelling state and calls the cancel endpoint", async () => {
    const user = userEvent.setup();
    window.sessionStorage.setItem("pdf2zh.active-job-id", "job-cancel");
    const fetchMock = installFetch({
      id: "job-cancel",
      status: "running",
      progress: 48,
      stage: "翻译中",
    });
    render(<App />);

    await user.click(await screen.findByRole("button", { name: "取消任务" }));

    expect(await screen.findByText("正在安全停止任务")).toBeInTheDocument();
    expect(
      fetchMock.mock.calls.some(
        ([url, init]) =>
          String(url) === "/api/jobs/job-cancel/cancel" &&
          (init as RequestInit | undefined)?.method === "POST",
      ),
    ).toBe(true);
  });

  it("retries a failed job with the newly selected PDF", async () => {
    const user = userEvent.setup();
    window.sessionStorage.setItem("pdf2zh.active-job-id", "job-failed");
    const failedJob = {
      id: "job-failed",
      status: "failed",
      progress: 37,
      stage: "翻译失败",
      error: "Google 请求超时",
    };
    let created = false;
    let createdId: string | null = null;
    const fetchMock = vi.fn(
      async (input: RequestInfo | URL, init?: RequestInit): Promise<Response> => {
        const url = String(input);
        if (url === "/api/health") return jsonResponse({ ok: true });
        if (url === "/api/config") return jsonResponse(CONFIG);
        if (url === "/api/jobs/job-failed") return jsonResponse(failedJob);
        if (url === "/api/jobs" && init?.method === "POST") {
          created = true;
          createdId = reservedJobId(init);
          return jsonResponse({ id: createdId, status: "queued" });
        }
        if (url === "/api/jobs/job-failed/release") return emptyResponse();
        if (createdId && url === `/api/jobs/${createdId}`) {
          return jsonResponse({
            id: createdId,
            status: "running",
            progress: 2,
            stage: "正在准备",
          });
        }
        if (url.endsWith("/heartbeat")) return emptyResponse();
        throw new Error(`Unexpected request: ${init?.method ?? "GET"} ${url}`);
      },
    );
    vi.stubGlobal("fetch", fetchMock);
    render(<App />);

    expect(await screen.findByText("这次翻译没有完成")).toBeInTheDocument();
    const fileInput = screen.getByLabelText("选择 PDF 文件") as HTMLInputElement;
    const pdf = new File(["pdf"], "paper.pdf", { type: "application/pdf" });
    await user.upload(fileInput, pdf);
    await user.click(screen.getByRole("button", { name: "重新提交" }));

    await waitFor(() => expect(created).toBe(true));
    const createCall = fetchMock.mock.calls.find(
      ([url, init]) =>
        String(url) === "/api/jobs" &&
        (init as RequestInit | undefined)?.method === "POST",
    );
    expect(createCall).toBeDefined();
    expect((createCall?.[1] as RequestInit).body).toBeInstanceOf(FormData);
    expect(
      ((createCall?.[1] as RequestInit).headers as Record<string, string>)[
        "x-job-id"
      ],
    ).toMatch(/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/);
    expect(await screen.findByText("正在处理文档")).toBeInTheDocument();
  });

  it("releases an active job with a beacon on pagehide", async () => {
    window.sessionStorage.setItem("pdf2zh.active-job-id", "job-pagehide");
    installFetch({
      id: "job-pagehide",
      status: "running",
      progress: 8,
      stage: "准备中",
    });
    const sendBeacon = vi.fn(() => true);
    Object.defineProperty(navigator, "sendBeacon", {
      configurable: true,
      value: sendBeacon,
    });
    render(<App />);
    await screen.findByText("正在处理文档");

    const event = new Event("pagehide") as PageTransitionEvent;
    Object.defineProperty(event, "persisted", { value: false });
    fireEvent(window, event);

    expect(sendBeacon).toHaveBeenCalledWith(
      "/api/jobs/job-pagehide/release",
    );

    fireEvent(window, new Event("pageshow"));
    expect(sendBeacon).toHaveBeenLastCalledWith(
      "/api/jobs/job-pagehide/heartbeat",
    );
  });

  it("does not release a job when pagehide enters the back-forward cache", async () => {
    window.sessionStorage.setItem("pdf2zh.active-job-id", "job-bfcache");
    installFetch({
      id: "job-bfcache",
      status: "running",
      progress: 18,
      stage: "翻译中",
    });
    const sendBeacon = vi.fn(() => true);
    Object.defineProperty(navigator, "sendBeacon", {
      configurable: true,
      value: sendBeacon,
    });
    render(<App />);
    await screen.findByText("正在处理文档");

    const event = new Event("pagehide") as PageTransitionEvent;
    Object.defineProperty(event, "persisted", { value: true });
    fireEvent(window, event);

    expect(sendBeacon).not.toHaveBeenCalledWith(
      "/api/jobs/job-bfcache/release",
    );
  });

  it("heartbeats a terminal job on pageshow to revoke release grace", async () => {
    window.sessionStorage.setItem("pdf2zh.active-job-id", "job-terminal-page");
    installFetch({
      id: "job-terminal-page",
      status: "succeeded",
      progress: 100,
      stage: "已完成",
      files: { mono: true, dual: true },
    });
    const sendBeacon = vi.fn(() => true);
    Object.defineProperty(navigator, "sendBeacon", {
      configurable: true,
      value: sendBeacon,
    });
    render(<App />);
    await screen.findByText("译稿已准备好");

    const event = new Event("pagehide") as PageTransitionEvent;
    Object.defineProperty(event, "persisted", { value: false });
    fireEvent(window, event);
    expect(sendBeacon).toHaveBeenCalledWith(
      "/api/jobs/job-terminal-page/release",
    );

    fireEvent(window, new Event("pageshow"));
    expect(sendBeacon).toHaveBeenLastCalledWith(
      "/api/jobs/job-terminal-page/heartbeat",
    );
  });

  it("continues periodic heartbeats while a terminal result page stays open", async () => {
    vi.useFakeTimers();
    window.sessionStorage.setItem("pdf2zh.active-job-id", "job-terminal-lease");
    const fetchMock = installFetch({
      id: "job-terminal-lease",
      status: "succeeded",
      progress: 100,
      stage: "已完成",
      files: { mono: true },
    });
    renderHook(() => useTranslationJob());
    await flushAsyncWork();

    const heartbeatCount = () =>
      fetchMock.mock.calls.filter(
        ([url, init]) =>
          String(url) === "/api/jobs/job-terminal-lease/heartbeat" &&
          (init as RequestInit | undefined)?.method === "POST",
      ).length;

    expect(heartbeatCount()).toBe(1);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(9_999);
    });
    await flushAsyncWork();
    expect(heartbeatCount()).toBe(1);

    await act(async () => {
      await vi.advanceTimersByTimeAsync(1);
    });
    await flushAsyncWork();
    expect(heartbeatCount()).toBe(2);
  });

  it("deletes a terminal job when the workbench is cleared", async () => {
    const user = userEvent.setup();
    window.sessionStorage.setItem("pdf2zh.active-job-id", "job-delete");
    const fetchMock = installFetch({
      id: "job-delete",
      status: "succeeded",
      progress: 100,
      stage: "已完成",
      files: { mono: true },
    });
    render(<App />);

    await user.click(
      await screen.findByRole("button", { name: "翻译另一份" }),
    );

    await waitFor(() =>
      expect(
        fetchMock.mock.calls.some(
          ([url, init]) =>
            String(url) === "/api/jobs/job-delete" &&
            (init as RequestInit | undefined)?.method === "DELETE",
        ),
      ).toBe(true),
    );
    expect(
      fetchMock.mock.calls.some(
        ([url]) => String(url) === "/api/jobs/job-delete/release",
      ),
    ).toBe(false);
    expect(window.sessionStorage.getItem("pdf2zh.active-job-id")).toBeNull();
  });

  it("keeps one reserved ID locked while a 409 upload later becomes a job", async () => {
    vi.useFakeTimers();
    let jobId = "";
    let createCount = 0;
    let lookupCount = 0;
    const fetchMock = vi.fn(
      async (input: RequestInfo | URL, init?: RequestInit): Promise<Response> => {
        const url = String(input);
        if (url.endsWith("/heartbeat")) return emptyResponse();
        if (url === "/api/jobs" && init?.method === "POST") {
          createCount += 1;
          jobId = reservedJobId(init);
          throw new TypeError("创建响应连接中断");
        }
        if (jobId && url === `/api/jobs/${jobId}`) {
          lookupCount += 1;
          if (lookupCount === 1) {
            return jsonResponse({ error: "任务仍在上传" }, 409);
          }
          return jsonResponse({
            id: jobId,
            status: "running",
            progress: 3,
            stage: "解析页面",
          });
        }
        throw new Error(`Unexpected request: ${init?.method ?? "GET"} ${url}`);
      },
    );
    vi.stubGlobal("fetch", fetchMock);
    const { result } = renderHook(() => useTranslationJob());

    let startPromise!: Promise<void>;
    act(() => {
      startPromise = result.current.start(testPdf(), TEST_DRAFT);
    });
    await flushAsyncWork();

    expect(createCount).toBe(1);
    expect(lookupCount).toBe(1);
    expect(result.current.creating).toBe(true);
    expect(result.current.job).toBeNull();
    expect(window.sessionStorage.getItem("pdf2zh.active-job-id")).toBe(jobId);

    act(() => {
      void result.current.start(testPdf(), TEST_DRAFT);
    });
    await flushAsyncWork();
    expect(createCount).toBe(1);

    await act(async () => {
      await vi.advanceTimersByTimeAsync(4_000);
    });
    await flushAsyncWork();
    await act(async () => {
      await startPromise;
    });

    expect(lookupCount).toBe(2);
    expect(result.current.creating).toBe(false);
    expect(result.current.job).toMatchObject({ id: jobId, status: "running" });
  });

  it("keeps the reserved ID through a recovery network error", async () => {
    vi.useFakeTimers();
    let jobId = "";
    let createCount = 0;
    let lookupCount = 0;
    const fetchMock = vi.fn(
      async (input: RequestInfo | URL, init?: RequestInit): Promise<Response> => {
        const url = String(input);
        if (url.endsWith("/heartbeat")) return emptyResponse();
        if (url === "/api/jobs" && init?.method === "POST") {
          createCount += 1;
          jobId = reservedJobId(init);
          throw new TypeError("创建响应连接中断");
        }
        if (jobId && url === `/api/jobs/${jobId}`) {
          lookupCount += 1;
          if (lookupCount === 1) {
            throw new TypeError("恢复查询暂时离线");
          }
          return jsonResponse({
            id: jobId,
            status: "queued",
            progress: 0,
            stage: "等待调度",
          });
        }
        throw new Error(`Unexpected request: ${init?.method ?? "GET"} ${url}`);
      },
    );
    vi.stubGlobal("fetch", fetchMock);
    const { result } = renderHook(() => useTranslationJob());

    let startPromise!: Promise<void>;
    act(() => {
      startPromise = result.current.start(testPdf(), TEST_DRAFT);
    });
    await flushAsyncWork();

    expect(createCount).toBe(1);
    expect(lookupCount).toBe(1);
    expect(result.current.creating).toBe(true);
    expect(window.sessionStorage.getItem("pdf2zh.active-job-id")).toBe(jobId);

    await act(async () => {
      await vi.advanceTimersByTimeAsync(4_000);
    });
    await flushAsyncWork();
    await act(async () => {
      await startPromise;
    });

    expect(createCount).toBe(1);
    expect(lookupCount).toBe(2);
    expect(result.current.creating).toBe(false);
    expect(result.current.job).toMatchObject({ id: jobId, status: "queued" });
  });

  it("unlocks a new submission only after a definitive 404", async () => {
    const requestedIds: string[] = [];
    const fetchMock = vi.fn(
      async (input: RequestInfo | URL, init?: RequestInit): Promise<Response> => {
        const url = String(input);
        if (url.endsWith("/heartbeat")) return emptyResponse();
        if (url === "/api/jobs" && init?.method === "POST") {
          const jobId = reservedJobId(init);
          requestedIds.push(jobId);
          if (requestedIds.length === 1) {
            throw new TypeError("创建响应连接中断");
          }
          return jsonResponse({ id: jobId, status: "queued" });
        }
        if (requestedIds[0] && url === `/api/jobs/${requestedIds[0]}`) {
          return jsonResponse({ error: "任务不存在" }, 404);
        }
        if (requestedIds[1] && url === `/api/jobs/${requestedIds[1]}`) {
          return jsonResponse({
            id: requestedIds[1],
            status: "running",
            progress: 1,
            stage: "正在准备",
          });
        }
        throw new Error(`Unexpected request: ${init?.method ?? "GET"} ${url}`);
      },
    );
    vi.stubGlobal("fetch", fetchMock);
    const { result } = renderHook(() => useTranslationJob());

    let firstStart!: Promise<void>;
    act(() => {
      firstStart = result.current.start(testPdf(), TEST_DRAFT);
    });
    await act(async () => {
      await firstStart;
    });

    expect(requestedIds).toHaveLength(1);
    expect(result.current.creating).toBe(false);
    expect(result.current.job).toBeNull();
    expect(result.current.actionError).toContain("可以重新提交");
    expect(window.sessionStorage.getItem("pdf2zh.active-job-id")).toBeNull();

    let secondStart!: Promise<void>;
    act(() => {
      secondStart = result.current.start(testPdf(), TEST_DRAFT);
    });
    await act(async () => {
      await secondStart;
    });

    expect(requestedIds).toHaveLength(2);
    expect(requestedIds[1]).not.toBe(requestedIds[0]);
    expect(result.current.creating).toBe(false);
    expect(result.current.job).toMatchObject({
      id: requestedIds[1],
      status: "running",
    });
  });

  it("ignores a late event from an earlier task", () => {
    const current = {
      id: "job-new",
      status: "running" as const,
      progress: 12,
      stage: "新任务",
    };

    expect(
      mergeJobUpdate(
        current,
        { id: "job-old", status: "succeeded", progress: 100 },
        "job-old",
      ),
    ).toEqual(current);
  });

  it("does not let a stale active snapshot reopen a terminal task", () => {
    const terminal = {
      id: "job-terminal",
      status: "cancelled" as const,
      revision: 9,
      progress: 41,
      message: "任务已取消",
    };

    expect(
      mergeJobUpdate(
        terminal,
        {
          id: "job-terminal",
          status: "running",
          revision: 8,
          progress: 40,
        },
        "job-terminal",
        true,
      ),
    ).toEqual(terminal);
  });

  it("keeps Rust API progress values in the documented 0–100 scale", () => {
    expect(
      mergeJobUpdate(
        { id: "job-progress", status: "running", progress: 0 },
        { id: "job-progress", status: "running", progress: 1 },
        "job-progress",
      )?.progress,
    ).toBe(1);
  });
});
