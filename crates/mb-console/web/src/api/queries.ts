import { queryOptions, useInfiniteQuery, useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api, unwrap } from "./client";
import type { components } from "./schema";

export type Workspace = components["schemas"]["WorkspaceView"];
export type CreateWorkspace = components["schemas"]["CreateWorkspace"];

export const workspacesQuery = queryOptions({
  queryKey: ["workspaces"] as const,
  queryFn: async (): Promise<Workspace[]> => unwrap(await api.GET("/api/v1/workspaces")),
});

/** Every workspace the caller can see. */
export function useWorkspaces() {
  return useQuery(workspacesQuery);
}

/** `POST /api/v1/workspaces` (sysadmin); refreshes the list and `me`. */
export function useCreateWorkspace() {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: async (body: CreateWorkspace): Promise<Workspace> =>
      unwrap(await api.POST("/api/v1/workspaces", { body })),
    onSuccess: async () => {
      await Promise.all([
        queryClient.invalidateQueries({ queryKey: workspacesQuery.queryKey }),
        queryClient.invalidateQueries({ queryKey: ["me"] }),
      ]);
    },
  });
}

const LAST_WORKSPACE = "mb.lastWorkspace";

/** The workspace the user last opened, if storage is available. */
export function lastWorkspace(): string | null {
  try {
    return window.localStorage.getItem(LAST_WORKSPACE);
  } catch {
    return null;
  }
}

/** Remembers `ws` as the last opened workspace (best effort). */
export function rememberWorkspace(ws: string): void {
  try {
    window.localStorage.setItem(LAST_WORKSPACE, ws);
  } catch {
    // Storage may be unavailable (private mode, blocked); nothing to do.
  }
}

export type Mock = components["schemas"]["MockView"];
export type CreateMock = components["schemas"]["CreateMock"];
export type PatchMock = components["schemas"]["PatchMock"];
export type DataPlaneAuthMode = components["schemas"]["DataPlaneAuthMode"];
export type ContractSummary = components["schemas"]["ContractVersionSummary"];
export type ContractDetail = components["schemas"]["ContractVersionDetail"];
export type UploadResult = components["schemas"]["UploadResult"];
export type Profile = components["schemas"]["LlmProfileView"];

/** Query keys of a workspace's resources; every key starts `["ws", ws]`. */
export const keys = {
  mocks: (ws: string) => ["ws", ws, "mocks"] as const,
  mock: (ws: string, mock: string) => ["ws", ws, "mocks", mock] as const,
  contracts: (ws: string, mock: string) => ["ws", ws, "mocks", mock, "contracts"] as const,
  contract: (ws: string, mock: string, version: number) => ["ws", ws, "mocks", mock, "contracts", version] as const,
  profiles: (ws: string) => ["ws", ws, "llm-profiles"] as const,
  apiKeys: (ws: string) => ["ws", ws, "api-keys"] as const,
  audit: (ws: string, filters: AuditFilters) => ["ws", ws, "audit", filters] as const,
  scenarios: (ws: string, mock: string) => ["ws", ws, "mocks", mock, "scenarios"] as const,
  sessions: (ws: string, mock: string) => ["ws", ws, "mocks", mock, "sessions"] as const,
  session: (ws: string, mock: string, session: string) => ["ws", ws, "mocks", mock, "sessions", session] as const,
};

/** The mocks of `ws`. */
export function useMocks(ws: string) {
  return useQuery({
    queryKey: keys.mocks(ws),
    queryFn: async (): Promise<Mock[]> =>
      unwrap(await api.GET("/api/v1/workspaces/{ws}/mocks", { params: { path: { ws } } })),
  });
}

/** One mock of `ws`. */
export function useMock(ws: string, mock: string) {
  return useQuery({
    queryKey: keys.mock(ws, mock),
    queryFn: async (): Promise<Mock> =>
      unwrap(await api.GET("/api/v1/workspaces/{ws}/mocks/{mock}", { params: { path: { ws, mock } } })),
  });
}

/** `POST …/mocks`; refreshes the list. */
export function useCreateMock(ws: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: async (body: CreateMock): Promise<Mock> =>
      unwrap(await api.POST("/api/v1/workspaces/{ws}/mocks", { params: { path: { ws } }, body })),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: keys.mocks(ws), exact: true }),
  });
}

