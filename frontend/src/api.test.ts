import { afterEach, describe, expect, it, vi } from "vitest";

import { createJob } from "./api";
import type { JobDraft } from "./types";

const RESERVED_JOB_ID = "4d35b53c-2f65-4f51-b34c-4c751ea58b16";
const PAID_DRAFT: JobDraft = {
  providerId: "custom-provider",
  modelId: "current-model",
  thinkingMode: "enabled",
  reasoningEffort: "high",
  sourceLang: "en",
  targetLang: "zh",
  pages: " 1-3 ",
  outputMono: true,
  outputDual: true,
};

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json" },
  });
}

afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

describe("createJob admission retry", () => {
  it("reuses the reserved job id and FormData for 409, 429 and 503", async () => {
    vi.useFakeTimers();
    const fetchMock = vi
      .fn<(input: RequestInfo | URL, init?: RequestInit) => Promise<Response>>()
      .mockResolvedValueOnce(jsonResponse({ error: "upload in progress" }, 409))
      .mockResolvedValueOnce(jsonResponse({ error: "busy" }, 429))
      .mockResolvedValueOnce(jsonResponse({ error: "recycling" }, 503))
      .mockResolvedValueOnce(
        jsonResponse({ id: RESERVED_JOB_ID, status: "queued" }, 202),
      );
    vi.stubGlobal("fetch", fetchMock);

    const createdPromise = createJob(
      new File(["%PDF-test"], "paper.pdf", { type: "application/pdf" }),
      PAID_DRAFT,
      RESERVED_JOB_ID,
      "csrf-token",
    );

    expect(fetchMock).toHaveBeenCalledTimes(1);
    for (let expectedCalls = 2; expectedCalls <= 4; expectedCalls += 1) {
      await vi.advanceTimersByTimeAsync(1_000);
      expect(fetchMock).toHaveBeenCalledTimes(expectedCalls);
    }
    await expect(createdPromise).resolves.toEqual({
      id: RESERVED_JOB_ID,
      status: "queued",
    });

    const bodies = fetchMock.mock.calls.map(([, init]) => init?.body);
    expect(bodies[0]).toBeInstanceOf(FormData);
    expect(bodies.every((body) => body === bodies[0])).toBe(true);
    const form = bodies[0] as FormData;
    expect(form.get("file")).toBeInstanceOf(File);
    expect(form.get("provider_id")).toBe("custom-provider");
    expect(form.get("model_id")).toBe("current-model");
    expect(form.get("pages")).toBe("1-3");

    for (const [, init] of fetchMock.mock.calls) {
      const headers = new Headers(init?.headers);
      expect(headers.get("x-job-id")).toBe(RESERVED_JOB_ID);
      expect(headers.get("x-csrf-token")).toBe("csrf-token");
      expect(headers.get("x-pdf2zh-origin")).toBe(window.location.origin);
    }
  });

  it("does not retry a non-admission error", async () => {
    vi.useFakeTimers();
    const fetchMock = vi
      .fn<(input: RequestInfo | URL, init?: RequestInit) => Promise<Response>>()
      .mockResolvedValue(jsonResponse({ error: "admin required" }, 401));
    vi.stubGlobal("fetch", fetchMock);

    const createdPromise = createJob(
      new File(["%PDF-test"], "paper.pdf", { type: "application/pdf" }),
      PAID_DRAFT,
      RESERVED_JOB_ID,
      "csrf-token",
    );

    await expect(createdPromise).rejects.toMatchObject({
      status: 401,
    });
    expect(fetchMock).toHaveBeenCalledTimes(1);
    expect(vi.getTimerCount()).toBe(0);
  });
});
