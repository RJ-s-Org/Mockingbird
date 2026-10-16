import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import { json, localMe, mockView, renderAt, serve, shell, viewerMe } from "./harness";

const MOCK = "/api/v1/workspaces/default/mocks/pets";

const contract = {
  version: 2,
  status: "active",
  source_digest: "2".repeat(64),
  created_by: null,
  created_at: "2026-10-16T00:00:00Z",
  title: "Orders",
  api_version: "1.0.0",
  operations: [
    { id: "listOrders", method: "GET", path: "/orders", kind: "query" },
    { id: "createOrder", method: "POST", path: "/orders", kind: "create" },
    { id: "getOrder", method: "GET", path: "/orders/{orderId}", kind: "query" },
  ],
};

function scenario(name: string, overrides: Record<string, unknown> = {}) {
  return {
    name,
    instructions: null,
    status: null,
    latency_ms: null,
    set: null,
    operations: [],
    source: "api",
    ...overrides,
  };
}

function routes(scenarios: unknown[]) {
  return {
    ...shell(localMe),
    [`GET ${MOCK}`]: mockView("pets", { active_version: 2 }),
    [`GET ${MOCK}/contracts/2`]: contract,
    [`GET ${MOCK}/scenarios`]: scenarios,
  };
}

describe("scenarios", () => {
  it("lists name, status, latency, operation count and source", async () => {
    serve(
      routes([
        scenario("slow", { latency_ms: 250, operations: ["listOrders", "getOrder"] }),
        scenario("broken", { status: 503, source: "file" }),
      ]),
    );
    renderAt("/w/default/mocks/pets/scenarios");

    const table = await screen.findByRole("table", { name: "Scenarios" });
    const row = (name: string) => within(table).getByText(name).closest("tr") as HTMLElement;
    await within(table).findByText("slow");
    expect(within(row("slow")).getByText("250 ms")).toBeTruthy();
    expect(within(row("slow")).getByText("2")).toBeTruthy();
    expect(within(row("slow")).getByText("api")).toBeTruthy();
    expect(within(row("broken")).getByText("503")).toBeTruthy();
    expect(within(row("broken")).getByText("all")).toBeTruthy();
    expect(within(row("broken")).getByText("file")).toBeTruthy();
    // A file scenario can be viewed, not deleted.
    expect(within(row("broken")).queryByRole("button", { name: "Delete scenario broken" })).toBeNull();
    expect(within(row("slow")).getByRole("button", { name: "Delete scenario slow" })).toBeTruthy();
  });

  it("scenario editor rejects invalid set JSON", async () => {
    const user = userEvent.setup();
    const calls = serve({
      ...routes([]),
      [`PUT ${MOCK}/scenarios/slow`]: (req: Request) => req.json().then((b) => json({ ...scenario("slow"), ...b })),
    });
    renderAt("/w/default/mocks/pets/scenarios");

    await user.click(await screen.findByRole("button", { name: "New scenario" }));
    const editor = await screen.findByRole("dialog", { name: "New scenario" });
    await user.type(within(editor).getByLabelText("Name"), "slow");
    const set = within(editor).getByLabelText("Set (JSON object)");
    const save = within(editor).getByRole("button", { name: "Save" });

    await user.click(set);
    await user.paste('{"status":');
    expect(within(editor).getByText(/Set is not valid JSON/)).toBeTruthy();
    expect(set.getAttribute("aria-invalid")).toBe("true");
    expect((save as HTMLButtonElement).disabled).toBe(true);

    await user.clear(set);
    await user.paste("[1, 2]");
    expect(within(editor).getByText("Set must be a JSON object.")).toBeTruthy();
    expect((save as HTMLButtonElement).disabled).toBe(true);

    await user.clear(set);
    await user.paste('{"status":"shipped"}');
    expect(within(editor).queryByText(/Set (is not valid|must be)/)).toBeNull();
    await user.type(within(editor).getByLabelText("Latency (ms)"), "100");
    await user.type(within(editor).getByLabelText("Status"), "201");
    await user.type(within(editor).getByLabelText("Instructions"), "Be slow");
    await user.click(save);

    await waitFor(() => expect(calls.some((c) => c.method === "PUT")).toBe(true));
    const put = calls.find((c) => c.method === "PUT");
    expect(put?.path).toBe(`${MOCK}/scenarios/slow`);
    expect(JSON.parse(put?.body ?? "")).toEqual({
      instructions: "Be slow",
      status: 201,
      latency_ms: 100,
      set: { status: "shipped" },
      operations: [],
    });
    await waitFor(() => expect(screen.queryByRole("dialog", { name: "New scenario" })).toBeNull());
  });

  it("scenario operations come from active contract", async () => {
    const user = userEvent.setup();
    const calls = serve({
      ...routes([scenario("slow", { latency_ms: 100, operations: ["getOrder"] })]),
      [`PUT ${MOCK}/scenarios/slow`]: (req: Request) => req.json().then((b) => json({ ...scenario("slow"), ...b })),
    });
    renderAt("/w/default/mocks/pets/scenarios");

    await user.click(await screen.findByRole("button", { name: "Edit scenario slow" }));
    const editor = await screen.findByRole("dialog", { name: "Scenario slow" });
    const ops = within(editor).getByLabelText("Operations") as HTMLSelectElement;
    await waitFor(() => expect(ops.options.length).toBe(3));
    expect(Array.from(ops.options).map((o) => o.value)).toEqual(["listOrders", "createOrder", "getOrder"]);
    expect(Array.from(ops.selectedOptions).map((o) => o.value)).toEqual(["getOrder"]);
    expect((within(editor).getByLabelText("Latency (ms)") as HTMLInputElement).value).toBe("100");

    await user.selectOptions(ops, ["listOrders"]);
    await user.click(within(editor).getByRole("button", { name: "Save" }));

    await waitFor(() => expect(calls.some((c) => c.method === "PUT")).toBe(true));
    const body = JSON.parse(calls.find((c) => c.method === "PUT")?.body ?? "");
    expect(body.operations).toEqual(["listOrders", "getOrder"]);
    expect(body.latency_ms).toBe(100);
    expect(body.set).toBeNull();
  });

  it("delete asks for confirmation", async () => {
    const user = userEvent.setup();
    const calls = serve({
      ...routes([scenario("slow")]),
      [`DELETE ${MOCK}/scenarios/slow`]: () => new Response(null, { status: 204 }),
    });
    renderAt("/w/default/mocks/pets/scenarios");

    await user.click(await screen.findByRole("button", { name: "Delete scenario slow" }));
    const confirm = await screen.findByRole("dialog", { name: "Delete scenario slow?" });
    expect(calls.some((c) => c.method === "DELETE")).toBe(false);
    await user.click(within(confirm).getByRole("button", { name: "Delete" }));
    await waitFor(() => expect(calls.some((c) => c.method === "DELETE" && c.path === `${MOCK}/scenarios/slow`)).toBe(true));
  });

  it("viewer and file scenarios are read-only", async () => {
    const user = userEvent.setup();
    serve({
      ...routes([scenario("broken", { status: 503, source: "file", set: { a: 1 } })]),
      ...shell(viewerMe),
    });
    renderAt("/w/default/mocks/pets/scenarios");

    const table = await screen.findByRole("table", { name: "Scenarios" });
    await within(table).findByText("broken");
    await screen.findByText("Vera");
    expect(screen.queryByRole("button", { name: "New scenario" })).toBeNull();
    expect(screen.queryByRole("button", { name: /Delete scenario/ })).toBeNull();

    await user.click(within(table).getByRole("button", { name: "View scenario broken" }));
    const editor = await screen.findByRole("dialog", { name: "Scenario broken" });
    expect(within(editor).getByText("Managed by config file")).toBeTruthy();
    expect(within(editor).getByLabelText("Status").matches(":disabled")).toBe(true);
    expect(within(editor).queryByRole("button", { name: "Save" })).toBeNull();
    expect((within(editor).getByLabelText("Set (JSON object)") as HTMLTextAreaElement).value).toContain('"a": 1');
  });
});
