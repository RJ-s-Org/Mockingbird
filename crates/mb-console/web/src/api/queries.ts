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
