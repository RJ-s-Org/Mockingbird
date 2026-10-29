import { useRef, useState, type FormEvent } from "react";
import { Link } from "@tanstack/react-router";
import {
  contractSourceUrl,
  useActivate,
  useContracts,
  useDeleteContract,
  useMock,
  useUploadContract,
  CONTRACT_EXTENSIONS,
  type ContractSummary,
} from "../../api/queries";
import { useMe } from "../../auth/me";
import { can } from "../../auth/permissions";
import { ConfirmDialog } from "../../components/ConfirmDialog";
import { DataTable, columnHelper } from "../../components/DataTable";
import { ProblemAlert } from "../../components/ProblemAlert";
import { OperationsTable } from "./OperationsTable";
import { readOnlyReason } from "./Overview";
import {
  Badge,
  Card,
  Checkbox,
  Loading,
  Notice,
  hintClass,
  labelClass,
  linkClass,
  primaryButton,
  secondaryButton,
  type Tone,
} from "../../components/ui";

const col = columnHelper<ContractSummary>();

function statusTone(status: string): Tone {
  if (status === "active") return "ok";
  if (status === "draft") return "info";
  return "neutral";
}

/** A mock's contract versions: upload, activate or roll back, inspect, discard. */
export function Contracts({ ws, mock }: { ws: string; mock: string }) {
  const me = useMe();
  const m = useMock(ws, mock);
  const versions = useContracts(ws, mock);
  const activate = useActivate(ws, mock);
  const discard = useDeleteContract(ws, mock);
  const [viewing, setViewing] = useState<number>();
  const [discarding, setDiscarding] = useState<number>();

  const canEdit = me.data ? can(me.data, ws, "edit") : false;
  const reason = m.data ? readOnlyReason(m.data, canEdit) : "";
  const active = m.data?.active_version ?? null;

  const columns = [
    col.accessor("version", {
      header: "Version",
      cell: (info) => <span className="font-medium whitespace-nowrap">{`v${info.getValue()}`}</span>,
    }),
    col.accessor("status", {
      header: "Status",
      cell: (info) => <Badge tone={statusTone(info.getValue())}>{info.getValue()}</Badge>,
    }),
    col.accessor("source_digest", {
      header: "Digest",
      cell: (info) => <code title={info.getValue()} className="text-xs text-fg-muted">
          {info.getValue().slice(0, 12)}
        </code>,
    }),
    col.accessor("created_by", { header: "Created by", cell: (info) => info.getValue() ?? "—" }),
    col.accessor("created_at", {
      header: "Created at",
      cell: (info) => (
        <time dateTime={info.getValue()} className="whitespace-nowrap">
          {new Date(info.getValue()).toLocaleString()}
        </time>
      ),
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
            {reason === undefined && v.status !== "active" ? (
              <button
                type="button"
                className={secondaryButton}
                aria-label={`Discard version ${v.version}`}
                onClick={() => setDiscarding(v.version)}
              >
                Discard
              </button>
            ) : null}
          </div>
        );
      },
    }),
  ];

  return (
    <div className="space-y-6">
      {reason ? (
        <Notice tone="warn">{reason}</Notice>
      ) : null}
      {reason === undefined ? <UploadForm ws={ws} mock={mock} /> : null}
      <ProblemAlert error={activate.error ?? discard.error ?? versions.error} />
      {versions.data ? (
        <DataTable
          caption="Contract versions"
          columns={columns}
          data={versions.data}
          empty="No contract uploaded yet."
        />
      ) : versions.isPending ? (
        <Loading />
      ) : null}
      {viewing !== undefined ? (
        <OperationsTable ws={ws} mock={mock} version={viewing} onClose={() => setViewing(undefined)} />
      ) : null}
      {discarding !== undefined ? (
        <ConfirmDialog
          title={`Discard version ${discarding}?`}
          message="Its source and compiled contract are deleted, with the finished import that produced it. This cannot be undone."
          confirmLabel="Discard"
          busy={discard.isPending}
          onConfirm={() => discard.mutate(discarding, { onSettled: () => setDiscarding(undefined) })}
          onCancel={() => setDiscarding(undefined)}
        />
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
    <Card title="Upload a contract">
      <form onSubmit={submit} className="space-y-4">
        <div className="flex flex-wrap items-end gap-x-6 gap-y-4">
          <div className="min-w-0 max-w-xl space-y-1.5">
            <label htmlFor="contract-file" className={labelClass}>
              Contract file
            </label>
            <input
              id="contract-file"
              ref={input}
              type="file"
              accept={CONTRACT_EXTENSIONS.join(",")}
              className="block w-full text-sm text-fg file:mr-3 file:rounded-md file:border file:border-line-strong file:bg-surface-2 file:px-3 file:py-1.5 file:text-sm file:font-medium file:text-fg hover:file:bg-surface"
              onChange={(e) => setFile(e.target.files?.[0])}
              aria-describedby="contract-file-hint"
            />
            <p id="contract-file-hint" className={hintClass}>
              OpenAPI 3.x or Swagger 2.0 (JSON or YAML), GraphQL SDL, .proto or descriptor set, WSDL, or a
              zip/tar.gz archive of several files; at most 20 MiB.
            </p>
          </div>
          <div className="pb-1">
            <Checkbox id="contract-activate" label="Activate after upload" checked={activate} onChange={setActivate} />
          </div>
        </div>
        <div className="flex flex-wrap items-center gap-3 border-t border-line pt-4">
          <button type="submit" className={primaryButton} disabled={!file || upload.isPending}>
            Upload
          </button>
          {upload.data && !upload.isPending ? (
            <p role="status" className="text-sm font-medium text-ok-fg">
              Uploaded version {upload.data.version} ({upload.data.status}, {upload.data.operations} operations).
            </p>
          ) : null}
        </div>
        <ProblemAlert error={upload.error} />
      </form>
    </Card>
  );
}
