import { api } from "./client";
import type { components } from "./schema";

/** One served call, as the live tail sends it. */
export type LiveRequestView = components["schemas"]["LiveRequestView"];

/** An event of the live tail: a served call, or how many this subscriber missed. */
export type LiveEvent = { kind: "request"; record: LiveRequestView } | { kind: "lagged"; dropped: number };

/** Filters of the live tail; empty values are left out of the query. */
export interface LiveFilters {
  operation?: string;
  session?: string;
  source?: string;
}

/** The stream URL of a mock's live tail with `filters` as its query. */
export function streamUrl(ws: string, mock: string, filters: LiveFilters): string {
  const e = encodeURIComponent;
  const query = new URLSearchParams();
  for (const key of ["operation", "session", "source"] as const) {
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
