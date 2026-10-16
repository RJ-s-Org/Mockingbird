import { useState } from "react";
import { keyStatus, useApiKeys, useRevokeApiKey, type ApiKey } from "../api/queries";
import { useMe } from "../auth/me";
import { can, roleIn } from "../auth/permissions";
import { ConfirmDialog, primaryButton, secondaryButton } from "../components/ConfirmDialog";
import { DataTable, columnHelper } from "../components/DataTable";
import { ProblemAlert } from "../components/ProblemAlert";
import { CreateKeyDialog } from "./CreateKeyDialog";

const col = columnHelper<ApiKey>();

/** An RFC 3339 instant in the viewer's locale, or a dash. */
export function DateTime({ value }: { value?: string | null }) {
  if (!value) return <>—</>;
  return <time dateTime={value}>{new Date(value).toLocaleString()}</time>;
}

const STATUS_STYLE = {
  active: "border-green-300 bg-green-50 text-green-900",
  revoked: "border-slate-300 bg-slate-100 text-slate-800",
  expired: "border-amber-300 bg-amber-50 text-amber-900",
};

/** The API keys of a workspace (ManageApiKeys): list, create and revoke. */
export function ApiKeys({ ws }: { ws: string }) {
  const me = useMe();
  const canManage = me.data ? can(me.data, ws, "manageApiKeys") : false;
  const role = me.data ? roleIn(me.data, ws) : undefined;
  const keys = useApiKeys(ws, canManage);
  const revoke = useRevokeApiKey(ws);
  const [creating, setCreating] = useState(false);
  const [revoking, setRevoking] = useState<ApiKey>();

  if (!me.data) return me.isPending ? <p className="text-slate-700">Loading…</p> : null;
  if (!canManage || !role) {
    return (
      <section className="space-y-4">
        <h1 className="text-2xl font-semibold">API keys</h1>
        <p className="rounded-md border border-amber-300 bg-amber-50 p-3 text-sm text-amber-900">
          Forbidden (403): managing API keys needs the admin role in this workspace. Ask a workspace owner for
          access.
        </p>
      </section>
    );
  }

  const columns = [
    col.accessor("name", { header: "Name" }),
    col.accessor("key_id", { header: "Key id", cell: (info) => <code>{info.getValue()}</code> }),
    col.accessor("role", {
      header: "Role",
      cell: (info) => (info.row.original.sysadmin ? `${info.getValue()} (sysadmin)` : info.getValue()),
    }),
    col.accessor("scopes", { header: "Scopes", cell: (info) => info.getValue().join(", ") }),
    col.accessor("created_by", { header: "Created by" }),
    col.accessor("created_at", { header: "Created", cell: (info) => <DateTime value={info.getValue()} /> }),
    col.accessor("expires_at", { header: "Expires", cell: (info) => <DateTime value={info.getValue()} /> }),
    col.accessor("last_used_at", { header: "Last used", cell: (info) => <DateTime value={info.getValue()} /> }),
    col.accessor((k) => keyStatus(k), {
      id: "status",
      header: "Status",
      cell: (info) => (
        <span className={`inline-flex rounded-full border px-2 py-0.5 text-xs font-medium ${STATUS_STYLE[info.getValue()]}`}>
          {info.getValue()}
        </span>
      ),
    }),
    col.display({
      id: "actions",
      header: "Actions",
      cell: ({ row }) =>
        row.original.revoked_at ? null : (
          <button
            type="button"
            className={secondaryButton}
            aria-label={`Revoke ${row.original.name}`}
            onClick={() => setRevoking(row.original)}
          >
            Revoke
          </button>
        ),
    }),
  ];

  return (
    <section className="space-y-4">
      <div className="flex items-center justify-between">
        <h1 className="text-2xl font-semibold">API keys</h1>
        <button type="button" className={primaryButton} onClick={() => setCreating(true)}>
          New API key
        </button>
      </div>
      {keys.error ? <ProblemAlert error={keys.error} /> : null}
      {keys.data ? (
        <DataTable caption="API keys" columns={columns} data={keys.data} empty="No API keys yet." />
      ) : keys.isPending ? (
        <p className="text-slate-700">Loading…</p>
      ) : null}
      {creating ? (
        <CreateKeyDialog ws={ws} myRole={role} sysadmin={me.data.sysadmin} onClose={() => setCreating(false)} />
      ) : null}
      {revoking ? (
        <ConfirmDialog
          title={`Revoke key ${revoking.name}?`}
          message={
            <>
              <p>
                Requests with key <code>{revoking.key_id}</code> fail from now on. This cannot be undone.
              </p>
              <ProblemAlert error={revoke.error} />
            </>
          }
          confirmLabel="Revoke"
          busy={revoke.isPending}
          onConfirm={() => revoke.mutate(revoking.key_id, { onSuccess: () => setRevoking(undefined) })}
          onCancel={() => {
            revoke.reset();
            setRevoking(undefined);
          }}
        />
      ) : null}
    </section>
  );
}
