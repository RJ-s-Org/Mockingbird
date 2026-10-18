import { act, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { directionLabel, fromMcmTrigger, toMcmTrigger, triggerError, type TriggerDraft } from "../api/channels";
import { json, localMe, mockView, problem, renderAt, serve, shell, viewerMe, type Answer } from "./harness";

const MOCK = "/api/v1/workspaces/default/mocks/shop";

/** A stand-in for the browser's `EventSource`: tests push events by hand. */
class FakeEventSource {
  static instances: FakeEventSource[] = [];
  url: string;
  closed = false;
  onerror: ((e: Event) => void) | null = null;
  private listeners = new Map<string, ((e: MessageEvent) => void)[]>();

  constructor(url: string | URL) {
    this.url = String(url);
    FakeEventSource.instances.push(this);
  }

  addEventListener(type: string, fn: (e: MessageEvent) => void) {
    this.listeners.set(type, [...(this.listeners.get(type) ?? []), fn]);
  }

  close() {
    this.closed = true;
  }

  emit(type: string, data: unknown) {
    for (const fn of this.listeners.get(type) ?? []) fn(new MessageEvent(type, { data: JSON.stringify(data) }));
  }

  static current(): FakeEventSource {
    const open = FakeEventSource.instances.filter((s) => !s.closed);
    expect(open.length).toBe(1);
    return open[0] as FakeEventSource;
  }
}

function channel(id: string, overrides: Record<string, unknown> = {}) {
  return {
    id,
    title: `${id} title`,
    direction: "publish",
    protocol: "kafka",
    address: `shop.${id}`,
    message_names: ["Msg"],
    content_type: "application/json",
    enabled: true,
    triggers: ["manual"],
    settings_source: "contract",
    reply: null,
    activity: { published: 0, received: 0, last_published_at: null, last_received_at: null, last_error: null },
    ...overrides,
  };
}

const CHANNELS = [
  channel("orderCreated", {
    triggers: [{ schedule: { every_ms: 5000 } }, { cron: { expr: "0 * * * *" } }],
    activity: {
      published: 12,
      received: 0,
      last_published_at: "2026-10-16T10:00:00Z",
      last_received_at: null,
      last_error: "broker unreachable",
    },
  }),
  channel("orderShipped", {
    triggers: [{ on_state_change: { resource_type: "order", kind: "upserted" } }],
  }),
  channel("paymentReceived", {
    direction: "subscribe",
    protocol: "amqp",
    triggers: [],
    activity: { published: 0, received: 3, last_published_at: null, last_received_at: null, last_error: null },
  }),
];

const BROKERS = [
  { name: "kafka-main", kind: "kafka", url: "kafka://k:9092", status: "connected" },
  { name: "mem", kind: "memory", url: "memory://", status: "connected" },
];

function routes(overrides: Record<string, Answer> = {}, me = localMe, mock = mockView("shop", { active_version: 1 })) {
  return {
    ...shell(me),
    [`GET ${MOCK}`]: mock,
    [`GET ${MOCK}/channels`]: CHANNELS,
    [`GET ${MOCK}/event-settings`]: { broker: null, source: "api" },
    "GET /api/v1/brokers": BROKERS,
    ...Object.fromEntries(
      CHANNELS.map((c) => [`GET ${MOCK}/channels/${c.id}`, { ...c, message_schema: {}, parameters: {} }]),
    ),
    ...overrides,
  };
}

async function openTab() {
  renderAt("/w/default/mocks/shop/channels");
  const table = await screen.findByRole("table", { name: "Channels" });
  await within(table).findByText("orderCreated");
  return table;
}

function row(table: HTMLElement, id: string): HTMLElement {
  return within(table).getByText(id).closest("tr") as HTMLElement;
}

beforeEach(() => {
  FakeEventSource.instances = [];
  vi.stubGlobal("EventSource", FakeEventSource);
});

describe("channels", () => {
  it("lists channels with trigger summaries and activity", async () => {
    serve(routes());
    const table = await openTab();
    // The tab sits after the live requests.
    const tabs = within(screen.getByRole("navigation", { name: "Mock" }))
      .getAllByRole("link")
      .map((l) => l.textContent);
    expect(tabs.indexOf("Channels")).toBe(tabs.indexOf("Live requests") + 1);

    const created = row(table, "orderCreated");
    expect(within(created).getByText("orderCreated title")).toBeTruthy();
    expect(within(created).getByText("Mock sends").getAttribute("title")).toMatch(/AsyncAPI 2 subscribe operation/);
    expect(within(created).getByText("kafka")).toBeTruthy();
    expect(within(created).getByText("shop.orderCreated")).toBeTruthy();
    expect(within(created).getByText("every 5s")).toBeTruthy();
    expect(within(created).getByText("cron 0 * * * *")).toBeTruthy();
    expect(within(created).getByText("12 published")).toBeTruthy();
    expect(within(created).getByText("Last error:")).toBeTruthy();
    expect(within(created).getByText("broker unreachable")).toBeTruthy();
    expect(within(row(table, "orderShipped")).getByText("on order upserted")).toBeTruthy();

    const payment = row(table, "paymentReceived");
    expect(within(payment).getByText("Mock receives")).toBeTruthy();
    expect(within(payment).getByText("3 received")).toBeTruthy();
    // Triggers and manual publishing apply to publish channels only.
    expect(within(payment).queryByRole("button", { name: /Edit triggers/ })).toBeNull();
    expect(within(payment).queryByRole("button", { name: /Publish now/ })).toBeNull();
    expect(within(created).getByRole("button", { name: "Edit triggers of orderCreated" })).toBeTruthy();
    expect(within(created).getByRole("button", { name: "Publish now on orderCreated" })).toBeTruthy();
  });

  it("toggle enabled sends put", async () => {
    const user = userEvent.setup();
    const calls = serve(
      routes({
        [`PUT ${MOCK}/channels/orderShipped`]: (req: Request) =>
          req.json().then((b) => json({ ...channel("orderShipped"), ...b, settings_source: "api" })),
      }),
    );
    const table = await openTab();
    const toggle = within(row(table, "orderShipped")).getByRole("switch", { name: "Enabled orderShipped" });
    expect(toggle.getAttribute("aria-checked")).toBe("true");
    await user.click(toggle);
    await waitFor(() => expect(calls.some((c) => c.method === "PUT")).toBe(true));
    const put = calls.find((c) => c.method === "PUT");
    expect(put?.path).toBe(`${MOCK}/channels/orderShipped`);
    // A contract-sourced channel keeps the contract's triggers.
    expect(JSON.parse(put?.body ?? "")).toEqual({ enabled: false, triggers: null });
  });

  it("trigger editor round trips mcm triggers", async () => {
    const drafts: TriggerDraft[] = [
      { kind: "schedule", every_ms: 250 },
      { kind: "cron", expr: "*/5 * * * *" },
      { kind: "on_state_change", resource_type: "order", change: "deleted" },
      { kind: "manual" },
    ];
    expect(drafts.map(toMcmTrigger)).toEqual([
      { schedule: { every_ms: 250 } },
      { cron: { expr: "*/5 * * * *" } },
      { on_state_change: { resource_type: "order", kind: "deleted" } },
      "manual",
    ]);
    expect(drafts.map(toMcmTrigger).map(fromMcmTrigger)).toEqual(drafts);
    expect(fromMcmTrigger({ on_state_change: { resource_type: "x" } })).toEqual({
      kind: "on_state_change",
      resource_type: "x",
      change: "any",
    });
    expect(fromMcmTrigger({ bogus: {} })).toBeNull();
    expect(fromMcmTrigger(42)).toBeNull();

    const user = userEvent.setup();
    const calls = serve(
      routes({
        [`GET ${MOCK}/channels/orderCreated`]: {
          ...CHANNELS[0],
          settings_source: "api",
          message_schema: {},
          parameters: {},
        },
        [`PUT ${MOCK}/channels/orderCreated`]: (req: Request) =>
          req.json().then((b) => json({ ...channel("orderCreated"), ...b, settings_source: "api" })),
      }),
    );
    const table = await openTab();
    await user.click(within(row(table, "orderCreated")).getByRole("button", { name: "Edit triggers of orderCreated" }));
    const editor = await screen.findByRole("dialog", { name: "Triggers of orderCreated" });
    const first = await within(editor).findByRole("group", { name: "Trigger 1" });
    expect((within(first).getByLabelText("Every (ms)") as HTMLInputElement).value).toBe("5000");
    const second = within(editor).getByRole("group", { name: "Trigger 2" });
    expect((within(second).getByLabelText("Cron expression") as HTMLInputElement).value).toBe("0 * * * *");

    // Remove the cron trigger, add a state-change one.
    await user.click(within(second).getByRole("button", { name: "Remove trigger 2" }));
    await user.click(within(editor).getByRole("button", { name: "Add trigger" }));
    const added = within(editor).getByRole("group", { name: "Trigger 2" });
    // A new trigger starts as Manual, and the editor says what that means.
    const kind = within(added).getByLabelText<HTMLSelectElement>("Kind");
    expect(kind.value).toBe("manual");
    expect(kind.getAttribute("aria-describedby")).toBeTruthy();
    expect(within(added).getByText(/Publishes only when someone clicks Publish now/)).toBeTruthy();
    await user.selectOptions(within(added).getByLabelText("Kind"), "on_state_change");
    await user.type(within(added).getByLabelText("Resource type"), "order");
    await user.selectOptions(within(added).getByLabelText("Change"), "deleted");
    await user.click(within(editor).getByRole("button", { name: "Save" }));

    await waitFor(() => expect(calls.some((c) => c.method === "PUT")).toBe(true));
    const put = calls.find((c) => c.method === "PUT");
    expect(JSON.parse(put?.body ?? "")).toEqual({
      enabled: true,
      triggers: [{ schedule: { every_ms: 5000 } }, { on_state_change: { resource_type: "order", kind: "deleted" } }],
    });
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
  });

  it("trigger editor rejects schedule under 100ms", async () => {
    const user = userEvent.setup();
    const calls = serve(
      routes({
        [`PUT ${MOCK}/channels/orderCreated`]: () => problem(422, "Unprocessable Entity", "triggers[0]: bad"),
      }),
    );
    const table = await openTab();
    await user.click(within(row(table, "orderCreated")).getByRole("button", { name: "Edit triggers of orderCreated" }));
    const editor = await screen.findByRole("dialog", { name: "Triggers of orderCreated" });
    await user.click(within(editor).getByRole("checkbox", { name: "Use the contract's triggers" }));
    const first = await within(editor).findByRole("group", { name: "Trigger 1" });
    const every = within(first).getByLabelText("Every (ms)");
    await user.clear(every);
    await user.type(every, "50");
    expect(within(first).getByText("The interval must be at least 100 ms.")).toBeTruthy();
    expect((within(editor).getByRole("button", { name: "Save" }) as HTMLButtonElement).disabled).toBe(true);

    // The API takes whole milliseconds only.
    await user.clear(every);
    await user.type(every, "150.5");
    expect(within(first).getByText("The interval must be a whole number of ms, at least 100.")).toBeTruthy();
    expect((within(editor).getByRole("button", { name: "Save" }) as HTMLButtonElement).disabled).toBe(true);

    // A cron trigger needs an expression.
    const cron = within(editor).getByRole("group", { name: "Trigger 2" });
    await user.clear(within(cron).getByLabelText("Cron expression"));
    expect(within(cron).getByText("A cron expression is required.")).toBeTruthy();

    // Valid again: the server's 422 is shown.
    await user.clear(every);
    await user.type(every, "100");
    await user.type(within(cron).getByLabelText("Cron expression"), "0 * * * *");
    await user.click(within(editor).getByRole("button", { name: "Save" }));
    expect((await within(editor).findByRole("alert")).textContent).toContain("triggers[0]: bad");
    expect(calls.filter((c) => c.method === "PUT").length).toBe(1);
  });

  it("use contract triggers sends null", async () => {
    const user = userEvent.setup();
    const custom = { ...CHANNELS[1], settings_source: "api", triggers: [{ cron: { expr: "0 0 * * *" } }] };
    const calls = serve(
      routes({
        [`GET ${MOCK}/channels`]: [CHANNELS[0], custom, CHANNELS[2]],
        [`GET ${MOCK}/channels/orderShipped`]: { ...custom, message_schema: {}, parameters: {} },
        [`PUT ${MOCK}/channels/orderShipped`]: (req: Request) =>
          req.json().then((b) => json({ ...custom, ...b })),
      }),
    );
    const table = await openTab();
    await user.click(within(row(table, "orderShipped")).getByRole("button", { name: "Edit triggers of orderShipped" }));
    const editor = await screen.findByRole("dialog", { name: "Triggers of orderShipped" });
    const useContract = await within(editor).findByRole("checkbox", { name: "Use the contract's triggers" });
    expect((useContract as HTMLInputElement).checked).toBe(false);
    await user.click(useContract);
    expect(within(editor).queryByRole("group", { name: "Trigger 1" })).toBeNull();
    await user.click(within(editor).getByRole("button", { name: "Save" }));
    await waitFor(() => expect(calls.some((c) => c.method === "PUT")).toBe(true));
    const put = calls.find((c) => c.method === "PUT");
    expect(JSON.parse(put?.body ?? "")).toEqual({ enabled: true, triggers: null });
  });

  it("publish dialog shows published payloads", async () => {
    const user = userEvent.setup();
    const calls = serve(
      routes({
        [`POST ${MOCK}/channels/orderCreated/publish`]: {
          messages: [
            { address: "shop.orderCreated", key: "o-1", headers: {}, payload: { id: "o-1", total: 3 }, source: "llm" },
            { address: "shop.orderCreated", key: null, headers: {}, payload: { id: "o-2" }, source: "fallback" },
          ],
        },
        [`POST ${MOCK}/channels/orderShipped/publish`]: () => problem(502, "Broker publish failed", "connection refused"),
      }),
    );
    const table = await openTab();
    await user.click(within(row(table, "orderCreated")).getByRole("button", { name: "Publish now on orderCreated" }));
    const dialog = await screen.findByRole("dialog", { name: "Publish on orderCreated" });
    const count = within(dialog).getByLabelText("Count");
    await user.clear(count);
    await user.type(count, "2");
    await user.type(within(dialog).getByLabelText("Context (JSON object)"), '{{"region": "eu"}');
    await user.click(within(dialog).getByRole("button", { name: "Publish" }));

    const payloads = await within(dialog).findAllByLabelText("Published payload");
    expect(payloads.map((p) => p.textContent)).toEqual([
      JSON.stringify({ id: "o-1", total: 3 }, null, 2),
      JSON.stringify({ id: "o-2" }, null, 2),
    ]);
    expect(within(dialog).getAllByText("shop.orderCreated").length).toBe(2);
    expect(within(dialog).getByText("o-1")).toBeTruthy();
    expect(within(dialog).getByText("llm")).toBeTruthy();
    expect(within(dialog).getByText("fallback")).toBeTruthy();
    const post = calls.find((c) => c.method === "POST");
    expect(JSON.parse(post?.body ?? "")).toEqual({ count: 2, context: { region: "eu" } });
    await user.click(within(dialog).getByRole("button", { name: "Close" }));

    // A 502 is shown inline.
    await user.click(within(row(table, "orderShipped")).getByRole("button", { name: "Publish now on orderShipped" }));
    const other = await screen.findByRole("dialog", { name: "Publish on orderShipped" });
    await user.click(within(other).getByRole("button", { name: "Publish" }));
    const alert = await within(other).findByRole("alert");
    expect(alert.textContent).toContain("Broker publish failed");
    expect(alert.textContent).toContain("connection refused");
  });

  it("publish context must be a json object", async () => {
    const user = userEvent.setup();
    const calls = serve(routes({ [`POST ${MOCK}/channels/orderCreated/publish`]: { messages: [] } }));
    const table = await openTab();
    await user.click(within(row(table, "orderCreated")).getByRole("button", { name: "Publish now on orderCreated" }));
    const dialog = await screen.findByRole("dialog", { name: "Publish on orderCreated" });
    const context = within(dialog).getByLabelText("Context (JSON object)");
    const submit = within(dialog).getByRole("button", { name: "Publish" }) as HTMLButtonElement;
    await user.type(context, "[[1, 2]");
    expect(within(dialog).getByText("Context must be a JSON object.")).toBeTruthy();
    expect(submit.disabled).toBe(true);
    await user.clear(context);
    await user.type(context, "{{oops");
    expect(within(dialog).getByText(/Context is not valid JSON/)).toBeTruthy();
    expect(submit.disabled).toBe(true);
    // Count stays within 1-100.
    await user.clear(context);
    const count = within(dialog).getByLabelText("Count");
    await user.clear(count);
    await user.type(count, "101");
    expect(within(dialog).getByText("Count must be between 1 and 100.")).toBeTruthy();
    expect(submit.disabled).toBe(true);
    await user.clear(count);
    await user.type(count, "1");
    expect(submit.disabled).toBe(false);
    await user.click(submit);
    await waitFor(() => expect(calls.some((c) => c.method === "POST")).toBe(true));
    expect(JSON.parse(calls.find((c) => c.method === "POST")?.body ?? "")).toEqual({ count: 1 });
  });

  it("broker selector saves event settings", async () => {
    const user = userEvent.setup();
    const calls = serve(
      routes({
        [`PUT ${MOCK}/event-settings`]: (req: Request) => req.json().then((b) => json({ ...b, source: "api" })),
      }),
    );
    await openTab();
    const broker = screen.getByLabelText("Broker") as HTMLSelectElement;
    await waitFor(() => expect(broker.options.length).toBe(3));
    expect(Array.from(broker.options).map((o) => o.textContent)).toEqual([
      "Automatic",
      "kafka-main (kafka, connected)",
      "mem (memory, connected)",
    ]);
    expect(broker.value).toBe("");
    await user.selectOptions(broker, "mem");
    await waitFor(() => expect(calls.some((c) => c.method === "PUT")).toBe(true));
    expect(JSON.parse(calls.find((c) => c.method === "PUT")?.body ?? "")).toEqual({ broker: "mem" });
    await user.selectOptions(broker, "");
    await waitFor(() => expect(calls.filter((c) => c.method === "PUT").length).toBe(2));
    expect(JSON.parse(calls.filter((c) => c.method === "PUT")[1]?.body ?? "")).toEqual({ broker: null });
  });

  it("event feed shows streamed event records", async () => {
    const user = userEvent.setup();
    serve(routes());
    await openTab();
    const feed = screen.getByRole("table", { name: "Event feed" });
    await waitFor(() => expect(FakeEventSource.instances.length).toBe(1));
    expect(FakeEventSource.current().url).toBe(`${MOCK}/requests/stream?kind=event`);
    const event = (n: number, method: string, source: string | null) => ({
      at: "2026-10-16T10:00:00Z",
      workspace: "default",
      mock: "shop",
      operation_id: `channel${n}`,
      session: "s",
      method,
      path: "shop.orders",
      status: 200,
      source,
      latency_ms: 1,
      prompt_tokens: 0,
      completion_tokens: 0,
      request: {},
      response: null,
      kind: "event",
    });
    act(() => {
      FakeEventSource.current().emit("request", event(1, "PUBLISH", "llm"));
      FakeEventSource.current().emit("request", event(2, "RECEIVE", null));
    });
    const rows = Array.from((feed as HTMLTableElement).tBodies[0]?.rows ?? []);
    expect(rows.length).toBe(2);
    // Newest first.
    expect(within(rows[0] as HTMLElement).getByText("channel2")).toBeTruthy();
    expect(within(rows[0] as HTMLElement).getByText("Mock receives")).toBeTruthy();
    expect(within(rows[1] as HTMLElement).getByText("Mock sends")).toBeTruthy();
    expect(within(rows[1] as HTMLElement).getByText("llm")).toBeTruthy();
    expect(within(rows[1] as HTMLElement).getByText("200")).toBeTruthy();

    // Paused: new events are not shown.
    await user.click(screen.getByRole("button", { name: "Pause" }));
    act(() => FakeEventSource.current().emit("request", event(3, "PUBLISH", "llm")));
    expect((feed as HTMLTableElement).tBodies[0]?.rows.length).toBe(2);
    await user.click(screen.getByRole("button", { name: "Resume" }));

    // Capped at 200 rows.
    act(() => {
      for (let i = 0; i < 250; i += 1) FakeEventSource.current().emit("request", event(100 + i, "PUBLISH", "llm"));
    });
    expect((feed as HTMLTableElement).tBodies[0]?.rows.length).toBe(200);
  }, 15_000);

  it("file mock is read only", async () => {
    const user = userEvent.setup();
    const calls = serve(routes({}, localMe, mockView("shop", { active_version: 1, source: "file" })));
    const table = await openTab();
    const created = row(table, "orderCreated");
    expect(screen.getAllByText("Managed by config file").length).toBeGreaterThan(0);
    const toggle = within(created).getByRole("switch", { name: "Enabled orderCreated" }) as HTMLButtonElement;
    expect(toggle.disabled).toBe(true);
    expect((within(created).getByRole("button", { name: "Edit triggers of orderCreated" }) as HTMLButtonElement).disabled).toBe(true);
    expect((within(created).getByRole("button", { name: "Publish now on orderCreated" }) as HTMLButtonElement).disabled).toBe(true);
    expect((screen.getByLabelText("Broker") as HTMLSelectElement).disabled).toBe(true);
    await user.click(toggle);
    expect(calls.some((c) => c.method !== "GET")).toBe(false);
  });

  it("viewer is read only", async () => {
    serve(routes({}, viewerMe));
    const table = await openTab();
    expect(screen.getByText("Read-only: changing channels needs the editor role.")).toBeTruthy();
    const toggle = within(row(table, "orderCreated")).getByRole("switch", { name: "Enabled orderCreated" });
    expect((toggle as HTMLButtonElement).disabled).toBe(true);
  });

  it("empty state without channels", async () => {
    serve(routes({ [`GET ${MOCK}/channels`]: [] }));
    renderAt("/w/default/mocks/shop/channels");
    expect(await screen.findByText("This mock's active contract declares no channels.")).toBeTruthy();
    expect(screen.queryByRole("table", { name: "Channels" })).toBeNull();
  });
});

describe("channel copy", () => {
  it("labels directions from the mock's point of view, consistently", () => {
    expect(directionLabel("publish").label).toBe("Mock sends");
    expect(directionLabel("PUBLISH").label).toBe("Mock sends");
    expect(directionLabel("subscribe").label).toBe("Mock receives");
    expect(directionLabel("RECEIVE").label).toBe("Mock receives");
    expect(directionLabel("other").label).toBe("other");
  });

  it("trigger errors are sentences", () => {
    for (const draft of [
      { kind: "schedule", every_ms: 5 },
      { kind: "schedule", every_ms: 150.5 },
      { kind: "cron", expr: " " },
      { kind: "on_state_change", resource_type: "", change: "any" },
    ] as const) {
      expect(triggerError(draft)).toMatch(/^[A-Z].*\.$/);
    }
  });
});
