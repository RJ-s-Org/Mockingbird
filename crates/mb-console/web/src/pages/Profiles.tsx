import { useState } from "react";
import { useDeleteProfile, useProfiles, type Profile } from "../api/queries";
import { useMe } from "../auth/me";
import { can } from "../auth/permissions";
import { ConfirmDialog, primaryButton, secondaryButton } from "../components/ConfirmDialog";
import { DataTable, columnHelper } from "../components/DataTable";
import { ProblemAlert } from "../components/ProblemAlert";
import { SourceBadge } from "../components/SourceBadge";
import { ProfileEditor } from "./ProfileEditor";

const col = columnHelper<Profile>();

/** What the last save or delete reported. */
interface Outcome {
  message: string;
  warnings: string[];
}

/** The LLM profiles of a workspace; ManageProfiles can create, edit and delete API profiles. */
export function Profiles({ ws }: { ws: string }) {
  const me = useMe();
  const profiles = useProfiles(ws);
  const remove = useDeleteProfile(ws);
  // `null`: a new profile; a profile: editing it.
  const [editing, setEditing] = useState<Profile | null>();
  const [deleting, setDeleting] = useState<Profile>();
  const [outcome, setOutcome] = useState<Outcome>();
  const canManage = me.data ? can(me.data, ws, "manageProfiles") : false;

  const columns = [
    col.accessor("name", { header: "Name" }),
    col.accessor("model", { header: "Model" }),
    col.accessor("base_url", { header: "Base URL", cell: (info) => <code>{info.getValue()}</code> }),
    col.accessor("secret_ref", {
      header: "Secret ref",
      cell: (info) => (info.getValue() ? <code>{info.getValue()}</code> : "—"),
    }),
    col.accessor("fallback_profile", { header: "Fallback", cell: (info) => info.getValue() || "—" }),
    col.accessor("source", { header: "Source", cell: (info) => <SourceBadge source={info.getValue()} /> }),
    ...(canManage
      ? [
          col.display({
            id: "actions",
            header: "Actions",
            cell: ({ row }) =>
              row.original.source === "api" ? (
                <div className="flex gap-2">
                  <button
                    type="button"
                    className={secondaryButton}
                    aria-label={`Edit ${row.original.name}`}
                    onClick={() => setEditing(row.original)}
                  >
                    Edit
                  </button>
                  <button
                    type="button"
                    className={secondaryButton}
                    aria-label={`Delete ${row.original.name}`}
                    onClick={() => setDeleting(row.original)}
                  >
                    Delete
                  </button>
                </div>
              ) : (
                <span className="text-slate-600 dark:text-slate-400">Managed by config file</span>
              ),
          }),
        ]
      : []),
  ];

  const del = (p: Profile) =>
    remove.mutate(p.name, {
      onSuccess: (r) => {
        setDeleting(undefined);
        setOutcome({ message: `Deleted profile ${p.name}.`, warnings: r.warnings });
      },
    });

  return (
    <section className="space-y-4">
      <div className="flex items-center justify-between">
        <h1 className="text-2xl font-semibold">LLM profiles</h1>
        {canManage ? (
          <button
            type="button"
            className={primaryButton}
            onClick={() => {
              setOutcome(undefined);
              setEditing(null);
            }}
          >
            New profile
          </button>
        ) : null}
      </div>
      {me.data && !canManage ? (
        <p className="rounded-md border border-amber-300 dark:border-amber-700 bg-amber-50 dark:bg-amber-950/60 p-3 text-sm text-amber-900 dark:text-amber-200">
          Read-only: managing LLM profiles needs the admin role.
        </p>
      ) : null}
      {outcome ? (
        <div role="status" className="rounded-md border border-slate-300 dark:border-slate-700 bg-white dark:bg-slate-900 p-3 text-sm">
          <p className="text-green-800 dark:text-green-300">{outcome.message}</p>
          {outcome.warnings.length > 0 ? (
            <ul className="mt-2 list-disc pl-5 text-amber-900 dark:text-amber-200">
              {outcome.warnings.map((w) => (
                <li key={w}>{w}</li>
              ))}
            </ul>
          ) : null}
        </div>
      ) : null}
      {profiles.error ? <ProblemAlert error={profiles.error} /> : null}
      {profiles.data ? (
        <DataTable caption="LLM profiles" columns={columns} data={profiles.data} empty="No LLM profiles yet." />
      ) : profiles.isPending ? (
        <p className="text-slate-700 dark:text-slate-300">Loading…</p>
      ) : null}
      {editing !== undefined ? (
        <ProfileEditor
          ws={ws}
          profile={editing ?? undefined}
          profiles={profiles.data ?? []}
          onClose={() => setEditing(undefined)}
          onSaved={(r) => {
            setEditing(undefined);
            setOutcome({ message: `Saved profile ${r.name}.`, warnings: r.warnings });
          }}
        />
      ) : null}
      {deleting ? (
        <ConfirmDialog
          title={`Delete profile ${deleting.name}?`}
          message={
            <>
              Mocks that use it lose their LLM profile. <ProblemAlert error={remove.error} />
            </>
          }
          confirmLabel="Delete"
          busy={remove.isPending}
          onConfirm={() => del(deleting)}
          onCancel={() => {
            remove.reset();
            setDeleting(undefined);
          }}
        />
      ) : null}
    </section>
  );
}
