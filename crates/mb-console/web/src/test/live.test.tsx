import { act, cleanup, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { localMe, mockView, renderAt, serve, shell } from "./harness";

const MOCK = "/api/v1/workspaces/default/mocks/pets";

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

  removeEventListener(type: string, fn: (e: MessageEvent) => void) {
    this.listeners.set(
      type,
      (this.listeners.get(type) ?? []).filter((f) => f !== fn),
    );
  }

  close() {
    this.closed = true;
  }

  emit(type: string, data: unknown) {
    for (const fn of this.listeners.get(type) ?? []) fn(new MessageEvent(type, { data: JSON.stringify(data) }));
  }

  static open(): FakeEventSource[] {
    return FakeEventSource.instances.filter((s) => !s.closed);
  }

  /** The one open stream. */
  static current(): FakeEventSource {
    const open = FakeEventSource.open();
    expect(open.length).toBe(1);
    return open[0] as FakeEventSource;
  }
}

let seq = 0;
function record(overrides: Record<string, unknown> = {}) {
  seq += 1;
  return {
    at: "2026-10-16T10:00:00Z",
    workspace: "default",
    mock: "pets",
    method: "GET",
    path: `/pets/orders/${seq}`,
    status: 200,
    source: "llm",
    latency_ms: 12,
    session: "s1",
    operation_id: "getOrder",
    prompt_tokens: 0,
    completion_tokens: 0,
    request: { headers: {} },
    response: { body: { id: seq } },
    ...overrides,
  };
}

function push(type: string, data: unknown) {
  act(() => FakeEventSource.current().emit(type, data));
}

const contract = {
  version: 1,
  status: "active",
  source_digest: "1".repeat(64),
  created_by: null,
  created_at: "2026-10-16T00:00:00Z",
  title: "Orders",
  api_version: "1.0.0",
  operations: [
    { id: "listOrders", method: "GET", path: "/orders", kind: "query" },
    { id: "getOrder", method: "GET", path: "/orders/{orderId}", kind: "query" },
  ],
};

async function open() {
  serve({
    ...shell(localMe),
    [`GET ${MOCK}`]: mockView("pets", { active_version: 1 }),
    [`GET ${MOCK}/contracts/1`]: contract,
  });
  renderAt("/w/default/mocks/pets/requests");
  const table = await screen.findByRole("table", { name: "Live requests" });
  await waitFor(() => expect(FakeEventSource.open().length).toBe(1));
  return table;
}

/**
 * The table's body rows. A plain DOM query rather than `getAllByRole("row")`:
 * role queries compute the accessibility tree of every row, which took
 * seconds over the 500-row cap test and made it time out under load.
 */
function bodyRows(table: HTMLElement): HTMLTableRowElement[] {
  return Array.from((table as HTMLTableElement).tBodies[0]?.rows ?? []);
}

beforeEach(() => {
  FakeEventSource.instances = [];
  vi.stubGlobal("EventSource", FakeEventSource);
});

