import { Link, useNavigate } from "@tanstack/react-router";
import { useState } from "react";
import { useMocks, type Mock } from "../../api/queries";
import { useMe } from "../../auth/me";
import { can } from "../../auth/permissions";
import { DataTable, columnHelper } from "../../components/DataTable";
import { ProblemAlert } from "../../components/ProblemAlert";
import { SourceBadge } from "../../components/SourceBadge";
import { CreateMockDialog } from "./CreateMockDialog";
import { Badge, Loading, PageHeader, linkClass, primaryButton } from "../../components/ui";

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
    col.accessor("route_prefix", {
      header: "Prefix",
      cell: (info) => <code className="text-fg-muted">{info.getValue()}</code>,
    }),
    col.accessor("active_version", {
      header: "Active version",
      cell: (info) => {
        const v = info.getValue();
        return v == null ? <span className="text-fg-muted">—</span> : <Badge tone="ok">{`v${v}`}</Badge>;
      },
    }),
    col.accessor("source", { header: "Source", cell: (info) => <SourceBadge source={info.getValue()} /> }),
    col.accessor((m) => m.data_plane_auth.mode, {
      id: "auth",
      header: "Data-plane auth",
      cell: (info) => <code className="text-fg-muted">{info.getValue()}</code>,
    }),
  ];

  return (
    <section className="space-y-6">
      <PageHeader
        title="Mocks"
        description="Each mock serves an API contract with schema-valid responses."
        actions={
          canEdit ? (
            <button type="button" className={primaryButton} onClick={() => setCreating(true)}>
              New mock
            </button>
          ) : null
        }
      />
      {mocks.error ? <ProblemAlert error={mocks.error} /> : null}
      {mocks.data ? (
        <DataTable
          caption="Mocks"
          columns={columns}
          data={mocks.data}
          empty={canEdit ? "No mocks yet. Create one to serve a contract." : "No mocks yet."}
        />
      ) : mocks.isPending ? (
        <Loading />
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
