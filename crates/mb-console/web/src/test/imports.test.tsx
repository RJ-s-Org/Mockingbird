import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { ApiError, api, unwrap } from "../api/client";
import { IMPORT_FILE_EXTENSIONS } from "../api/imports";
import { json, localMe, mockView, renderAt, serve, shell, viewerMe } from "./harness";

const MOCK = "/api/v1/workspaces/default/mocks/pets";
const JOB_ID = "0b6f3c1e-4c8b-4b7a-9d55-3f1f0f6a2a10";

function job(status: string, overrides: Record<string, unknown> = {}) {
  return {
    id: JOB_ID,
    mock: "pets",
    kind: "markdown",
    status,
    progress: { phase: "queued", done: 0, total: 0 },
    source_name: "pets.md",
    created_by: "local",
    created_at: "2026-10-16T00:00:00Z",
    started_at: null,
    finished_at: null,
    error: null,
    result_version: null,
    ...overrides,
  };
}

function summary(version: number, imported: boolean) {
  return {
    version,
    status: imported ? "draft" : "active",
    source_digest: `${version}`.repeat(64),
    created_by: "alice",
    created_at: "2026-10-16T00:00:00Z",
    operations: 3,
    imported,
  };
}

describe("imports", () => {
  it("file import sends multipart with upload header", async () => {
    const user = userEvent.setup();
    let uploadHeader: string | null = null;
    const calls = serve({
      ...shell(localMe),
      [`GET ${MOCK}`]: mockView("pets"),
      [`POST ${MOCK}/imports`]: (req: Request) => {
        uploadHeader = req.headers.get("X-Mockingbird-Upload");
        return json(job("queued"), 202);
      },
      [`GET ${MOCK}/imports/${JOB_ID}`]: job("queued"),
    });
    // Vitest re-encodes jsdom's FormData for Node's Request and drops part
    // file names on the way, so the name is checked where it is appended.
    const append = vi.spyOn(FormData.prototype, "append");
    const router = renderAt("/w/default/mocks/pets/import");

    const input = await screen.findByLabelText("Document file");
    expect(input.getAttribute("accept")).toBe(IMPORT_FILE_EXTENSIONS.join(","));
    expect((screen.getByLabelText("Kind") as HTMLSelectElement).value).toBe("");
    await user.upload(input, new File(["# Pets\n"], "pets.md", { type: "text/markdown" }));
    await user.click(screen.getByRole("button", { name: "Start import" }));

    await waitFor(() => expect(router.state.location.search).toEqual({ job: JOB_ID }));
    const post = calls.find((c) => c.method === "POST");
    expect(post?.path).toBe(`${MOCK}/imports`);
    expect(post?.contentType?.startsWith("multipart/form-data")).toBe(true);
    expect(post?.body).toContain('name="file"');
    expect(post?.body).toContain("# Pets");
    expect(post?.body).not.toContain('name="kind"');
    expect(append).toHaveBeenCalledWith("file", expect.any(File), "pets.md");
    expect(uploadHeader).toBe("1");
    await screen.findByText("Queued");
  });

  it("url import sends json", async () => {
    const user = userEvent.setup();
    const calls = serve({
      ...shell(localMe),
      [`GET ${MOCK}`]: mockView("pets"),
      [`POST ${MOCK}/imports`]: () => json(job("queued", { kind: "url" }), 202),
      [`GET ${MOCK}/imports/${JOB_ID}`]: job("queued", { kind: "url" }),
    });
    const router = renderAt("/w/default/mocks/pets/import");

    await user.click(await screen.findByRole("radio", { name: "URL" }));
    const url = screen.getByLabelText("Document URL");
    expect(url.getAttribute("type")).toBe("url");
    await user.type(url, "https://docs.example.com/pets");
    await user.click(screen.getByRole("button", { name: "Start import" }));

    await waitFor(() => expect(router.state.location.search).toEqual({ job: JOB_ID }));
    const post = calls.find((c) => c.method === "POST");
    expect(post?.contentType).toBe("application/json");
    expect(JSON.parse(post?.body ?? "")).toEqual({ url: "https://docs.example.com/pets" });
  });

  it("paste requires a kind", async () => {
    const user = userEvent.setup();
    const calls = serve({
      ...shell(localMe),
      [`GET ${MOCK}`]: mockView("pets"),
      [`POST ${MOCK}/imports`]: () => json(job("queued"), 202),
      [`GET ${MOCK}/imports/${JOB_ID}`]: job("queued"),
    });
    const router = renderAt("/w/default/mocks/pets/import");

    await user.click(await screen.findByRole("radio", { name: "Paste text" }));
    await user.type(screen.getByLabelText("Document text"), "GET /pets lists pets");
    const kind = screen.getByLabelText("Kind") as HTMLSelectElement;
    expect(kind.required).toBe(true);
    await user.click(screen.getByRole("button", { name: "Start import" }));
    expect(calls.some((c) => c.method === "POST")).toBe(false);

    await user.selectOptions(kind, "markdown");
    await user.click(screen.getByRole("button", { name: "Start import" }));
    await waitFor(() => expect(router.state.location.search).toEqual({ job: JOB_ID }));
    const post = calls.find((c) => c.method === "POST");
    expect(post?.contentType).toBe("application/json");
    expect(JSON.parse(post?.body ?? "")).toEqual({ kind: "markdown", text: "GET /pets lists pets" });
  });

  it("progress polls until done and opens the draft", { timeout: 10_000 }, async () => {
    const answers = [
      job("queued"),
      job("running", { progress: { phase: "analyzing", done: 1, total: 3 } }),
      job("done", { progress: { phase: "assembling", done: 3, total: 3 }, result_version: 2 }),
    ];
    let polls = 0;
    serve({
      ...shell(localMe),
      [`GET ${MOCK}`]: mockView("pets"),
      [`GET ${MOCK}/imports/${JOB_ID}`]: () => json(answers[Math.min(polls++, answers.length - 1)]),
    });
    const router = renderAt(`/w/default/mocks/pets/import?job=${JOB_ID}`);

    await screen.findByText("Queued");
    await screen.findByText("1 of 3", undefined, { timeout: 3000 });
    const bar = document.querySelector("progress");
    expect(bar?.getAttribute("value")).toBe("1");
    expect(bar?.getAttribute("max")).toBe("3");
    expect(screen.getByRole("button", { name: "Cancel import" })).toBeTruthy();

    await waitFor(() => expect(router.state.location.pathname).toMatch(/\/contracts\/2\/draft$/), {
      timeout: 4000,
    });
  });

  it("failed job shows its error and start over", async () => {
    const user = userEvent.setup();
    serve({
      ...shell(localMe),
      [`GET ${MOCK}`]: mockView("pets"),
      [`GET ${MOCK}/imports/${JOB_ID}`]: job("failed", { error: "the document has no API operations" }),
    });
    const router = renderAt(`/w/default/mocks/pets/import?job=${JOB_ID}`);

    const alert = await screen.findByRole("alert");
    expect(alert.textContent).toContain("the document has no API operations");
    expect(screen.queryByRole("button", { name: "Cancel import" })).toBeNull();
    await user.click(screen.getByRole("button", { name: "Start over" }));

    await screen.findByLabelText("Document file");
    expect(router.state.location.search).toEqual({});
  });

  it("cancel import calls delete", async () => {
    const user = userEvent.setup();
    let canceled = false;
    const running = job("running", { progress: { phase: "fetching", done: 0, total: 0 } });
    const calls = serve({
      ...shell(localMe),
      [`GET ${MOCK}`]: mockView("pets"),
      [`GET ${MOCK}/imports/${JOB_ID}`]: () => json(canceled ? job("canceled") : running),
      [`DELETE ${MOCK}/imports/${JOB_ID}`]: () => {
        canceled = true;
        return json(job("canceled"));
      },
    });
    renderAt(`/w/default/mocks/pets/import?job=${JOB_ID}`);

    await screen.findByText("Fetching the document");
    expect(document.querySelector("progress")).toBeNull();
    await user.click(screen.getByRole("button", { name: "Cancel import" }));

    await screen.findByText("Import canceled");
    expect(calls.some((c) => c.method === "DELETE" && c.path === `${MOCK}/imports/${JOB_ID}`)).toBe(true);
    expect(screen.getByRole("button", { name: "Start over" })).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Cancel import" })).toBeNull();
  });

  it("viewer cannot start an import", async () => {
    serve({
      ...shell(viewerMe),
      [`GET ${MOCK}`]: mockView("pets"),
    });
    renderAt("/w/default/mocks/pets/import");

    await screen.findByText("Read-only: editing a mock needs the editor role.");
    const start = screen.getByRole("button", { name: "Start import" }) as HTMLButtonElement;
    expect(start.disabled).toBe(true);
  });

  it("file mock cannot start an import", async () => {
    serve({
      ...shell(localMe),
      [`GET ${MOCK}`]: mockView("pets", { source: "file" }),
    });
    renderAt("/w/default/mocks/pets/import");

    await screen.findByText("Managed by config file");
    expect((screen.getByRole("button", { name: "Start import" }) as HTMLButtonElement).disabled).toBe(true);
  });

  it("contracts table links imported versions to review", async () => {
    const user = userEvent.setup();
    serve({
      ...shell(localMe),
      [`GET ${MOCK}`]: mockView("pets", { active_version: 1 }),
      [`GET ${MOCK}/contracts`]: [summary(2, true), summary(1, false)],
    });
    const router = renderAt("/w/default/mocks/pets/contracts");

    const nav = await screen.findByRole("navigation", { name: "Mock" });
    const tabs = within(nav)
      .getAllByRole("link")
      .map((l) => l.textContent);
    expect(tabs.indexOf("Import")).toBe(tabs.indexOf("Contracts") + 1);

    const table = await screen.findByRole("table", { name: "Contract versions" });
    await within(table).findByText("v2");
    const row = (v: number) => within(table).getByText(`v${v}`).closest("tr") as HTMLElement;
    const review = within(row(2)).getByRole("link", { name: "Review version 2" });
    expect(review.getAttribute("href")).toBe("/w/default/mocks/pets/contracts/2/draft");
    expect(within(row(1)).queryByRole("link", { name: /Review/ })).toBeNull();

    await user.click(review);
    await waitFor(() => expect(router.state.location.pathname).toBe("/w/default/mocks/pets/contracts/2/draft"));
  });

  it("client keeps problem errors", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              type: "about:blank",
              title: "Unprocessable Entity",
              status: 422,
              detail: "the draft does not compile",
              errors: [{ pointer: "/paths/~1pets/get", message: "missing responses" }],
            }),
            { status: 422, headers: { "Content-Type": "application/problem+json" } },
          ),
      ),
    );
    const error = await api
      .PUT("/api/v1/workspaces/{ws}/mocks/{mock}/contracts/{version}/draft", {
        params: { path: { ws: "default", mock: "pets", version: 2 } },
        body: { document: {} },
      })
      .then(unwrap)
      .catch((e: unknown) => e);
    expect(error).toBeInstanceOf(ApiError);
    const problem = (error as ApiError).problem;
    expect(problem?.errors?.[0]?.pointer).toBe("/paths/~1pets/get");
    expect(problem?.errors?.[0]?.message).toBe("missing responses");
  });
});
