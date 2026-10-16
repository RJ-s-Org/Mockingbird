import { useEffect } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import { isFinished, useCancelImport, useImportJob } from "../../api/imports";
import { keys } from "../../api/queries";
import { primaryButton, secondaryButton } from "../../components/ConfirmDialog";
import { ProblemAlert } from "../../components/ProblemAlert";

const PHASES: Record<string, string> = {
  queued: "Queued",
  fetching: "Fetching the document",
  extracting: "Extracting text",
  analyzing: "Analyzing the document",
  summarizing: "Summarizing operations",
  assembling: "Assembling the draft",
};

/** A human label for a progress phase (unknown phases as sent). */
function phaseLabel(phase: string): string {
  return PHASES[phase] ?? phase;
}

/** One import job's progress; opens the draft once the job is done. */
export function ImportProgress({ ws, mock, job: id }: { ws: string; mock: string; job: string }) {
  const job = useImportJob(ws, mock, id);
  const cancel = useCancelImport(ws, mock);
  const navigate = useNavigate();
  const queryClient = useQueryClient();

  const status = job.data?.status;
  const version = job.data?.result_version;
  useEffect(() => {
    if (status !== "done" || version == null) return;
    // The new draft shows up in the mock's contract list.
    void queryClient.invalidateQueries({ queryKey: keys.contracts(ws, mock) });
    void navigate({
      to: "/w/$ws/mocks/$mock/contracts/$version/draft",
      params: { ws, mock, version: String(version) },
      replace: true,
    });
  }, [status, version, ws, mock, navigate, queryClient]);

  const startOver = () => void navigate({ to: "/w/$ws/mocks/$mock/import", params: { ws, mock }, search: {} });

  if (!job.data) {
    return job.error ? (
      <div className="space-y-3">
        <ProblemAlert error={job.error} />
        <button type="button" className={secondaryButton} onClick={startOver}>
          Start over
        </button>
      </div>
    ) : (
      <p className="text-slate-700">Loading…</p>
    );
  }

  const { progress } = job.data;
  const source = job.data.source_name ? ` of ${job.data.source_name}` : "";
  return (
    <section
      aria-labelledby="import-progress-title"
      className="max-w-2xl space-y-4 rounded-md border border-slate-300 bg-white p-4"
    >
      <h2 id="import-progress-title" className="text-lg font-semibold">
        Import{source}
      </h2>
      {!isFinished(job.data) ? (
        <div className="space-y-2" aria-live="polite">
          <p className="text-sm font-medium">{phaseLabel(progress.phase)}</p>
          {progress.total > 0 ? (
            <div className="flex items-center gap-3">
              <progress value={progress.done} max={progress.total} aria-label="Import progress" className="h-2 w-64" />
              <span className="text-sm text-slate-700">
                {progress.done} of {progress.total}
              </span>
            </div>
          ) : null}
          <button
            type="button"
            className={secondaryButton}
            disabled={cancel.isPending}
            onClick={() => cancel.mutate(id)}
          >
            Cancel import
          </button>
          <ProblemAlert error={cancel.error} />
        </div>
      ) : null}
      {job.data.status === "done" ? (
        <p role="status" className="text-sm text-green-800">
          Import finished; opening the draft…
        </p>
      ) : null}
      {job.data.status === "failed" ? (
        <div className="space-y-3">
          <div role="alert" className="rounded-md border border-red-300 bg-red-50 p-3 text-sm text-red-900">
            <p className="font-semibold">Import failed</p>
            {job.data.error ? <p className="mt-1">{job.data.error}</p> : null}
          </div>
          <button type="button" className={primaryButton} onClick={startOver}>
            Start over
          </button>
        </div>
      ) : null}
      {job.data.status === "canceled" ? (
        <div className="space-y-3">
          <p role="status" className="text-sm font-medium">
            Import canceled
          </p>
          <button type="button" className={primaryButton} onClick={startOver}>
            Start over
          </button>
        </div>
      ) : null}
    </section>
  );
}
