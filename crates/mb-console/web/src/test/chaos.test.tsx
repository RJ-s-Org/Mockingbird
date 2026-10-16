import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import { json, localMe, mockView, problem, renderAt, serve, shell, viewerMe, type Answer } from "./harness";

const MOCK = "/api/v1/workspaces/default/mocks/pets";

function isDisabled(el: HTMLElement): boolean {
  return el.matches(":disabled");
}

/** Routes for the overview page of `pets`; PATCH merges the body into the mock. */
function overview(mock: Record<string, unknown>, extra: Record<string, Answer> = {}, me = localMe) {
  let current: Record<string, unknown> = mockView("pets", { limits: {}, chaos: null, ...mock });
  return serve({
    ...shell(me),
    [`GET ${MOCK}`]: () => json(current),
    [`GET /api/v1/workspaces/default/llm-profiles`]: [],
    [`PATCH ${MOCK}`]: async (req: Request) => {
      current = { ...current, ...((await req.json()) as object), updated_at: "2026-10-16T01:00:00Z" };
      return json(current);
    },
    ...extra,
  });
}

async function section() {
  return screen.findByRole("region", { name: "Chaos and limits" });
}

function field(region: HTMLElement, label: string): HTMLInputElement {
  return within(region).getByLabelText(label);
}

const ALL_FIELDS = [
  "Error rate (%)",
  "Latency jitter min (ms)",
  "Latency jitter max (ms)",
  "Dropped streams (%)",
  "Rate limit (requests/s)",
  "Burst",
  "Daily token budget",
  "Max request size (KiB)",
  "Max response size (KiB)",
];

