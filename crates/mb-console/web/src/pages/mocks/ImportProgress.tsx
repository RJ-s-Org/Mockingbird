import { useEffect } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import { isFinished, useCancelImport, useImportJob } from "../../api/imports";
import { keys } from "../../api/queries";
import { ProblemAlert } from "../../components/ProblemAlert";
import { Card, Loading, primaryButton, secondaryButton } from "../../components/ui";

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
      <Loading />
    );
  }

  const { progress } = job.data;
  const source = job.data.source_name ? ` of ${job.data.source_name}` : "";
  return (
    <Card as="section" id="import-progress-title" title={<>Import{source}</>} className="max-w-2xl">
      <div className="space-y-4">
        {!isFinished(job.data) ? (
          <div className="space-y-3" aria-live="polite">
            <Loading label={phaseLabel(progress.phase)} />
            {progress.total > 0 ? (
              <div className="flex items-center gap-3">
                <progress
                  value={progress.done}
                  max={progress.total}
                  aria-label="Import progress"
                  className="h-2 w-64 max-w-full accent-accent-solid"
                />
                <span className="text-sm whitespace-nowrap text-fg-muted">
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
          <p role="status" className="text-sm font-medium text-ok-fg">
            Import finished; opening the draft…
          </p>
        ) : null}
        {job.data.status === "failed" ? (
          <div className="space-y-3">
            <div role="alert" className="rounded-md border border-bad-line bg-bad-bg px-3 py-2.5 text-sm text-bad-fg">
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
            <p role="status" className="text-sm font-medium text-fg-muted">
              Import canceled
            </p>
            <button type="button" className={primaryButton} onClick={startOver}>
              Start over
            </button>
          </div>
        ) : null}
      </div>
    </Card>
  );
}
