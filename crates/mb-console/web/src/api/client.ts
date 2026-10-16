import createClient, { type Client, type Middleware } from "openapi-fetch";
import type { paths } from "./schema";

/** An RFC 9457 problem, as the Admin API sends it. */
export interface Problem {
  title: string;
  detail: string;
  status: number;
}

/** A failed Admin API call; `problem` is the server's problem body, if any. */
export class ApiError extends Error {
  status: number;
  problem?: Problem;

  constructor(status: number, problem?: Problem) {
    super(problem?.detail || problem?.title || `Request failed with status ${status}`);
    this.name = "ApiError";
    this.status = status;
    this.problem = problem;
  }
}

/** Where to send the browser to sign in and come back to `location`. */
export function loginUrl(location: Pick<Location, "pathname" | "search">): string {
  return "/auth/login?return_to=" + encodeURIComponent(location.pathname + location.search);
}

function isProblem(v: unknown): v is Problem {
  return typeof v === "object" && v !== null && "title" in v && "status" in v;
}

function toProblem(v: unknown): Problem | undefined {
  if (!isProblem(v)) return undefined;
  const p = v as Partial<Problem>;
  return { title: String(p.title ?? ""), detail: String(p.detail ?? ""), status: Number(p.status) };
}

async function readProblem(response: Response): Promise<Problem | undefined> {
  try {
    return toProblem(await response.clone().json());
  } catch {
    return undefined;
  }
}

const SAFE_METHODS = new Set(["GET", "HEAD", "OPTIONS"]);

/** Set once the browser has been sent to the login page, so it happens once. */
let redirectingToLogin = false;

const middleware: Middleware = {
  onRequest({ request }) {
    // The Admin API refuses mutations without a non-simple Content-Type.
    if (!SAFE_METHODS.has(request.method) && !request.headers.has("Content-Type")) {
      request.headers.set("Content-Type", "application/json");
    }
    return request;
  },
  async onResponse({ request, response }) {
    if (response.status !== 401 || !new URL(request.url).pathname.startsWith("/api/")) {
      return response;
    }
    if (!redirectingToLogin) {
      redirectingToLogin = true;
      window.location.href = loginUrl(window.location);
    }
    throw new ApiError(401, await readProblem(response));
  },
};

/** Resolves relative URLs against the page, as browsers do (Node does not). */
class PageRequest extends Request {
  constructor(input: RequestInfo | URL, init?: RequestInit) {
    super(typeof input === "string" ? new URL(input, window.location.origin) : input, init);
  }
}

/** The typed Admin API client, on the page's own origin. */
export const api: Client<paths> = createClient<paths>({
  baseUrl: "",
  Request: PageRequest,
  // Looked up per call, so tests can stub `fetch`.
  fetch: (request: Request) => globalThis.fetch(request),
});
api.use(middleware);

/** The data of an openapi-fetch result; throws [`ApiError`] on an error response. */
export function unwrap<T>(r: { data?: T; error?: unknown; response: Response }): T {
  if (r.error !== undefined || !r.response.ok) {
    throw new ApiError(r.response.status, toProblem(r.error));
  }
  return r.data as T;
}
