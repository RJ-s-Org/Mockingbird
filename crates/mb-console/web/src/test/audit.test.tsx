import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import { json, localMe, renderAt, serve, shell, viewerMe } from "./harness";

const BASE = "/api/v1/workspaces/default";

function event(id: number, overrides: Record<string, unknown> = {}) {
  return {
    id,
    at: "2026-10-16T08:00:00Z",
    workspace: "default",
    actor_kind: "local",
    actor_subject: "local",
    action: "mock.update",
    target: `mock:m${id}`,
    before: { name: `m${id}`, persona: null },
    after: { name: `m${id}`, persona: "friendly" },
    request_id: null,
    ...overrides,
  };
}

/** `count` events with descending ids from `top`. */
function page(top: number, count: number) {
  return Array.from({ length: count }, (_, i) => event(top - i));
}

describe("audit", () => {
  it("audit filters and load more use before id", async () => {
    const user = userEvent.setup();
    const calls = serve({
      ...shell(viewerMe),
      [`GET ${BASE}/audit`]: (req: Request) => {
        const before = new URL(req.url).searchParams.get("before");
        return json(before ? page(Number(before) - 1, 3) : page(200, 50));
      },
    });
    renderAt("/w/default/audit");

    const table = await screen.findByRole("table", { name: "Audit events" });
    expect(await within(table).findByText("mock:m200")).toBeTruthy();
    await user.type(screen.getByLabelText("Action"), "mock.update");
    await user.type(screen.getByLabelText("Actor"), "local");
    await user.click(screen.getByRole("button", { name: "Apply" }));
    await waitFor(() => expect(calls.some((c) => c.search.includes("actor=local"))).toBe(true));
    const filtered = calls.filter((c) => c.path === `${BASE}/audit`).pop();
    const q = new URLSearchParams(filtered?.search);
    expect(q.get("action")).toBe("mock.update");
    expect(q.get("actor")).toBe("local");
    expect(q.get("before")).toBeNull();

    await user.click(await screen.findByRole("button", { name: "Load more" }));
    // The filtered list is a new table.
    const filteredTable = await screen.findByRole("table", { name: "Audit events" });
    expect(await within(filteredTable).findByText("mock:m150")).toBeTruthy();
    const more = calls.filter((c) => c.path === `${BASE}/audit`).pop();
    const mq = new URLSearchParams(more?.search);
    expect(mq.get("before")).toBe("151");
    expect(mq.get("action")).toBe("mock.update");
    expect(mq.get("actor")).toBe("local");
    // A short page is the last one.
    await waitFor(() => expect(screen.queryByRole("button", { name: "Load more" })).toBeNull());
    // No sysadmin toggle for a viewer.
    expect(screen.queryByLabelText("All workspaces")).toBeNull();
    // Rendering 53 rows in jsdom is slow.
  }, 20_000);

  it("jsonl export link carries filters", async () => {
    const user = userEvent.setup();
    serve({ ...shell(localMe), [`GET ${BASE}/audit`]: [event(1)], "GET /api/v1/audit": [event(2)] });
    renderAt("/w/default/audit");

    await screen.findByText("mock:m1");
    await user.type(screen.getByLabelText("Action"), "api_key.create");
    await user.type(screen.getByLabelText("Actor"), "ada lovelace");
    await user.click(screen.getByRole("button", { name: "Apply" }));
    const link = screen.getByRole("link", { name: "Export JSONL" });
    expect(link.hasAttribute("download")).toBe(true);
    const href = new URL(link.getAttribute("href") ?? "", "http://x");
    expect(href.pathname).toBe(`${BASE}/audit`);
    expect(href.searchParams.get("action")).toBe("api_key.create");
    expect(href.searchParams.get("actor")).toBe("ada lovelace");
    expect(href.searchParams.get("format")).toBe("jsonl");

    // The sysadmin's "All workspaces" toggle switches to the global log.
    await user.click(screen.getByLabelText("All workspaces"));
    expect(await screen.findByText("mock:m2")).toBeTruthy();
    expect(new URL(link.getAttribute("href") ?? "", "http://x").pathname).toBe("/api/v1/audit");
  });

  it("an event expands to its before and after JSON", async () => {
    const user = userEvent.setup();
    serve({ ...shell(localMe), [`GET ${BASE}/audit`]: [event(7)] });
    renderAt("/w/default/audit");

    const table = await screen.findByRole("table", { name: "Audit events" });
    const row = (await within(table).findByText("mock:m7")).closest("tr") as HTMLElement;
    expect(within(row).getByText("mock.update")).toBeTruthy();
    expect(within(row).getByText("local")).toBeTruthy();
    await user.click(within(row).getByText("Changes"));
    const before = within(row).getByLabelText("Before");
    const after = within(row).getByLabelText("After");
    expect(before.textContent).toContain('"persona": null');
    expect(after.textContent).toContain('"persona": "friendly"');
  });
});