/** `PATCH …/mocks/{mock}` with a merge patch; refreshes the mock and the list. */
export function usePatchMock(ws: string, mock: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: async (body: PatchMock): Promise<Mock> =>
      unwrap(await api.PATCH("/api/v1/workspaces/{ws}/mocks/{mock}", { params: { path: { ws, mock } }, body })),
    onSuccess: async (updated) => {
      queryClient.setQueryData(keys.mock(ws, mock), updated);
      await queryClient.invalidateQueries({ queryKey: keys.mocks(ws), exact: true });
    },
  });
}

/** `DELETE …/mocks/{mock}`; forgets the mock and refreshes the list. */
export function useDeleteMock(ws: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: async (mock: string): Promise<void> => {
      unwrap(await api.DELETE("/api/v1/workspaces/{ws}/mocks/{mock}", { params: { path: { ws, mock } } }));
    },
    onSuccess: async (_, mock) => {
      queryClient.removeQueries({ queryKey: keys.mock(ws, mock) });
      await queryClient.invalidateQueries({ queryKey: keys.mocks(ws), exact: true });
    },
  });
}

/** The contract versions of a mock, newest first. */
export function useContracts(ws: string, mock: string) {
  return useQuery({
    queryKey: keys.contracts(ws, mock),
    queryFn: async (): Promise<ContractSummary[]> =>
      unwrap(
        await api.GET("/api/v1/workspaces/{ws}/mocks/{mock}/contracts", { params: { path: { ws, mock } } }),
      ),
  });
}

/** One contract version with its operations; idle while `version` is unknown. */
export function useContract(ws: string, mock: string, version: number | null | undefined) {
  return useQuery({
    queryKey: keys.contract(ws, mock, version ?? 0),
    queryFn: async (): Promise<ContractDetail> =>
      unwrap(
        await api.GET("/api/v1/workspaces/{ws}/mocks/{mock}/contracts/{version}", {
          params: { path: { ws, mock, version: version ?? 0 } },
        }),
      ),
    enabled: version != null,
  });
}

/** The operation ids of a mock's active contract, in contract order (empty without one). */
export function useActiveOperations(ws: string, mock: string): string[] {
  const m = useMock(ws, mock);
  const contract = useContract(ws, mock, m.data?.active_version);
  return contract.data?.operations.map((o) => o.id) ?? [];
}

/** Contract file extensions the console uploads (as the CLI does). */
export const CONTRACT_EXTENSIONS = [
  ".json",
  ".yaml",
  ".yml",
  ".graphql",
  ".graphqls",
  ".gql",
  ".proto",
  ".pb",
  ".desc",
  ".wsdl",
  ".xsd",
  ".xml",
  ".zip",
  ".tgz",
  ".tar.gz",
];

/**
 * The upload Content-Type for a contract file name, by extension (the same
 * mapping as `mockingbird import`); `undefined` for any other file.
 */
export function contentTypeFor(fileName: string): string | undefined {
  const name = fileName.toLowerCase();
  if (name.endsWith(".tar.gz")) return "application/gzip";
  const ext = name.includes(".") ? name.split(".").pop() : undefined;
  switch (ext) {
    case "json":
      return "application/json";
    case "yaml":
    case "yml":
      return "application/yaml";
    case "graphql":
    case "graphqls":
    case "gql":
      return "application/graphql";
    case "proto":
      return "text/x-protobuf";
    case "pb":
    case "desc":
      return "application/x-protobuf";
    case "wsdl":
    case "xsd":
    case "xml":
      return "application/xml";
    case "zip":
      return "application/zip";
    case "tgz":
      return "application/gzip";
    default:
      return undefined;
  }
}

/** Everything that shows a mock's versions or active version. */
function invalidateMock(queryClient: ReturnType<typeof useQueryClient>, ws: string) {
  // `["ws", ws, "mocks"]` prefixes the list, every mock and their contracts.
  return queryClient.invalidateQueries({ queryKey: keys.mocks(ws) });
}

/** `POST …/contracts[?activate=true]` with the file's bytes as the body. */
export function useUploadContract(ws: string, mock: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: async ({ file, activate }: { file: File; activate: boolean }): Promise<UploadResult> => {
      const contentType = contentTypeFor(file.name);
      if (!contentType) throw new Error(`Choose a contract file (${CONTRACT_EXTENSIONS.join(", ")}).`);
      // Sent as the file's raw bytes (archives and descriptor sets are
      // binary). The schema types the body per media type as text or a
      // number array; neither is how it travels, hence the cast.
      const bytes = await file.arrayBuffer();
      return unwrap(
        await api.POST("/api/v1/workspaces/{ws}/mocks/{mock}/contracts", {
          params: { path: { ws, mock }, query: activate ? { activate: true } : undefined },
          body: bytes as unknown as string,
          bodySerializer: (b: unknown) => b as BodyInit,
          headers: { "Content-Type": contentType },
        }),
      );
    },
    onSuccess: () => invalidateMock(queryClient, ws),
  });
}

