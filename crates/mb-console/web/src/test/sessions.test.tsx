import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, onTestFinished, vi } from "vitest";
import { localMe, mockView, renderAt, serve, shell, viewerMe } from "./harness";

const MOCK = "/api/v1/workspaces/default/mocks/pets";

const sessions = [
  { session: "s1", resources: 3, last_touched: "2026-10-16T10:00:00Z" },
  { session: "s2", resources: 0, last_touched: "2026-10-16T09:00:00Z" },
];

const exported = [{ id: "o1", resource_type: "orders", data: { id: "o1", status: "pending" } }];

function routes() {
  return {
    ...shell(localMe),
    [`GET ${MOCK}`]: mockView("pets"),
    [`GET ${MOCK}/sessions`]: sessions,
    [`GET ${MOCK}/sessions/s1`]: exported,
    [`DELETE ${MOCK}/sessions/s1`]: () => new Response(null, { status: 204 }),
    [`PUT ${MOCK}/sessions/seeded`]: () => new Response(null, { status: 204 }),
  };
}

function row(table: HTMLElement, session: string) {
  return within(table).getByText(session).closest("tr") as HTMLElement;
}

describe("sessions", () => {
  it("lists session, resources and last touched; inspect shows the export", async () => {
    const user = userEvent.setup();
    serve(routes());
    renderAt("/w/default/mocks/pets/sessions");

    const table = await screen.findByRole("table", { name: "Sessions" });
    await within(table).findByText("s1");
    expect(within(row(table, "s1")).getByText("3")).toBeTruthy();
    expect(within(row(table, "s1")).getByRole("time").getAttribute("datetime")).toBe("2026-10-16T10:00:00Z");

    await user.click(within(row(table, "s1")).getByRole("button", { name: "Inspect session s1" }));
    const drawer = await screen.findByRole("dialog", { name: "Session s1" });
    await waitFor(() => expect(drawer.querySelector("pre")?.textContent).toBe(JSON.stringify(exported, null, 2)));
  });

  it("session reset and seed call API", async () => {
    const user = userEvent.setup();
    const calls = serve(routes());
    renderAt("/w/default/mocks/pets/sessions");

    const table = await screen.findByRole("table", { name: "Sessions" });
    await within(table).findByText("s1");
    await user.click(within(row(table, "s1")).getByRole("button", { name: "Reset session s1" }));
    const confirm = await screen.findByRole("dialog", { name: "Reset session s1?" });
    expect(calls.some((c) => c.method === "DELETE")).toBe(false);
    await user.click(within(confirm).getByRole("button", { name: "Reset" }));
    await waitFor(() => expect(calls.some((c) => c.method === "DELETE" && c.path === `${MOCK}/sessions/s1`)).toBe(true));

    // Invalid JSON is reported inline and nothing is sent.
    await user.type(screen.getByLabelText("Session to seed"), "seeded");
    const file = screen.getByLabelText("Resources file");
    await user.upload(file, new File(["[{not json"], "bad.json", { type: "application/json" }));
    await user.click(screen.getByRole("button", { name: "Seed" }));
    const seedError = await screen.findByText(/not valid JSON/);
    expect(seedError.id).toBe("seed-error");
    // A friendly message, never the parser's raw text.
    expect(seedError.textContent).toMatch(/^The file is not valid JSON( \(line \d+, column \d+\))?\.$/);
    expect(seedError.textContent).not.toContain("Unexpected token");
    await user.upload(file, new File(['{"id":"o1"}'], "object.json", { type: "application/json" }));
    await user.click(screen.getByRole("button", { name: "Seed" }));
    await screen.findByText("The file must contain a JSON array of resources.");
    expect(calls.some((c) => c.method === "PUT")).toBe(false);

    const text = JSON.stringify(exported);
    await user.upload(file, new File([text], "seed.json", { type: "application/json" }));
    await user.click(screen.getByRole("button", { name: "Seed" }));
    await screen.findByText("Seeded session seeded with 1 resource.");
    const put = calls.find((c) => c.method === "PUT");
    expect(put?.path).toBe(`${MOCK}/sessions/seeded`);
    expect(put?.contentType).toBe("application/json");
    expect(JSON.parse(put?.body ?? "")).toEqual(exported);
  });

  it("export downloads <mock>-<session>.json", async () => {
    const user = userEvent.setup();
    serve(routes());
    let blob: Blob | undefined;
    const createObjectURL = vi.fn((b: Blob) => {
      blob = b;
      return "blob:export";
    });
    const original = { create: URL.createObjectURL, revoke: URL.revokeObjectURL };
    URL.createObjectURL = createObjectURL;
    URL.revokeObjectURL = vi.fn();
    onTestFinished(() => {
      URL.createObjectURL = original.create;
      URL.revokeObjectURL = original.revoke;
    });
    const clicks: HTMLAnchorElement[] = [];
    vi.spyOn(HTMLAnchorElement.prototype, "click").mockImplementation(function (this: HTMLAnchorElement) {
      clicks.push(this);
    });
    renderAt("/w/default/mocks/pets/sessions");

    const table = await screen.findByRole("table", { name: "Sessions" });
    await within(table).findByText("s1");
    await user.click(within(row(table, "s1")).getByRole("button", { name: "Export session s1" }));

    await waitFor(() => expect(clicks.length).toBe(1));
    expect(clicks[0]?.download).toBe("pets-s1.json");
    expect(clicks[0]?.getAttribute("href")).toBe("blob:export");
    expect(JSON.parse((await blob?.text()) ?? "")).toEqual(exported);
  });

  it("viewer cannot reset or seed", async () => {
    serve({ ...routes(), ...shell(viewerMe) });
    renderAt("/w/default/mocks/pets/sessions");

    const table = await screen.findByRole("table", { name: "Sessions" });
    await within(table).findByText("s1");
    await screen.findByText("Vera");
    expect(within(table).getByRole("button", { name: "Inspect session s1" })).toBeTruthy();
    expect(within(table).queryByRole("button", { name: "Reset session s1" })).toBeNull();
    expect(screen.queryByLabelText("Resources file")).toBeNull();
  });
});
