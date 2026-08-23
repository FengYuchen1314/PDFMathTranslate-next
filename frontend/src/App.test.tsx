import { act, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import App from "./App";
import type { AdminProvider, AdminSession, AppConfig } from "./types";

const PUBLIC_CONFIG: AppConfig = {
  provider: "google",
  providers: [
    {
      id: "google",
      kind: "google",
      name: "Google",
      available: true,
      models: [],
    },
    {
      id: "deepseek",
      kind: "deepseek",
      name: "DeepSeek",
      available: true,
      models: [
        { id: "deepseek-v4-flash", name: "DeepSeek V4 Flash", isDefault: true },
        { id: "deepseek-v4-pro", name: "DeepSeek V4 Pro" },
      ],
      thinkingModes: ["default", "enabled", "disabled"],
      reasoningEfforts: ["low", "high", "max"],
      defaultReasoningEffort: "high",
    },
  ],
  maxUploadBytes: 10_000_000,
  languages: [
    { code: "en", name: "英语" },
    { code: "zh", name: "简体中文" },
  ],
};

const AUTHENTICATED_SESSION: AdminSession = {
  setupRequired: false,
  setupTokenRequired: false,
  authenticated: true,
  username: "admin",
  csrfToken: "csrf-test-token",
};

const DEEPSEEK_PROVIDER: AdminProvider = {
  id: "deepseek",
  kind: "deepseek",
  name: "DeepSeek",
  baseUrl: "https://api.deepseek.com",
  enabled: false,
  hasApiKey: false,
  revision: 3,
  models: [
    { id: "deepseek-v4-flash", name: "DeepSeek V4 Flash", isDefault: true },
    { id: "deepseek-v4-pro", name: "DeepSeek V4 Pro", isDefault: false },
  ],
};

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

  emit(data: unknown) {
    this.onmessage?.(
      new MessageEvent<string>("message", { data: JSON.stringify(data) }),
    );
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

function requestPath(input: RequestInfo | URL): string {
  return new URL(String(input), "http://localhost").pathname;
}

function testPdf(): File {
  return new File(["pdf"], "paper.pdf", { type: "application/pdf" });
}

describe("practical translation page", () => {
  beforeEach(() => {
    window.history.replaceState({}, "", "/");
    FakeEventSource.instances = [];
    vi.stubGlobal("EventSource", FakeEventSource);
    Object.defineProperty(navigator, "sendBeacon", {
      configurable: true,
      value: vi.fn(() => true),
    });
  });

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("renders a simple accessible settings form without the old decorative shell", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL) => {
        if (requestPath(input) === "/api/config") return jsonResponse(PUBLIC_CONFIG);
        throw new Error(`Unexpected request: ${String(input)}`);
      }),
    );

    render(<App />);

    expect(await screen.findByRole("heading", { name: "PDF 翻译" })).toBeInTheDocument();
    expect(screen.getByLabelText("PDF 文件")).toHaveAttribute("type", "file");
    expect(screen.getByRole("checkbox", { name: "纯译文" })).toBeChecked();
    expect(screen.getByRole("checkbox", { name: "双语对照" })).toBeChecked();
    expect(screen.getByRole("link", { name: "管理后台" })).toHaveAttribute("href", "/admin");
    expect(document.querySelector(".ambient")).not.toBeInTheDocument();
    expect(screen.queryByText("让论文更容易阅读")).not.toBeInTheDocument();
  });

  it("does not pretend Google is available when public config cannot load", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => {
        throw new Error("offline");
      }),
    );

    render(<App />);

    expect(await screen.findByRole("alert")).toHaveTextContent("无法读取服务配置");
    expect(screen.getByRole("button", { name: "开始翻译" })).toBeDisabled();
    expect(screen.queryByRole("option", { name: "Google" })).not.toBeInTheDocument();
  });

  it("restores, cancels and releases an active job without changing its lifecycle", async () => {
    const runningJob = {
      id: "job-running",
      status: "running",
      progress: 37,
      stage: "翻译正文",
      message: "正在调用翻译服务",
      options: {
        providerId: "deepseek",
        modelId: "deepseek-v4-pro",
      },
    };
    window.sessionStorage.setItem("pdf2zh.active-job-id", runningJob.id);
    const fetchMock = vi.fn(
      async (input: RequestInfo | URL, init?: RequestInit): Promise<Response> => {
        const path = requestPath(input);
        if (path === "/api/config") return jsonResponse(PUBLIC_CONFIG);
        if (path === `/api/jobs/${runningJob.id}/heartbeat`) return emptyResponse();
        if (path === `/api/jobs/${runningJob.id}/cancel`) {
          return jsonResponse({ ...runningJob, status: "cancelling" });
        }
        if (path === `/api/jobs/${runningJob.id}`) return jsonResponse(runningJob);
        throw new Error(`Unexpected request: ${init?.method ?? "GET"} ${path}`);
      },
    );
    vi.stubGlobal("fetch", fetchMock);

    render(<App />);

    const progress = await screen.findByRole("progressbar", { name: "翻译进度" });
    expect(progress.tagName).toBe("PROGRESS");
    expect(progress).toHaveAttribute("value", "37");
    expect(screen.getByText("DeepSeek / DeepSeek V4 Pro")).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "取消任务" }));
    expect(await screen.findByText("正在取消", { selector: ".status" })).toBeInTheDocument();
    expect(fetchMock).toHaveBeenCalledWith(
      "/api/jobs/job-running/cancel",
      expect.objectContaining({ method: "POST" }),
    );

    act(() => {
      window.dispatchEvent(new PageTransitionEvent("pagehide", { persisted: false }));
    });
    expect(navigator.sendBeacon).toHaveBeenCalledWith(
      "/api/jobs/job-running/release",
    );
  });

  it("submits only dynamic provider/model IDs and never public credentials", async () => {
    const user = userEvent.setup();
    const customConfig: AppConfig = {
      ...PUBLIC_CONFIG,
      csrfToken: "csrf-test-token",
      providers: [
        PUBLIC_CONFIG.providers[0],
        {
          id: "lab-channel",
          kind: "openai_compatible",
          name: "实验室模型",
          available: true,
          models: [
            { id: "model-a", name: "模型 A", isDefault: true },
            { id: "model-b", name: "模型 B" },
          ],
        },
      ],
    };
    let submittedForm: FormData | null = null;
    const fetchMock = vi.fn(
      async (input: RequestInfo | URL, init?: RequestInit): Promise<Response> => {
        const path = requestPath(input);
        if (path === "/api/config") return jsonResponse(customConfig);
        if (path === "/api/jobs" && init?.method === "POST") {
          submittedForm = init.body as FormData;
          const id = new Headers(init.headers).get("x-job-id") ?? "job-new";
          return jsonResponse({ id, status: "queued" });
        }
        if (path.startsWith("/api/jobs/") && path.endsWith("/heartbeat")) {
          return emptyResponse();
        }
        if (path.startsWith("/api/jobs/")) {
          return jsonResponse({
            id: path.split("/").at(-1),
            status: "queued",
            progress: 0,
          });
        }
        throw new Error(`Unexpected request: ${init?.method ?? "GET"} ${path}`);
      },
    );
    vi.stubGlobal("fetch", fetchMock);

    render(<App />);
    await screen.findByRole("option", { name: "实验室模型" });
    await user.selectOptions(screen.getByLabelText("翻译渠道"), "lab-channel");
    await user.selectOptions(screen.getByLabelText("模型"), "model-b");
    await user.upload(screen.getByLabelText("PDF 文件"), testPdf());
    await user.click(screen.getByRole("button", { name: "开始翻译" }));

    await waitFor(() => expect(submittedForm).not.toBeNull());
    const submitCall = fetchMock.mock.calls.find(
      ([input]) => requestPath(input as RequestInfo | URL) === "/api/jobs",
    );
    const form = submitCall?.[1]?.body;
    expect(form).toBeInstanceOf(FormData);
    if (!(form instanceof FormData)) throw new Error("翻译请求缺少 FormData");
    expect(form.get("provider_id")).toBe("lab-channel");
    expect(form.get("model_id")).toBe("model-b");
    expect(form.has("provider")).toBe(false);
    expect(form.has("api_key")).toBe(false);
    expect(form.has("base_url")).toBe(false);
    expect(form.has("deepseek_model")).toBe(false);
    expect(new Headers(submitCall?.[1]?.headers).get("X-CSRF-Token")).toBe(
      "csrf-test-token",
    );
  });

  it("tombstones an ambiguous create before a delayed POST can become orphaned", async () => {
    const user = userEvent.setup();
    let reservedJobId = "";
    let tombstoned = false;
    let lateJobCreated = false;
    let releaseWorkerHealth: (() => void) | undefined;
    const workerHealthGate = new Promise<void>((resolve) => {
      releaseWorkerHealth = resolve;
    });
    let delayedServerPost: Promise<"accepted" | "gone"> | null = null;

    const fetchMock = vi.fn(
      async (input: RequestInfo | URL, init?: RequestInit): Promise<Response> => {
        const path = requestPath(input);
        const method = init?.method ?? "GET";
        if (path === "/api/config") return jsonResponse(PUBLIC_CONFIG);
        if (path === "/api/jobs" && method === "POST") {
          reservedJobId = new Headers(init?.headers).get("x-job-id") ?? "";
          delayedServerPost = workerHealthGate.then(() => {
            if (tombstoned) return "gone";
            lateJobCreated = true;
            return "accepted";
          });
          // The browser loses the response while the server-side handler is
          // still awaiting worker health and has not reserved this UUID yet.
          throw new TypeError("translation response lost");
        }
        if (path === `/api/jobs/${reservedJobId}/release` && method === "POST") {
          tombstoned = true;
          releaseWorkerHealth?.();
          return emptyResponse(202);
        }
        if (path === `/api/jobs/${reservedJobId}` && method === "GET") {
          if (lateJobCreated) {
            return jsonResponse({ id: reservedJobId, status: "queued" });
          }
          return jsonResponse(
            { error: tombstoned ? "retired" : "not found" },
            tombstoned ? 410 : 404,
          );
        }
        throw new Error(`Unexpected request: ${method} ${path}`);
      },
    );
    vi.stubGlobal("fetch", fetchMock);

    render(<App />);
    await screen.findByRole("option", { name: "Google" });
    await user.upload(screen.getByLabelText("PDF 文件"), testPdf());
    await user.click(screen.getByRole("button", { name: "开始翻译" }));

    expect(
      await screen.findByText(/服务已安全退休原任务编号，可以重新提交/),
    ).toBeInTheDocument();
    await waitFor(() =>
      expect(screen.getByRole("button", { name: "开始翻译" })).toBeEnabled(),
    );
    expect(reservedJobId).toMatch(/^[0-9a-f-]{36}$/);
    expect(window.sessionStorage.getItem("pdf2zh.active-job-id")).toBeNull();
    await expect(delayedServerPost).resolves.toBe("gone");
    expect(lateJobCreated).toBe(false);

    const jobRequests = fetchMock.mock.calls
      .map(([input, init]) => `${init?.method ?? "GET"} ${requestPath(input)}`)
      .filter(
        (request) =>
          request.startsWith("POST /api/jobs") ||
          request.startsWith("GET /api/jobs"),
      );
    expect(jobRequests).toEqual([
      "POST /api/jobs",
      `GET /api/jobs/${reservedJobId}`,
      `POST /api/jobs/${reservedJobId}/release`,
      `GET /api/jobs/${reservedJobId}`,
    ]);
  });

  it("migrates the previous DeepSeek draft without persisting a credential", async () => {
    window.localStorage.setItem(
      "pdf2zh.translation-draft",
      JSON.stringify({
        provider: "deepseek",
        deepseekModel: "deepseek-v4-pro",
        deepseekThinkingMode: "enabled",
        deepseekReasoningEffort: "max",
        sourceLang: "en",
        targetLang: "zh",
        outputMono: true,
        outputDual: false,
        apiKey: "must-not-survive",
      }),
    );
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL) => {
        if (requestPath(input) === "/api/config") return jsonResponse(PUBLIC_CONFIG);
        throw new Error(`Unexpected request: ${String(input)}`);
      }),
    );

    render(<App />);

    expect(await screen.findByLabelText("翻译渠道")).toHaveValue("deepseek");
    expect(screen.getByLabelText("模型")).toHaveValue("deepseek-v4-pro");
    const persisted = window.localStorage.getItem("pdf2zh.translation-draft") ?? "";
    expect(persisted).not.toContain("must-not-survive");
    expect(JSON.parse(persisted)).toMatchObject({
      providerId: "deepseek",
      modelId: "deepseek-v4-pro",
      thinkingMode: "enabled",
      reasoningEffort: "max",
    });
  });
});

