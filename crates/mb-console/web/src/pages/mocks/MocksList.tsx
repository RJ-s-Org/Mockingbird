import { Link, useNavigate } from "@tanstack/react-router";
import { useState } from "react";
import { useMocks, type Mock } from "../../api/queries";
import { useMe } from "../../auth/me";
import { can } from "../../auth/permissions";
import { primaryButton } from "../../components/ConfirmDialog";
import { DataTable, columnHelper } from "../../components/DataTable";
import { ProblemAlert } from "../../components/ProblemAlert";
import { SourceBadge } from "../../components/SourceBadge";
import { CreateMockDialog } from "./CreateMockDialog";

export const linkClass =
  "font-medium text-indigo-700 underline hover:text-indigo-900 focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-indigo-700";

const col = columnHelper<Mock>();

/** The mocks of a workspace; an editor can create one. */
export function MocksList({ ws }: { ws: string }) {
  const me = useMe();
  const mocks = useMocks(ws);
  const navigate = useNavigate();
  const [creating, setCreating] = useState(false);
  const canEdit = me.data ? can(me.data, ws, "edit") : false;

  const columns = [
    col.accessor("name", {
      header: "Name",
      cell: (info) => (
        <Link
          to="/w/$ws/mocks/$mock/overview"
          params={{ ws, mock: info.getValue() }}
          className={linkClass}
        >
          {info.getValue()}
        </Link>
      ),
    }),
    col.accessor("route_prefix", { header: "Prefix", cell: (info) => <code>{info.getValue()}</code> }),
    col.accessor("active_version", {
      header: "Active version",
      cell: (info) => {
        const v = info.getValue();
        return v == null ? "—" : `v${v}`;
      },
    }),
    col.accessor("source", { header: "Source", cell: (info) => <SourceBadge source={info.getValue()} /> }),
    col.accessor((m) => m.data_plane_auth.mode, { id: "auth", header: "Data-plane auth" }),
  ];

  return (
    <section className="space-y-4">
      <div className="flex items-center justify-between">
        <h1 className="text-2xl font-semibold">Mocks</h1>
        {canEdit ? (
          <button type="button" className={primaryButton} onClick={() => setCreating(true)}>
            New mock
          </button>
        ) : null}
      </div>
      {mocks.error ? <ProblemAlert error={mocks.error} /> : null}
      {mocks.data ? (
        <DataTable caption="Mocks" columns={columns} data={mocks.data} empty="No mocks yet." />
      ) : mocks.isPending ? (
        <p className="text-slate-700">Loading…</p>
      ) : null}
      {creating ? (
        <CreateMockDialog
          ws={ws}
          onClose={() => setCreating(false)}
          onCreated={(m) => {
            setCreating(false);
            void navigate({ to: "/w/$ws/mocks/$mock/overview", params: { ws, mock: m.name } });
          }}
        />
      ) : null}
    </section>
  );
}
