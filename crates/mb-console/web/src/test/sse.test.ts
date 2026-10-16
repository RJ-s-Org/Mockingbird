import { afterEach, describe, expect, it, vi } from "vitest";
import { subscribeRequests } from "../api/sse";

/** A minimal `EventSource` whose state tests set by hand. */
class FakeEventSource {
  static readonly CONNECTING = 0;
  static readonly OPEN = 1;
  static readonly CLOSED = 2;
  static last: FakeEventSource | undefined;
  readyState = FakeEventSource.CONNECTING;
  onerror: ((e: Event) => void) | null = null;

  constructor(public url: string) {
    FakeEventSource.last = this;
  }

  addEventListener() {}

  close() {
    this.readyState = FakeEventSource.CLOSED;
  }

  fail(readyState: number) {
    this.readyState = readyState;
    this.onerror?.(new Event("error"));
  }
}

function stubLocation(): string[] {
  const origin = window.location.origin;
  const assignments: string[] = [];
  vi.stubGlobal("location", {
    origin,
    pathname: "/w/default/mocks/pets/live",
    search: "?session=s1",
    get href() {
      return `${origin}/w/default/mocks/pets/live?session=s1`;
    },
    set href(v: string) {
      assignments.push(v);
    },
  });
  return assignments;
}

function json(status: number, body: unknown): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": status >= 400 ? "application/problem+json" : "application/json" },
  });
}

const me = { subject: "local", display_name: "Local", kind: "local", sysadmin: true, workspaces: [] };

describe("live tail stream", () => {
  afterEach(() => {
    FakeEventSource.last = undefined;
  });

  it("sends the browser to login when a closed stream's session expired", async () => {
    vi.stubGlobal("EventSource", FakeEventSource);
    const assignments = stubLocation();
    const fetchMock = vi.fn(async () => json(401, { title: "Unauthorized", status: 401, detail: "authentication required" }));
    vi.stubGlobal("fetch", fetchMock);
    const onError = vi.fn();
    subscribeRequests("default", "pets", {}, () => {}, onError);

    FakeEventSource.last?.fail(FakeEventSource.CLOSED);
    expect(onError).toHaveBeenCalledTimes(1);
    await vi.waitFor(() => expect(assignments).toEqual(["/auth/login?return_to=%2Fw%2Fdefault%2Fmocks%2Fpets%2Flive%3Fsession%3Ds1"]));
    const req = (fetchMock.mock.calls[0] as unknown as [Request])[0];
    expect(new URL(req.url).pathname).toBe("/api/v1/me");
  });

  it("stays put when the session is still valid", async () => {
    vi.stubGlobal("EventSource", FakeEventSource);
    const assignments = stubLocation();
    const fetchMock = vi.fn(async () => json(200, me));
    vi.stubGlobal("fetch", fetchMock);
    const onError = vi.fn();
    subscribeRequests("default", "pets", {}, () => {}, onError);

    FakeEventSource.last?.fail(FakeEventSource.CLOSED);
    await vi.waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(1));
    await new Promise((r) => setTimeout(r, 0));
    expect(assignments).toEqual([]);
    expect(onError).toHaveBeenCalledTimes(1);
  });

  it("does not check the session while the browser reconnects", () => {
    vi.stubGlobal("EventSource", FakeEventSource);
    stubLocation();
    const fetchMock = vi.fn(async () => json(200, me));
    vi.stubGlobal("fetch", fetchMock);
    const onError = vi.fn();
    subscribeRequests("default", "pets", {}, () => {}, onError);

    FakeEventSource.last?.fail(FakeEventSource.CONNECTING);
    expect(onError).toHaveBeenCalledTimes(1);
    expect(fetchMock).not.toHaveBeenCalled();
  });
});