describe("administrator page", () => {
  beforeEach(() => {
    window.history.replaceState({}, "", "/admin");
  });

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("shows the installation code only when required and creates the first admin", async () => {
    const user = userEvent.setup();
    let setupBody: Record<string, unknown> | null = null;
    const fetchMock = vi.fn(
      async (input: RequestInfo | URL, init?: RequestInit): Promise<Response> => {
        const path = requestPath(input);
        if (path === "/api/admin/session") {
          return jsonResponse({
            setupRequired: true,
            setupTokenRequired: true,
            authenticated: false,
          });
        }
        if (path === "/api/admin/setup" && init?.method === "POST") {
          setupBody = JSON.parse(String(init.body)) as Record<string, unknown>;
          return jsonResponse(AUTHENTICATED_SESSION);
        }
        if (path === "/api/admin/providers") {
          return jsonResponse({ providers: [DEEPSEEK_PROVIDER] });
        }
        throw new Error(`Unexpected request: ${init?.method ?? "GET"} ${path}`);
      },
    );
    vi.stubGlobal("fetch", fetchMock);

    render(<App />);

    expect(await screen.findByRole("heading", { name: "创建首位管理员" })).toBeInTheDocument();
    await user.type(screen.getByLabelText("安装码"), "install-once");
    await user.type(screen.getByLabelText("用户名"), "owner");
    await user.type(screen.getByLabelText("密码"), "a-secure-password");
    await user.type(screen.getByLabelText("确认密码"), "a-secure-password");
    await user.click(screen.getByRole("button", { name: "创建管理员" }));

    expect(await screen.findByRole("heading", { name: "翻译渠道" })).toBeInTheDocument();
    expect(setupBody).toEqual({
      setupToken: "install-once",
      username: "owner",
      password: "a-secure-password",
    });
    expect(window.localStorage.getItem("install-once")).toBeNull();
    expect(screen.queryByDisplayValue("install-once")).not.toBeInTheDocument();
  });

  it("logs in with an HttpOnly-cookie flow and sends CSRF on logout", async () => {
    const user = userEvent.setup();
    let authenticated = false;
    const fetchMock = vi.fn(
      async (input: RequestInfo | URL, init?: RequestInit): Promise<Response> => {
        const path = requestPath(input);
        if (path === "/api/admin/session") {
          return jsonResponse({
            setupRequired: false,
            setupTokenRequired: false,
            authenticated: false,
          });
        }
        if (path === "/api/admin/login" && init?.method === "POST") {
          authenticated = true;
          return jsonResponse(AUTHENTICATED_SESSION);
        }
        if (path === "/api/admin/providers" && authenticated) {
          return jsonResponse({ providers: [DEEPSEEK_PROVIDER] });
        }
        if (path === "/api/admin/logout" && init?.method === "POST") {
          authenticated = false;
          return emptyResponse();
        }
        throw new Error(`Unexpected request: ${init?.method ?? "GET"} ${path}`);
      },
    );
    vi.stubGlobal("fetch", fetchMock);

    render(<App />);
    expect(await screen.findByRole("heading", { name: "管理员登录" })).toBeInTheDocument();
    expect(screen.queryByLabelText("安装码")).not.toBeInTheDocument();
    await user.type(screen.getByLabelText("用户名"), "admin");
    await user.type(screen.getByLabelText("密码"), "a-secure-password");
    await user.click(screen.getByRole("button", { name: "登录" }));
    await user.click(await screen.findByRole("button", { name: "退出登录" }));

    expect(await screen.findByRole("heading", { name: "管理员登录" })).toBeInTheDocument();
    const logoutCall = fetchMock.mock.calls.find(
      ([input]) => requestPath(input as RequestInfo | URL) === "/api/admin/logout",
    );
    expect(new Headers(logoutCall?.[1]?.headers).get("X-CSRF-Token")).toBe(
      "csrf-test-token",
    );
  });

  it("stores a DeepSeek key only in the write request and clears the input immediately", async () => {
    const user = userEvent.setup();
    let savedProvider = DEEPSEEK_PROVIDER;
    let savedBody: Record<string, unknown> | null = null;
    const fetchMock = vi.fn(
      async (input: RequestInfo | URL, init?: RequestInit): Promise<Response> => {
        const path = requestPath(input);
        if (path === "/api/admin/session") return jsonResponse(AUTHENTICATED_SESSION);
        if (path === "/api/admin/providers" && (!init?.method || init.method === "GET")) {
          return jsonResponse({ providers: [savedProvider] });
        }
        if (path === "/api/admin/providers/deepseek" && init?.method === "PUT") {
          savedBody = JSON.parse(String(init.body)) as Record<string, unknown>;
          savedProvider = {
            ...DEEPSEEK_PROVIDER,
            enabled: true,
            hasApiKey: true,
            revision: 4,
          };
          return jsonResponse(savedProvider);
        }
        throw new Error(`Unexpected request: ${init?.method ?? "GET"} ${path}`);
      },
    );
    vi.stubGlobal("fetch", fetchMock);

    render(<App />);
    await screen.findByRole("heading", { name: "翻译渠道" });
    await user.click(screen.getByRole("button", { name: "配置 DeepSeek" }));
    expect(screen.getByLabelText("渠道名称")).toHaveAttribute("readonly");
    expect(screen.getByLabelText("API Base URL")).toHaveAttribute("readonly");
    expect(screen.queryByRole("button", { name: "添加模型" })).not.toBeInTheDocument();
    await user.type(screen.getByLabelText("API 密钥"), "sk-deepseek-secret");
    await user.click(screen.getByRole("checkbox", { name: "启用此渠道供管理员翻译使用" }));
    await user.click(screen.getByRole("button", { name: "保存配置" }));

    await waitFor(() => expect(screen.queryByLabelText("API 密钥")).not.toBeInTheDocument());
    expect(savedBody).toMatchObject({
      kind: "deepseek",
      apiKey: "sk-deepseek-secret",
      enabled: true,
      expectedRevision: 3,
    });
    expect(JSON.stringify(window.localStorage)).not.toContain("sk-deepseek-secret");
    expect(document.body).not.toHaveTextContent("sk-deepseek-secret");
    const writeCall = fetchMock.mock.calls.find(
      ([input]) => requestPath(input as RequestInfo | URL) === "/api/admin/providers/deepseek",
    );
    expect(new Headers(writeCall?.[1]?.headers).get("X-CSRF-Token")).toBe(
      "csrf-test-token",
    );
  });

  it("creates an OpenAI-compatible channel with multiple models and one default", async () => {
    const user = userEvent.setup();
    let savedBody: Record<string, unknown> | null = null;
    const fetchMock = vi.fn(
      async (input: RequestInfo | URL, init?: RequestInit): Promise<Response> => {
        const path = requestPath(input);
        if (path === "/api/admin/session") return jsonResponse(AUTHENTICATED_SESSION);
        if (path === "/api/admin/providers" && init?.method === "POST") {
          savedBody = JSON.parse(String(init.body)) as Record<string, unknown>;
          return jsonResponse({
            id: "lab",
            revision: 1,
            hasApiKey: true,
            ...savedBody,
          });
        }
        if (path === "/api/admin/providers") {
          return jsonResponse({ providers: [DEEPSEEK_PROVIDER] });
        }
        throw new Error(`Unexpected request: ${init?.method ?? "GET"} ${path}`);
      },
    );
    vi.stubGlobal("fetch", fetchMock);

    render(<App />);
    await user.click(await screen.findByRole("button", { name: "添加兼容渠道" }));
    await user.type(screen.getByLabelText("渠道名称"), "实验室网关");
    await user.type(screen.getByLabelText("API Base URL"), "http://llm.example.com/v1");
    await user.click(screen.getByRole("button", { name: "保存配置" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("必须使用 https");
    await user.clear(screen.getByLabelText("API Base URL"));
    await user.type(screen.getByLabelText("API Base URL"), "https://llm.example.com/v1");
    await user.type(screen.getByLabelText("API 密钥"), "sk-generic-secret");

    let modelIds = screen.getAllByLabelText("模型 ID");
    let modelNames = screen.getAllByLabelText("显示名称");
    await user.type(modelIds[0], "model-a");
    await user.type(modelNames[0], "模型 A");
    await user.click(screen.getByRole("button", { name: "添加模型" }));
    modelIds = screen.getAllByLabelText("模型 ID");
    modelNames = screen.getAllByLabelText("显示名称");
    await user.type(modelIds[1], "model-b");
    await user.type(modelNames[1], "模型 B");
    await user.click(screen.getAllByRole("radio", { name: "默认" })[1]);
    await user.click(screen.getByRole("checkbox", { name: "启用此渠道供管理员翻译使用" }));
    await user.click(screen.getByRole("button", { name: "保存配置" }));

    await waitFor(() => expect(savedBody).not.toBeNull());
    expect(savedBody).toMatchObject({
      kind: "openai_compatible",
      name: "实验室网关",
      baseUrl: "https://llm.example.com/v1",
      apiKey: "sk-generic-secret",
      models: [
        { id: "model-a", name: "模型 A", isDefault: false },
        { id: "model-b", name: "模型 B", isDefault: true },
      ],
    });
    expect(document.body).not.toHaveTextContent("sk-generic-secret");
  });
});