/** `POST …/contracts/{version}/activate`: activates or rolls back. */
export function useActivate(ws: string, mock: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: async (version: number): Promise<ContractSummary> =>
      unwrap(
        await api.POST("/api/v1/workspaces/{ws}/mocks/{mock}/contracts/{version}/activate", {
          params: { path: { ws, mock, version } },
        }),
      ),
    onSuccess: () => invalidateMock(queryClient, ws),
  });
}

/** The LLM profiles of `ws`. */
export function useProfiles(ws: string) {
  return useQuery({
    queryKey: keys.profiles(ws),
    queryFn: async (): Promise<Profile[]> =>
      unwrap(await api.GET("/api/v1/workspaces/{ws}/llm-profiles", { params: { path: { ws } } })),
  });
}

/** The URL of a contract version's uploaded source. */
export function contractSourceUrl(ws: string, mock: string, version: number): string {
  const e = encodeURIComponent;
  return `/api/v1/workspaces/${e(ws)}/mocks/${e(mock)}/contracts/${version}/source`;
}

export type Scenario = components["schemas"]["ScenarioView"];
export type PutScenario = components["schemas"]["PutScenario"];
export type SessionInfo = components["schemas"]["SessionInfoView"];
export type StoredResource = components["schemas"]["StoredResourceView"];

/** The scenarios of a mock. */
export function useScenarios(ws: string, mock: string) {
  return useQuery({
    queryKey: keys.scenarios(ws, mock),
    queryFn: async (): Promise<Scenario[]> =>
      unwrap(
        await api.GET("/api/v1/workspaces/{ws}/mocks/{mock}/scenarios", { params: { path: { ws, mock } } }),
      ),
  });
}

/** `PUT …/scenarios/{name}` with the full scenario; refreshes the list. */
export function usePutScenario(ws: string, mock: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: async ({ name, body }: { name: string; body: PutScenario }): Promise<Scenario> =>
      unwrap(
        await api.PUT("/api/v1/workspaces/{ws}/mocks/{mock}/scenarios/{name}", {
          params: { path: { ws, mock, name } },
          body,
        }),
      ),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: keys.scenarios(ws, mock) }),
  });
}

/** `DELETE …/scenarios/{name}`; refreshes the list. */
export function useDeleteScenario(ws: string, mock: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: async (name: string): Promise<void> => {
      unwrap(
        await api.DELETE("/api/v1/workspaces/{ws}/mocks/{mock}/scenarios/{name}", {
          params: { path: { ws, mock, name } },
        }),
      );
    },
    onSuccess: () => queryClient.invalidateQueries({ queryKey: keys.scenarios(ws, mock) }),
  });
}

/** The live sessions of a mock, most recently touched first. */
export function useSessions(ws: string, mock: string) {
  return useQuery({
    queryKey: keys.sessions(ws, mock),
    queryFn: async (): Promise<SessionInfo[]> =>
      unwrap(await api.GET("/api/v1/workspaces/{ws}/mocks/{mock}/sessions", { params: { path: { ws, mock } } })),
  });
}

/** `GET …/sessions/{session}`: the session's resources, oldest first. */
export async function fetchSessionExport(ws: string, mock: string, session: string): Promise<StoredResource[]> {
  return unwrap(
    await api.GET("/api/v1/workspaces/{ws}/mocks/{mock}/sessions/{session}", {
      params: { path: { ws, mock, session } },
    }),
  );
}

/** One session's export. */
export function useSessionExport(ws: string, mock: string, session: string) {
  return useQuery({
    queryKey: keys.session(ws, mock, session),
    queryFn: () => fetchSessionExport(ws, mock, session),
  });
}

/** `DELETE …/sessions/{session}`: forgets its resources; refreshes the list. */
export function useResetSession(ws: string, mock: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: async (session: string): Promise<void> => {
      unwrap(
        await api.DELETE("/api/v1/workspaces/{ws}/mocks/{mock}/sessions/{session}", {
          params: { path: { ws, mock, session } },
        }),
      );
    },
    onSuccess: () => queryClient.invalidateQueries({ queryKey: keys.sessions(ws, mock) }),
  });
}

