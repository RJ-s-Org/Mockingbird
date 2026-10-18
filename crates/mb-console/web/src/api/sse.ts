import { api } from "./client";
import type { components } from "./schema";

/** One served call, as the live tail sends it. */
export type LiveRequestView = components["schemas"]["LiveRequestView"];

/** One recorded call, as the request log returns it. */
export type RequestLogView = components["schemas"]["RequestLogView"];

/** How many recorded calls a live feed shows when it opens. */
export const HISTORY = 50;

/**
 * The latest `limit` recorded calls matching `filters`, newest first, shaped
 * like live records, so a feed can show recent history before its stream
 * delivers anything. A failed read gives no history rather than an error.
 */
export async function recentRequests(
  ws: string,
  mock: string,
  filters: LiveFilters,
  limit = HISTORY,
): Promise<LiveRequestView[]> {
  try {
    const { data, response } = await api.GET("/api/v1/workspaces/{ws}/mocks/{mock}/requests", {
      params: { path: { ws, mock }, query: { ...filters, limit } },
    });
    if (!response.ok || !Array.isArray(data)) return [];
    return data.map((entry) => {
      const record: Partial<RequestLogView> = { ...entry };
      delete record.id;
      return { ...(record as Omit<RequestLogView, "id">), workspace: ws, mock };
    });
  } catch {
    return [];
  }
}

/** Whether two records describe the same served call (live records carry no id). */
function sameCall(a: LiveRequestView, b: LiveRequestView): boolean {
  return (
    a.method === b.method &&
    a.path === b.path &&
    a.status === b.status &&
    a.session === b.session &&
    a.latency_ms === b.latency_ms &&
    (a.operation_id ?? null) === (b.operation_id ?? null) &&
    a.kind === b.kind
  );
}

/**
 * The `history` records older than everything already `streamed`: a call
 * served while the history loaded is both streamed and logged, so each
 * streamed record cancels one matching history record.
 */
export function olderThan(streamed: LiveRequestView[], history: LiveRequestView[]): LiveRequestView[] {
  const pending = [...streamed];
  return history.filter((h) => {
    const at = pending.findIndex((s) => sameCall(s, h));
    if (at < 0) return true;
    pending.splice(at, 1);
    return false;
  });
}

/** An event of the live tail: a served call, or how many this subscriber missed. */
export type LiveEvent = { kind: "request"; record: LiveRequestView } | { kind: "lagged"; dropped: number };

/** Filters of the live tail; empty values are left out of the query. */
export interface LiveFilters {
  operation?: string;
  session?: string;
  source?: string;
  /** Request kind, e.g. `event` for event-channel messages. */
  kind?: string;
}

/** The stream URL of a mock's live tail with `filters` as its query. */
export function streamUrl(ws: string, mock: string, filters: LiveFilters): string {
  const e = encodeURIComponent;
  const query = new URLSearchParams();
  for (const key of ["operation", "session", "source", "kind"] as const) {
    const value = filters[key];
    if (value) query.set(key, value);
  }
  const qs = query.toString();
  return `/api/v1/workspaces/${e(ws)}/mocks/${e(mock)}/requests/stream${qs ? `?${qs}` : ""}`;
}

/** `EventSource.CLOSED`: the browser will not reconnect. */
const CLOSED = 2;

function parse(data: unknown): unknown {
  try {
    return typeof data === "string" ? JSON.parse(data) : undefined;
  } catch {
    return undefined;
  }
}

/**
 * Subscribes to a mock's live tail (`GET …/requests/stream`) with an
 * `EventSource`, which sends the session cookie and reconnects on its own.
 * `onError` is called whenever the connection drops. When the browser gives
 * up (the stream is `CLOSED`, e.g. after a 401 the stream cannot report), the
 * session is checked with `GET /api/v1/me`, whose 401 sends the browser to
 * `/auth/login?return_to=<current path>` (see the API client). Returns
 * `close()`.
 */
export function subscribeRequests(
  ws: string,
  mock: string,
  filters: LiveFilters,
  onEvent: (e: LiveEvent) => void,
  onError: () => void,
): () => void {
  const source = new EventSource(streamUrl(ws, mock, filters));
  source.addEventListener("request", (e: MessageEvent) => {
    const record = parse(e.data);
    if (typeof record === "object" && record !== null) onEvent({ kind: "request", record: record as LiveRequestView });
  });
  source.addEventListener("lagged", (e: MessageEvent) => {
    const body = parse(e.data) as { dropped?: unknown } | undefined;
    const dropped = Number(body?.dropped);
    if (Number.isFinite(dropped)) onEvent({ kind: "lagged", dropped });
  });
  source.onerror = () => {
    onError();
    if (source.readyState === CLOSED) {
      // A 401 redirects to the login in the client's middleware; any other
      // failure leaves the page as it is.
      api.GET("/api/v1/me").catch(() => {});
    }
  };
  return () => source.close();
}
