import { useState } from "react";
import { useDeleteScenario, useScenarios, type Scenario } from "../../api/queries";
import { useMe } from "../../auth/me";
import { can } from "../../auth/permissions";
import { ConfirmDialog } from "../../components/ConfirmDialog";
import { DataTable, columnHelper } from "../../components/DataTable";
import { ProblemAlert } from "../../components/ProblemAlert";
import { SourceBadge } from "../../components/SourceBadge";
import { StatusCode } from "../../components/http";
import { ScenarioEditor } from "./ScenarioEditor";
import { Loading, primaryButton, secondaryButton } from "../../components/ui";

const col = columnHelper<Scenario>();

/** What the editor shows: a new scenario, or an existing one by name. */
type Editing = { kind: "new" } | { kind: "existing"; name: string };

/** A mock's scenarios: list, create, edit and delete. */
export function Scenarios({ ws, mock }: { ws: string; mock: string }) {
  const me = useMe();
  const scenarios = useScenarios(ws, mock);
  const remove = useDeleteScenario(ws, mock);
  const [editing, setEditing] = useState<Editing>();
  const [deleting, setDeleting] = useState<string>();

  const canEdit = me.data ? can(me.data, ws, "edit") : false;
  const editable = (s: Scenario) => canEdit && s.source !== "file";
  const current = editing?.kind === "existing" ? scenarios.data?.find((s) => s.name === editing.name) : undefined;

  const columns = [
    col.accessor("name", {
      header: "Name",
      cell: (info) => <code className="font-medium">{info.getValue()}</code>,
    }),
    col.accessor("status", {
      header: "Status",
      cell: (info) => {
        const status = info.getValue();
        return status == null ? <span className="text-fg-muted">—</span> : <StatusCode status={status} />;
      },
    }),
    col.accessor("latency_ms", {
      header: "Latency",
      cell: (info) => {
        const ms = info.getValue();
        return ms == null ? "—" : `${ms} ms`;
      },
    }),
    col.accessor("operations", {
      header: "Operations",
      cell: (info) => (info.getValue().length === 0 ? "all" : String(info.getValue().length)),
    }),
    col.accessor("source", {
      header: "Source",
      cell: (info) => <SourceBadge source={info.getValue()} />,
    }),
    col.display({
      id: "actions",
      header: "Actions",
      cell: ({ row }) => {
        const s = row.original;
        const verb = editable(s) ? "Edit" : "View";
        return (
          <div className="flex flex-nowrap gap-2">
            <button
              type="button"
              className={secondaryButton}
              aria-label={`${verb} scenario ${s.name}`}
              onClick={() => setEditing({ kind: "existing", name: s.name })}
            >
              {verb}
            </button>
            {editable(s) ? (
              <button
                type="button"
                className={secondaryButton}
                aria-label={`Delete scenario ${s.name}`}
                onClick={() => setDeleting(s.name)}
              >
                Delete
              </button>
            ) : null}
          </div>
        );
      },
    }),
  ];

  return (
    <div className="space-y-4">
      {canEdit ? (
        <div className="flex flex-wrap items-center justify-end gap-2">
          <button type="button" className={primaryButton} onClick={() => setEditing({ kind: "new" })}>
            New scenario
          </button>
        </div>
      ) : null}
      <ProblemAlert error={scenarios.error ?? remove.error} />
      {scenarios.data ? (
        <DataTable caption="Scenarios" columns={columns} data={scenarios.data} empty="No scenarios yet." />
      ) : scenarios.isPending ? (
        <Loading />
      ) : null}
      {editing?.kind === "new" ? (
        <ScenarioEditor ws={ws} mock={mock} canEdit={canEdit} onClose={() => setEditing(undefined)} />
      ) : current ? (
        <ScenarioEditor
          ws={ws}
          mock={mock}
          scenario={current}
          canEdit={canEdit}
          onClose={() => setEditing(undefined)}
        />
      ) : null}
      {deleting !== undefined ? (
        <ConfirmDialog
          title={`Delete scenario ${deleting}?`}
          message="Requests that name it in X-Mockingbird-Scenario will no longer match it."
          confirmLabel="Delete"
          busy={remove.isPending}
          onConfirm={() => remove.mutate(deleting, { onSettled: () => setDeleting(undefined) })}
          onCancel={() => setDeleting(undefined)}
        />
      ) : null}
    </div>
  );
}