/** `PUT …/sessions/{session}`: replaces its resources; refreshes the list. */
export function useSeedSession(ws: string, mock: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: async ({ session, resources }: { session: string; resources: StoredResource[] }): Promise<void> => {
      unwrap(
        await api.PUT("/api/v1/workspaces/{ws}/mocks/{mock}/sessions/{session}", {
          params: { path: { ws, mock, session } },
          body: resources,
        }),
      );
    },
    onSuccess: () => queryClient.invalidateQueries({ queryKey: keys.sessions(ws, mock) }),
  });
}

export type PutProfile = components["schemas"]["PutProfile"];
export type ProfileWriteResult = components["schemas"]["ProfileWriteResult"];
export type ProfileDeleteResult = components["schemas"]["ProfileDeleteResult"];

/** `PUT …/llm-profiles/{name}` with the full profile; refreshes the list. */
export function usePutProfile(ws: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: async ({ name, body }: { name: string; body: PutProfile }): Promise<ProfileWriteResult> =>
      unwrap(
        await api.PUT("/api/v1/workspaces/{ws}/llm-profiles/{name}", { params: { path: { ws, name } }, body }),
      ),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: keys.profiles(ws) }),
  });
}

/** `DELETE …/llm-profiles/{name}`: the mocks that still use it as warnings; refreshes the list. */
export function useDeleteProfile(ws: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: async (name: string): Promise<ProfileDeleteResult> => {
      const r = await api.DELETE("/api/v1/workspaces/{ws}/llm-profiles/{name}", { params: { path: { ws, name } } });
      // 204: no mock used the profile.
      return unwrap(r) ?? { deleted: name, warnings: [] };
    },
    onSuccess: () => queryClient.invalidateQueries({ queryKey: keys.profiles(ws) }),
  });
}

export type ApiKey = components["schemas"]["ApiKeyView"];
export type CreateApiKey = components["schemas"]["CreateApiKey"];
export type CreatedApiKey = components["schemas"]["CreatedApiKey"];
export type Role = components["schemas"]["Role"];
export type KeyScope = components["schemas"]["KeyScope"];

/** The API keys of `ws`, newest first, revoked ones included (ManageApiKeys). */
export function useApiKeys(ws: string, enabled = true) {
  return useQuery({
    queryKey: keys.apiKeys(ws),
    queryFn: async (): Promise<ApiKey[]> =>
      unwrap(await api.GET("/api/v1/workspaces/{ws}/api-keys", { params: { path: { ws } } })),
    enabled,
  });
}

/**
 * `POST …/api-keys`: the result carries the token, which is never shown
 * again. `gcTime: 0` drops the mutation (and the token in its data) from the
 * mutation cache as soon as nothing observes it, i.e. once the dialog
 * resets it or unmounts.
 */
export function useCreateApiKey(ws: string) {
  const queryClient = useQueryClient();
  return useMutation({
    gcTime: 0,
    mutationFn: async (body: CreateApiKey): Promise<CreatedApiKey> =>
      unwrap(await api.POST("/api/v1/workspaces/{ws}/api-keys", { params: { path: { ws } }, body })),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: keys.apiKeys(ws) }),
  });
}

/** `DELETE …/api-keys/{key_id}`; refreshes the list. */
export function useRevokeApiKey(ws: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: async (keyId: string): Promise<void> => {
      unwrap(
        await api.DELETE("/api/v1/workspaces/{ws}/api-keys/{key_id}", { params: { path: { ws, key_id: keyId } } }),
      );
    },
    onSuccess: () => queryClient.invalidateQueries({ queryKey: keys.apiKeys(ws) }),
  });
}

/** `active`, `revoked` or `expired` (at `now`). */
export function keyStatus(key: ApiKey, now = Date.now()): "active" | "revoked" | "expired" {
  if (key.revoked_at) return "revoked";
  if (key.expires_at && Date.parse(key.expires_at) <= now) return "expired";
  return "active";
}

export type AuditEvent = components["schemas"]["AuditEventView"];

/** Audit list filters; empty strings are left out of the query. */
export interface AuditFilters {
  action?: string;
  actor?: string;
}

/** Events per audit page; a shorter page is the last one. */
export const AUDIT_PAGE = 50;

function auditQuery(filters: AuditFilters) {
  const q: { action?: string; actor?: string } = {};
  if (filters.action) q.action = filters.action;
  if (filters.actor) q.actor = filters.actor;
  return q;
}

/** The URL of an audit log (`ws`, or every workspace when `null`) as JSONL, with `filters`. */
export function auditExportUrl(ws: string | null, filters: AuditFilters): string {
  const base = ws === null ? "/api/v1/audit" : `/api/v1/workspaces/${encodeURIComponent(ws)}/audit`;
  const params = new URLSearchParams({ ...auditQuery(filters), format: "jsonl" });
  return `${base}?${params.toString()}`;
}