describe("chaos and limits", () => {
  it("saves chaos and limits as patch", async () => {
    const user = userEvent.setup();
    const calls = overview({});
    renderAt("/w/default/mocks/pets/overview");

    const region = await section();
    expect(
      within(region).getByText(
        "Callers can bypass chaos with X-Mockingbird-Chaos: off when header overrides are enabled.",
      ),
    ).toBeTruthy();
    const save = within(region).getByRole("button", { name: "Save chaos and limits" });
    expect(isDisabled(save)).toBe(true);

    await user.type(field(region, "Error rate (%)"), "5");
    await user.type(field(region, "Latency jitter min (ms)"), "100");
    await user.type(field(region, "Latency jitter max (ms)"), "500");
    await user.type(field(region, "Rate limit (requests/s)"), "10");
    await user.type(field(region, "Burst"), "20");
    await user.type(field(region, "Daily token budget"), "1000");
    await user.type(field(region, "Max request size (KiB)"), "64");
    await user.type(field(region, "Max response size (KiB)"), "128");
    await user.click(save);

    await within(await section()).findByText("Saved.");
    const patches = calls.filter((c) => c.method === "PATCH");
    expect(patches).toHaveLength(1);
    expect(patches[0]?.path).toBe(MOCK);
    expect(JSON.parse(patches[0]?.body ?? "")).toEqual({
      chaos: { error_rate: 0.05, drop_stream_rate: 0, latency_jitter: { min_ms: 100, max_ms: 500 } },
      limits: {
        rate_limit: { requests_per_second: 10, burst: 20 },
        daily_token_budget: 1000,
        max_request_bytes: 65536,
        max_response_bytes: 131072,
      },
    });
  });

  it("percent fields convert to rates", async () => {
    const user = userEvent.setup();
    const calls = overview({
      chaos: { error_rate: 0.125, drop_stream_rate: 0.5 },
      limits: { max_request_bytes: 1500 },
    });
    renderAt("/w/default/mocks/pets/overview");

    const region = await section();
    expect(field(region, "Error rate (%)").value).toBe("12.5");
    expect(field(region, "Dropped streams (%)").value).toBe("50");
    expect(field(region, "Latency jitter min (ms)").value).toBe("");
    // Bytes that are not a whole KiB survive the round trip.
    expect(field(region, "Max request size (KiB)").value).toBe(String(1500 / 1024));

    await user.clear(field(region, "Error rate (%)"));
    await user.type(field(region, "Error rate (%)"), "7");
    await user.clear(field(region, "Dropped streams (%)"));
    await user.type(field(region, "Dropped streams (%)"), "100");
    await user.click(within(region).getByRole("button", { name: "Save chaos and limits" }));

    await waitFor(() => expect(calls.some((c) => c.method === "PATCH")).toBe(true));
    const body = JSON.parse(calls.find((c) => c.method === "PATCH")?.body ?? "");
    expect(body.chaos).toEqual({ error_rate: 0.07, drop_stream_rate: 1, latency_jitter: null });
    expect(body.limits.max_request_bytes).toBe(1500);
  });

  it("jitter min above max blocks save", async () => {
    const user = userEvent.setup();
    const calls = overview({});
    renderAt("/w/default/mocks/pets/overview");

    const region = await section();
    const save = within(region).getByRole("button", { name: "Save chaos and limits" });
    await user.type(field(region, "Latency jitter min (ms)"), "500");
    await user.type(field(region, "Latency jitter max (ms)"), "100");
    expect(within(region).getByText("Jitter min must not be above jitter max.")).toBeTruthy();
    expect(field(region, "Latency jitter min (ms)").getAttribute("aria-invalid")).toBe("true");
    expect(isDisabled(save)).toBe(true);

    // Other ranges are checked too.
    await user.clear(field(region, "Latency jitter max (ms)"));
    await user.type(field(region, "Latency jitter max (ms)"), "30001");
    expect(within(region).getByText("Jitter max must be between 0 and 30000 ms.")).toBeTruthy();
    await user.clear(field(region, "Latency jitter max (ms)"));
    await user.type(field(region, "Latency jitter max (ms)"), "600");
    await user.type(field(region, "Error rate (%)"), "101");
    expect(within(region).getByText("Error rate must be between 0 and 100%.")).toBeTruthy();
    await user.clear(field(region, "Error rate (%)"));
    await user.type(field(region, "Burst"), "5");
    expect(within(region).getByText("Set both rate limit and burst, or neither.")).toBeTruthy();
    await user.clear(field(region, "Burst"));
    await user.type(field(region, "Max response size (KiB)"), "16385");
    expect(within(region).getByText("Max response size must be between 1 and 16384 KiB.")).toBeTruthy();
    expect(isDisabled(save)).toBe(true);
    await user.clear(field(region, "Max response size (KiB)"));

    expect(isDisabled(save)).toBe(false);
    await user.click(save);
    await waitFor(() => expect(calls.some((c) => c.method === "PATCH")).toBe(true));
    expect(JSON.parse(calls.find((c) => c.method === "PATCH")?.body ?? "").chaos).toEqual({
      error_rate: 0,
      drop_stream_rate: 0,
      latency_jitter: { min_ms: 500, max_ms: 600 },
    });
  });

  it("clearing chaos sends null", async () => {
    const user = userEvent.setup();
    const calls = overview({
      chaos: { error_rate: 0.2, drop_stream_rate: 0.1, latency_jitter: { min_ms: 0, max_ms: 50 } },
      limits: { daily_token_budget: 5 },
    });
    renderAt("/w/default/mocks/pets/overview");

    const region = await section();
    expect(field(region, "Latency jitter min (ms)").value).toBe("0");
    expect(field(region, "Latency jitter max (ms)").value).toBe("50");
    await user.clear(field(region, "Error rate (%)"));
    await user.clear(field(region, "Dropped streams (%)"));
    await user.clear(field(region, "Latency jitter max (ms)"));
    // A zero is the same as empty.
    await user.type(field(region, "Latency jitter max (ms)"), "0");
    await user.click(within(region).getByRole("button", { name: "Save chaos and limits" }));

    await waitFor(() => expect(calls.some((c) => c.method === "PATCH")).toBe(true));
    expect(JSON.parse(calls.find((c) => c.method === "PATCH")?.body ?? "")).toEqual({
      chaos: null,
      limits: { rate_limit: null, daily_token_budget: 5, max_request_bytes: null, max_response_bytes: null },
    });
  });

  it("422 is shown", async () => {
    const user = userEvent.setup();
    overview(
      {},
      {
        [`PATCH ${MOCK}`]: () =>
          problem(422, "Unprocessable Entity", "limits.daily_token_budget must be at least 1"),
      },
    );
    renderAt("/w/default/mocks/pets/overview");

    const region = await section();
    await user.type(field(region, "Daily token budget"), "3");
    await user.click(within(region).getByRole("button", { name: "Save chaos and limits" }));
    const alert = await within(region).findByRole("alert");
    expect(alert.textContent).toContain("limits.daily_token_budget must be at least 1");
  });

  it("file mock section is read only", async () => {
    overview({ source: "file", chaos: { error_rate: 0.5, drop_stream_rate: 0 } });
    renderAt("/w/default/mocks/pets/overview");

    const region = await section();
    expect(within(region).getByText("Managed by config file")).toBeTruthy();
    expect(field(region, "Error rate (%)").value).toBe("50");
    for (const label of ALL_FIELDS) expect(isDisabled(field(region, label)), label).toBe(true);
    expect(isDisabled(within(region).getByRole("button", { name: "Save chaos and limits" }))).toBe(true);
  });

  it("viewer section is read only", async () => {
    overview({}, {}, viewerMe);
    renderAt("/w/default/mocks/pets/overview");

    await screen.findByText("Vera");
    const region = await section();
    await within(region).findByText("Read-only: editing a mock needs the editor role.");
    for (const label of ALL_FIELDS) expect(isDisabled(field(region, label)), label).toBe(true);
    expect(within(region).queryByRole("button", { name: "Save chaos and limits" })).toBeNull();
  });

  it("scenario chaos override round trips", async () => {
    const user = userEvent.setup();
    const chaos = { error_rate: 0.25, drop_stream_rate: 0, latency_jitter: { min_ms: 10, max_ms: 20 } };
    const stored = {
      name: "flaky",
      instructions: null,
      status: null,
      latency_ms: null,
      set: null,
      operations: [],
      source: "api",
      chaos,
    };
    const calls = serve({
      ...shell(localMe),
      [`GET ${MOCK}`]: mockView("pets"),
      [`GET ${MOCK}/scenarios`]: [stored],
      [`PUT ${MOCK}/scenarios/flaky`]: (req: Request) => req.json().then((b) => json({ ...stored, ...b })),
    });
    renderAt("/w/default/mocks/pets/scenarios");

    await user.click(await screen.findByRole("button", { name: "Edit scenario flaky" }));
    const editor = await screen.findByRole("dialog", { name: "Scenario flaky" });
    const override = within(editor).getByRole("group", { name: "Chaos override" });
    expect(field(override, "Error rate (%)").value).toBe("25");
    expect(field(override, "Latency jitter min (ms)").value).toBe("10");
    expect(field(override, "Latency jitter max (ms)").value).toBe("20");
    expect(field(override, "Dropped streams (%)").value).toBe("");
    await user.click(within(editor).getByRole("button", { name: "Save" }));

    await waitFor(() => expect(calls.filter((c) => c.method === "PUT")).toHaveLength(1));
    expect(JSON.parse(calls.filter((c) => c.method === "PUT")[0]?.body ?? "").chaos).toEqual(chaos);
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());

    // Emptying the override removes it; a bad range blocks the save first.
    await user.click(screen.getByRole("button", { name: "Edit scenario flaky" }));
    const again = await screen.findByRole("dialog", { name: "Scenario flaky" });
    const group = within(again).getByRole("group", { name: "Chaos override" });
    await user.clear(field(group, "Latency jitter min (ms)"));
    await user.type(field(group, "Latency jitter min (ms)"), "30");
    expect(within(group).getByText("Jitter min must not be above jitter max.")).toBeTruthy();
    expect(isDisabled(within(again).getByRole("button", { name: "Save" }))).toBe(true);
    await user.clear(field(group, "Error rate (%)"));
    await user.clear(field(group, "Latency jitter min (ms)"));
    await user.clear(field(group, "Latency jitter max (ms)"));
    await user.click(within(again).getByRole("button", { name: "Save" }));

    await waitFor(() => expect(calls.filter((c) => c.method === "PUT")).toHaveLength(2));
    expect(JSON.parse(calls.filter((c) => c.method === "PUT")[1]?.body ?? "")).not.toHaveProperty("chaos");
  });
});
