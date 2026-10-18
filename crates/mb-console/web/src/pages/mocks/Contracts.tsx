import { useRef, useState, type FormEvent } from "react";
import { Link } from "@tanstack/react-router";
import {
  contractSourceUrl,
  useActivate,
  useContracts,
  useMock,
  useUploadContract,
  CONTRACT_EXTENSIONS,
  type ContractSummary,
} from "../../api/queries";
import { useMe } from "../../auth/me";
import { can } from "../../auth/permissions";
import { primaryButton, secondaryButton } from "../../components/ConfirmDialog";
import { DataTable, columnHelper } from "../../components/DataTable";
import { ProblemAlert } from "../../components/ProblemAlert";
import { OperationsTable } from "./OperationsTable";
import { linkClass } from "./MocksList";
import { readOnlyReason } from "./Overview";

const col = columnHelper<ContractSummary>();

/** A mock's contract versions: upload, activate or roll back, inspect. */
export function Contracts({ ws, mock }: { ws: string; mock: string }) {
  const me = useMe();
  const m = useMock(ws, mock);
  const versions = useContracts(ws, mock);
  const activate = useActivate(ws, mock);
  const [viewing, setViewing] = useState<number>();

  const canEdit = me.data ? can(me.data, ws, "edit") : false;
  const reason = m.data ? readOnlyReason(m.data, canEdit) : "";
  const active = m.data?.active_version ?? null;

  const columns = [
    col.accessor("version", { header: "Version", cell: (info) => `v${info.getValue()}` }),
    col.accessor("status", { header: "Status" }),
    col.accessor("source_digest", {
      header: "Digest",
      cell: (info) => <code title={info.getValue()}>{info.getValue().slice(0, 12)}</code>,
    }),
    col.accessor("created_by", { header: "Created by", cell: (info) => info.getValue() ?? "—" }),
    col.accessor("created_at", {
      header: "Created at",
      cell: (info) => <time dateTime={info.getValue()}>{new Date(info.getValue()).toLocaleString()}</time>,
    }),
    col.accessor("operations", { header: "Operations" }),
    col.display({
      id: "actions",
      header: "Actions",
      cell: ({ row }) => {
        const v = row.original;
        const rollBack = active !== null && v.version < active;
        const activatable = reason === undefined && v.status !== "active" && v.status !== "draft";
        return (
          <div className="flex flex-wrap items-center gap-2">
            {activatable ? (
              <button
                type="button"
                className={primaryButton}
                aria-label={rollBack ? `Roll back to version ${v.version}` : `Activate version ${v.version}`}
                disabled={activate.isPending}
                onClick={() => activate.mutate(v.version)}
              >
                {rollBack ? "Roll back" : "Activate"}
              </button>
            ) : null}
            <button
              type="button"
              className={secondaryButton}
              aria-label={`View operations of version ${v.version}`}
              onClick={() => setViewing(v.version)}
            >
              View operations
            </button>
            {v.imported ? (
              <Link
                to="/w/$ws/mocks/$mock/contracts/$version/draft"
                params={{ ws, mock, version: String(v.version) }}
                className={linkClass}
                aria-label={`Review version ${v.version}`}
              >
                Review
              </Link>
            ) : null}
            <a
              href={contractSourceUrl(ws, mock, v.version)}
              download
              className={linkClass}
              aria-label={`Download source of version ${v.version}`}
            >
              Download source
            </a>
          </div>
        );
      },
    }),
  ];

  return (
    <div className="space-y-6">
      {reason ? (
        <p className="rounded-md border border-amber-300 dark:border-amber-700 bg-amber-50 dark:bg-amber-950/60 p-3 text-sm text-amber-900 dark:text-amber-200">{reason}</p>
      ) : null}
      {reason === undefined ? <UploadForm ws={ws} mock={mock} /> : null}
      <ProblemAlert error={activate.error ?? versions.error} />
      {versions.data ? (
        <DataTable
          caption="Contract versions"
          columns={columns}
          data={versions.data}
          empty="No contract uploaded yet."
        />
      ) : versions.isPending ? (
        <p className="text-slate-700 dark:text-slate-300">Loading…</p>
      ) : null}
      {viewing !== undefined ? (
        <OperationsTable ws={ws} mock={mock} version={viewing} onClose={() => setViewing(undefined)} />
      ) : null}
    </div>
  );
}

/** Uploads a contract file or archive, optionally activating it. */
function UploadForm({ ws, mock }: { ws: string; mock: string }) {
  const upload = useUploadContract(ws, mock);
  const input = useRef<HTMLInputElement>(null);
  const [file, setFile] = useState<File>();
  const [activate, setActivate] = useState(false);

  const submit = (e: FormEvent) => {
    e.preventDefault();
    if (!file) return;
    upload.mutate(
      { file, activate },
      {
        onSuccess: () => {
          setFile(undefined);
          if (input.current) input.current.value = "";
        },
      },
    );
  };

  return (
    <form onSubmit={submit} className="space-y-3 rounded-md border border-slate-300 dark:border-slate-700 bg-white dark:bg-slate-900 p-4">
      <h2 className="text-lg font-semibold">Upload a contract</h2>
      <div className="flex flex-wrap items-end gap-4">
        <div className="space-y-1">
          <label htmlFor="contract-file" className="block text-sm font-medium">
            Contract file
          </label>
          <input
            id="contract-file"
            ref={input}
            type="file"
            accept={CONTRACT_EXTENSIONS.join(",")}
            className="text-sm"
            onChange={(e) => setFile(e.target.files?.[0])}
            aria-describedby="contract-file-hint"
          />
          <p id="contract-file-hint" className="text-xs text-slate-600 dark:text-slate-400">
            OpenAPI 3.x or Swagger 2.0 (JSON or YAML), GraphQL SDL, .proto or descriptor set, WSDL, or a
            zip/tar.gz archive of several files; at most 20 MiB.
          </p>
        </div>
        <div className="flex items-center gap-2">
          <input
            id="contract-activate"
            type="checkbox"
            checked={activate}
            onChange={(e) => setActivate(e.target.checked)}
          />
          <label htmlFor="contract-activate" className="text-sm font-medium">
            Activate after upload
          </label>
        </div>
        <button type="submit" className={primaryButton} disabled={!file || upload.isPending}>
          Upload
        </button>
      </div>
      {upload.data && !upload.isPending ? (
        <p role="status" className="text-sm text-green-800 dark:text-green-300">
          Uploaded version {upload.data.version} ({upload.data.status}, {upload.data.operations} operations).
        </p>
      ) : null}
      <ProblemAlert error={upload.error} />
    </form>
  );
}
