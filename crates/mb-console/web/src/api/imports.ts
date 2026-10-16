import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api, unwrap } from "./client";
import { keys, type ContractSummary } from "./queries";
import type { components } from "./schema";

export type ImportJob = components["schemas"]["ImportJobView"];
export type ImportKind = components["schemas"]["ImportKindSchema"];
export type CreateImport = components["schemas"]["CreateImport"];

/** The kinds a pasted document can be. */
export type TextKind = "markdown" | "html" | "text" | "har" | "postman";

/** What to import: an uploaded file, a URL to fetch, or pasted text. */
export type ImportSource =
  | { type: "file"; file: File; kind?: ImportKind }
  | { type: "url"; url: string }
  | { type: "text"; kind: TextKind; text: string };

/** Document file extensions the import wizard offers. */
export const IMPORT_FILE_EXTENSIONS = [".md", ".markdown", ".html", ".htm", ".pdf", ".har", ".json", ".txt"];

/** Query keys of a mock's imports and drafts (under the mock's own key). */
export const importKeys = {
  imports: (ws: string, mock: string) => ["ws", ws, "mocks", mock, "imports"] as const,
  job: (ws: string, mock: string, id: string) => ["ws", ws, "mocks", mock, "imports", id] as const,
  draft: (ws: string, mock: string, version: number) => ["ws", ws, "mocks", mock, "draft", version] as const,
};

/** Whether a job has stopped for good (`done`, `failed` or `canceled`). */
export function isFinished(job: ImportJob): boolean {
  return job.status === "done" || job.status === "failed" || job.status === "canceled";
}

/**
 * `POST …/imports`: a file goes as `multipart/form-data` (part `file`,
 * optional part `kind`) with the `X-Mockingbird-Upload` header; a URL or
 * pasted text as JSON. Seeds the job's query with the queued job.
 */
export function useCreateImport(ws: string, mock: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: async (source: ImportSource): Promise<ImportJob> => {
      const params = { path: { ws, mock } };
      if (source.type === "file") {
        const form = new FormData();
        form.append("file", source.file, source.file.name);
        if (source.kind) form.append("kind", source.kind);
        // The browser sets the multipart Content-Type with its boundary.
        return unwrap(
          await api.POST("/api/v1/workspaces/{ws}/mocks/{mock}/imports", {
            params,
            body: form as unknown as CreateImport,
            bodySerializer: (b: unknown) => b as BodyInit,
            headers: { "X-Mockingbird-Upload": "1" },
          }),
        );
      }
      const body: CreateImport = source.type === "url" ? { url: source.url } : { kind: source.kind, text: source.text };
      return unwrap(await api.POST("/api/v1/workspaces/{ws}/mocks/{mock}/imports", { params, body }));
    },
    onSuccess: async (job) => {
      queryClient.setQueryData(importKeys.job(ws, mock, job.id), job);
      await queryClient.invalidateQueries({ queryKey: importKeys.imports(ws, mock), exact: true });
    },
  });
}

/** One import job, polled every second while it is queued or running; idle without `id`. */
export function useImportJob(ws: string, mock: string, id: string | undefined) {
  return useQuery({
    queryKey: importKeys.job(ws, mock, id ?? ""),
    queryFn: async (): Promise<ImportJob> =>
      unwrap(
        await api.GET("/api/v1/workspaces/{ws}/mocks/{mock}/imports/{job}", {
          params: { path: { ws, mock, job: id ?? "" } },
        }),
      ),
    enabled: id !== undefined,
    refetchInterval: (query) => (query.state.data && !isFinished(query.state.data) ? 1000 : false),
  });
}

/** `DELETE …/imports/{job}`: cancels a queued or running job; stores the canceled job. */
export function useCancelImport(ws: string, mock: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: async (id: string): Promise<ImportJob> =>
      unwrap(
        await api.DELETE("/api/v1/workspaces/{ws}/mocks/{mock}/imports/{job}", {
          params: { path: { ws, mock, job: id } },
        }),
      ),
    onSuccess: async (job) => {
      queryClient.setQueryData(importKeys.job(ws, mock, job.id), job);
      await queryClient.invalidateQueries({ queryKey: importKeys.imports(ws, mock), exact: true });
    },
  });
}

export type DraftView = components["schemas"]["DraftView"];
export type DraftOperation = components["schemas"]["DraftOperationView"];
export type PreviewRequest = components["schemas"]["PreviewRequest"];
export type PreviewResponse = components["schemas"]["PreviewResponse"];

/** `GET …/contracts/{version}/draft`: an imported version with its review metadata. */
export function useDraft(ws: string, mock: string, version: number) {
  return useQuery({
    queryKey: importKeys.draft(ws, mock, version),
    queryFn: async (): Promise<DraftView> =>
      unwrap(
        await api.GET("/api/v1/workspaces/{ws}/mocks/{mock}/contracts/{version}/draft", {
          params: { path: { ws, mock, version } },
        }),
      ),
  });
}

/** `PUT …/contracts/{version}/draft`: replaces the draft's document; stores the new view. */
export function usePutDraft(ws: string, mock: string, version: number) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: async (document: object): Promise<DraftView> =>
      unwrap(
        await api.PUT("/api/v1/workspaces/{ws}/mocks/{mock}/contracts/{version}/draft", {
          params: { path: { ws, mock, version } },
          body: { document: document as DraftView["document"] },
        }),
      ),
    onSuccess: async (view) => {
      queryClient.setQueryData(importKeys.draft(ws, mock, version), view);
      await queryClient.invalidateQueries({ queryKey: keys.contracts(ws, mock) });
    },
  });
}

/** `POST …/contracts/{version}/approve`: approves a draft; refreshes the mock's versions and the draft. */
export function useApproveDraft(ws: string, mock: string, version: number) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: async (): Promise<ContractSummary> =>
      unwrap(
        await api.POST("/api/v1/workspaces/{ws}/mocks/{mock}/contracts/{version}/approve", {
          params: { path: { ws, mock, version } },
        }),
      ),
    // `["ws", ws, "mocks", mock]` prefixes the mock, its contracts and its drafts.
    onSuccess: () => queryClient.invalidateQueries({ queryKey: keys.mock(ws, mock) }),
  });
}

/** `POST …/contracts/{version}/preview`: generates one response of an operation (may spend tokens). */
export function usePreview(ws: string, mock: string, version: number) {
  return useMutation({
    mutationFn: async (request: PreviewRequest): Promise<PreviewResponse> =>
      unwrap(
        await api.POST("/api/v1/workspaces/{ws}/mocks/{mock}/contracts/{version}/preview", {
          params: { path: { ws, mock, version } },
          body: request,
        }),
      ),
  });
}
