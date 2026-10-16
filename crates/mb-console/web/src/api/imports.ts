import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api, unwrap } from "./client";
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