const auditPaging = {
  initialPageParam: undefined as number | undefined,
  getNextPageParam: (last: AuditEvent[]) => (last.length < AUDIT_PAGE ? undefined : last[last.length - 1]?.id),
};

/** The audit log of `ws`, newest first; `fetchNextPage` loads older events (`before=<last id>`). */
export function useAudit(ws: string, filters: AuditFilters, enabled = true) {
  return useInfiniteQuery({
    queryKey: keys.audit(ws, filters),
    queryFn: async ({ pageParam }): Promise<AuditEvent[]> =>
      unwrap(
        await api.GET("/api/v1/workspaces/{ws}/audit", {
          params: { path: { ws }, query: { ...auditQuery(filters), limit: AUDIT_PAGE, before: pageParam } },
        }),
      ),
    ...auditPaging,
    enabled,
  });
}

/** The audit log of every workspace (sysadmin), paged like [`useAudit`]. */
export function useGlobalAudit(filters: AuditFilters, enabled = true) {
  return useInfiniteQuery({
    queryKey: ["audit", filters] as const,
    queryFn: async ({ pageParam }): Promise<AuditEvent[]> =>
      unwrap(
        await api.GET("/api/v1/audit", {
          params: { query: { ...auditQuery(filters), limit: AUDIT_PAGE, before: pageParam } },
        }),
      ),
    ...auditPaging,
    enabled,
  });
}

/** `GET` on an ops path of the admin port (not part of the Admin API client). */
function fetchOps(path: string): Promise<Response> {
  return globalThis.fetch(new Request(new URL(path, window.location.origin)));
}

/** The `/readyz` answer. */
export interface Readiness {
  status: string;
  reasons: string[];
}

/** `/readyz`: `ready`, `degraded` or `unavailable` (a 503), with reasons. */
export function useReadiness() {
  return useQuery({
    queryKey: ["ops", "readyz"] as const,
    queryFn: async (): Promise<Readiness> => {
      const response = await fetchOps("/readyz");
      let body: Partial<Readiness> = {};
      try {
        body = (await response.json()) as Partial<Readiness>;
      } catch {
        // Not JSON: fall back to the status code.
      }
      return {
        status: typeof body.status === "string" ? body.status : `HTTP ${response.status}`,
        reasons: Array.isArray(body.reasons) ? body.reasons.map(String) : [],
      };
    },
    refetchInterval: 15_000,
  });
}

/** What the Status page shows from `/metrics`. */
export interface MetricsSummary {
  /** Sum of every `mb_requests_total` series. */
  requestsTotal: number;
  /** `mb_requests_total` summed per `source` label, in first-seen order. */
  bySource: Record<string, number>;
  /** Sum of every `mb_request_log_dropped_total` series. */
  logDropped: number;
}

const SAMPLE = /^([a-zA-Z_:][a-zA-Z0-9_:]*)(?:\{(.*)\})?\s+(\S+)/;
const LABEL = /([a-zA-Z_][a-zA-Z0-9_]*)="((?:[^"\\]|\\.)*)"/g;

/** Sums the series the Status page shows from Prometheus text exposition. */
export function parseMetricsSummary(text: string): MetricsSummary {
  const summary: MetricsSummary = { requestsTotal: 0, bySource: {}, logDropped: 0 };
  for (const line of text.split("\n")) {
    if (line.startsWith("#")) continue;
    const m = SAMPLE.exec(line.trim());
    if (!m) continue;
    const [, name, labels = "", raw] = m;
    const value = Number(raw);
    if (!Number.isFinite(value)) continue;
    if (name === "mb_requests_total") {
      summary.requestsTotal += value;
      const source = [...labels.matchAll(LABEL)].find((l) => l[1] === "source")?.[2] ?? "unknown";
      summary.bySource[source] = (summary.bySource[source] ?? 0) + value;
    } else if (name === "mb_request_log_dropped_total") {
      summary.logDropped += value;
    }
  }
  return summary;
}

/** `/metrics` summarised, or `"private"` when it needs credentials (401/403). */
export function useMetricsSummary() {
  return useQuery({
    queryKey: ["ops", "metrics"] as const,
    queryFn: async (): Promise<MetricsSummary | "private"> => {
      const response = await fetchOps("/metrics");
      if (response.status === 401 || response.status === 403) return "private";
      if (!response.ok) throw new Error(`/metrics answered ${response.status}`);
      return parseMetricsSummary(await response.text());
    },
    refetchInterval: 15_000,
  });
}