describe("live requests", () => {
  it("opens with the latest recorded requests, then streams new ones on top without duplicates", async () => {
    /** A live record as the request log returns it: an id, no workspace or mock. */
    const asLogged = (r: Record<string, unknown>, id: number, at: string) => {
      const entry: Record<string, unknown> = { id, ...r, at, kind: "http" };
      delete entry.workspace;
      delete entry.mock;
      return entry;
    };
    const logged = (id: number, path: string, at: string) => asLogged(record({ path }), id, at);
    let release: (r: Response) => void = () => {};
    const calls = serve({
      ...shell(localMe),
      [`GET ${MOCK}`]: mockView("pets", { active_version: 1 }),
      [`GET ${MOCK}/contracts/1`]: contract,
      [`GET ${MOCK}/requests`]: () => new Promise<Response>((resolve) => (release = resolve)),
    });
    renderAt("/w/default/mocks/pets/requests");
    const table = await screen.findByRole("table", { name: "Live requests" });
    await waitFor(() => expect(FakeEventSource.open().length).toBe(1));
    expect(screen.getByText("Loading recent requests…")).toBeTruthy();
    await waitFor(() => expect(calls.some((c) => c.path === `${MOCK}/requests`)).toBe(true));
    expect(new URLSearchParams(calls.find((c) => c.path === `${MOCK}/requests`)?.search).get("limit")).toBe("50");

    // A call served while the history loads arrives on the stream and is also in the log.
    const live = record({ path: "/pets/orders/live", kind: "http" });
    push("request", live);
    const history = [
      asLogged(live, 3, "2026-10-16T09:59:59Z"),
      logged(2, "/pets/orders/older", "2026-10-16T09:59:00Z"),
      logged(1, "/pets/orders/oldest", "2026-10-16T09:58:00Z"),
    ];
    await act(async () => release(new Response(JSON.stringify(history), { headers: { "Content-Type": "application/json" } })));
    await waitFor(() => expect(bodyRows(table).length).toBe(3));
    const paths = bodyRows(table).map((r) => r.querySelectorAll("td")[2]?.textContent);
    expect(paths).toEqual(["/pets/orders/live", "/pets/orders/older", "/pets/orders/oldest"]);
    expect(screen.getByText(/Showing the latest recorded requests, then new ones as they arrive/)).toBeTruthy();

    push("request", record({ path: "/pets/orders/newest" }));
    expect(bodyRows(table)[0]?.textContent).toContain("/pets/orders/newest");
  });

  it("with no recorded requests the empty feed says new ones will appear", async () => {
    serve({
      ...shell(localMe),
      [`GET ${MOCK}`]: mockView("pets", { active_version: 1 }),
      [`GET ${MOCK}/contracts/1`]: contract,
      [`GET ${MOCK}/requests`]: [],
    });
    renderAt("/w/default/mocks/pets/requests");
    expect(await screen.findByText("No requests recorded yet. New ones appear here as they are served.")).toBeTruthy();
  });

  it("live table prepends events and caps at 500", async () => {
    const table = await open();
    expect(FakeEventSource.current().url).toBe(`${MOCK}/requests/stream`);

    push("request", record({ path: "/pets/orders/first", status: 201, source: "fallback", latency_ms: 7 }));
    const first = bodyRows(table)[0] as HTMLElement;
    expect(within(first).getByText("/pets/orders/first")).toBeTruthy();
    expect(within(first).getByText("201")).toBeTruthy();
    expect(within(first).getByText("fallback")).toBeTruthy();
    expect(within(first).getByText("7 ms")).toBeTruthy();
    expect(within(first).getByText("s1")).toBeTruthy();

    act(() => {
      const s = FakeEventSource.current();
      for (let i = 0; i < 500; i += 1) s.emit("request", record({ path: `/pets/orders/n${i}` }));
    });
    const rows = bodyRows(table);
    expect(rows.length).toBe(500);
    expect(within(rows[0] as HTMLElement).getByText("/pets/orders/n499")).toBeTruthy();
    expect(within(rows[499] as HTMLElement).getByText("/pets/orders/n0")).toBeTruthy();
    // The oldest row was dropped (a text scan, not a role query).
    expect(rows.some((r) => r.textContent?.includes("/pets/orders/first"))).toBe(false);
    // Rendering 500 rows in jsdom takes about 1 s on an idle machine (2 s
    // before `bodyRows` stopped using role queries), and timed out at the
    // default 5 s while cargo builds loaded every core; the assertions are
    // unchanged, only the budget for the unavoidable rendering is wider.
  }, 15_000);

  it("pause stops rendering new rows", async () => {
    const user = userEvent.setup();
    const table = await open();
    push("request", record({ path: "/pets/orders/before" }));
    await user.click(screen.getByRole("button", { name: "Pause" }));
    push("request", record({ path: "/pets/orders/during1" }));
    push("request", record({ path: "/pets/orders/during2" }));
    expect(bodyRows(table).length).toBe(1);
    expect(screen.getByText("2 requests arrived while paused.")).toBeTruthy();

    await user.click(screen.getByRole("button", { name: "Resume" }));
    expect(screen.queryByText(/while paused/)).toBeNull();
    push("request", record({ path: "/pets/orders/after" }));
    const rows = bodyRows(table);
    expect(rows.length).toBe(2);
    expect(within(rows[0] as HTMLElement).getByText("/pets/orders/after")).toBeTruthy();
  });

  it("lagged banner shows dropped count", async () => {
    const user = userEvent.setup();
    await open();
    push("lagged", { dropped: 42 });
    expect(screen.getByText("42 requests were dropped")).toBeTruthy();
    await user.click(screen.getByRole("button", { name: "Dismiss" }));
    expect(screen.queryByText(/were dropped/)).toBeNull();
  });

  it("filters re-subscribe with query", async () => {
    const user = userEvent.setup();
    await open();
    const operation = screen.getByLabelText("Operation") as HTMLSelectElement;
    await waitFor(() => expect(operation.options.length).toBe(3));
    await user.selectOptions(operation, "getOrder");
    await waitFor(() =>
      expect(FakeEventSource.current().url).toBe(`${MOCK}/requests/stream?operation=getOrder`),
    );
    await user.selectOptions(screen.getByLabelText("Source"), "repaired");
    await user.type(screen.getByLabelText("Session"), "s9");
    await waitFor(() =>
      expect(FakeEventSource.current().url).toBe(
        `${MOCK}/requests/stream?operation=getOrder&session=s9&source=repaired`,
      ),
    );
    // Every superseded stream was closed.
    expect(FakeEventSource.instances.filter((s) => !s.closed).length).toBe(1);
    expect(FakeEventSource.instances.length).toBeGreaterThan(2);
  });

  it("clicking a row opens the request drawer", async () => {
    const user = userEvent.setup();
    const table = await open();
    push("request", record({ path: "/pets/orders/42", request: { query: "x=1" }, response: { body: { id: 42 } } }));
    push("request", record({ path: "/pets/orders/43" }));
    await user.click(within(table).getByRole("button", { name: /\/pets\/orders\/42/ }));
    const drawer = await screen.findByRole("dialog", { name: "GET /pets/orders/42" });
    const pres = drawer.querySelectorAll("pre");
    expect(pres[0]?.textContent).toBe(JSON.stringify({ query: "x=1" }, null, 2));
    expect(pres[1]?.textContent).toBe(JSON.stringify({ body: { id: 42 } }, null, 2));
  });

  it("rows are keyboard navigable with the arrow keys", async () => {
    const user = userEvent.setup();
    const table = await open();
    push("request", record({ path: "/pets/orders/a" }));
    push("request", record({ path: "/pets/orders/b" }));
    const buttons = within(table).getAllByRole("button");
    (buttons[0] as HTMLElement).focus();
    await user.keyboard("{ArrowDown}");
    expect(document.activeElement).toBe(buttons[1]);
    await user.keyboard("{ArrowUp}");
    expect(document.activeElement).toBe(buttons[0]);
    await user.keyboard("{Enter}");
    await screen.findByRole("dialog", { name: "GET /pets/orders/b" });
  });

  it("closes the stream on unmount", async () => {
    await open();
    const stream = FakeEventSource.current();
    cleanup();
    expect(stream.closed).toBe(true);
  });
});
