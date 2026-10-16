import { queryOptions, useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
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

/** The upload Content-Type for a contract file name, by extension. */
export function contentTypeFor(fileName: string): "application/json" | "application/yaml" | undefined {
  const ext = fileName.toLowerCase().split(".").pop();
  if (ext === "json") return "application/json";
  if (ext === "yaml" || ext === "yml") return "application/yaml";
  return undefined;
}

/** Everything that shows a mock's versions or active version. */
function invalidateMock(queryClient: ReturnType<typeof useQueryClient>, ws: string) {
  // `["ws", ws, "mocks"]` prefixes the list, every mock and their contracts.
  return queryClient.invalidateQueries({ queryKey: keys.mocks(ws) });
}

/** `POST …/contracts[?activate=true]` with the file's text as the body. */
export function useUploadContract(ws: string, mock: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: async ({ file, activate }: { file: File; activate: boolean }): Promise<UploadResult> => {
      const contentType = contentTypeFor(file.name);
      if (!contentType) throw new Error("Choose a .json, .yaml or .yml file.");
      const body = await file.text();
      return unwrap(
        await api.POST("/api/v1/workspaces/{ws}/mocks/{mock}/contracts", {
          params: { path: { ws, mock }, query: activate ? { activate: true } : undefined },
          body,
          bodySerializer: (b: string) => b,
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

/** The LLM profiles of `ws` (read-only here). */
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
