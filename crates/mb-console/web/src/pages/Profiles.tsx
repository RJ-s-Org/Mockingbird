import { useState } from "react";
import { useDeleteProfile, useProfiles, type Profile } from "../api/queries";
import { useMe } from "../auth/me";
import { can } from "../auth/permissions";
import { ConfirmDialog } from "../components/ConfirmDialog";
import { DataTable, columnHelper } from "../components/DataTable";
import { ProblemAlert } from "../components/ProblemAlert";
import { SourceBadge } from "../components/SourceBadge";
import { ProfileEditor } from "./ProfileEditor";
import { Loading, Notice, PageHeader, primaryButton, secondaryButton } from "../components/ui";

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
                <span className="text-sm whitespace-nowrap text-fg-muted">Managed by config file</span>
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
    <section className="space-y-6">
      <PageHeader
        title="LLM profiles"
        description="The model endpoints mocks use to generate responses."
        actions={
          canManage ? (
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
          ) : null
        }
      />
      {me.data && !canManage ? <Notice tone="warn">Read-only: managing LLM profiles needs the admin role.</Notice> : null}
      {outcome ? (
        <Notice role="status">
          <p className="font-medium text-ok-fg">{outcome.message}</p>
          {outcome.warnings.length > 0 ? (
            <ul className="mt-2 list-disc space-y-0.5 pl-5 text-warn-fg">
              {outcome.warnings.map((w) => (
                <li key={w}>{w}</li>
              ))}
            </ul>
          ) : null}
        </Notice>
      ) : null}
      {profiles.error ? <ProblemAlert error={profiles.error} /> : null}
      {profiles.data ? (
        <DataTable caption="LLM profiles" columns={columns} data={profiles.data} empty="No LLM profiles yet." />
      ) : profiles.isPending ? (
        <Loading />
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
