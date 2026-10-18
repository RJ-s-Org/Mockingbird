import { useEffect, useId, useRef, useState } from "react";
import { Link } from "@tanstack/react-router";
import { useDraft, usePreview, useApproveDraft, type DraftOperation, type DraftView } from "../../api/imports";
import { useActivate } from "../../api/queries";
import { useMe } from "../../auth/me";
import { can } from "../../auth/permissions";
import { ConfirmDialog, inputClass, primaryButton, secondaryButton } from "../../components/ConfirmDialog";
import { DataTable, columnHelper } from "../../components/DataTable";
import { ProblemAlert } from "../../components/ProblemAlert";
import { linkClass } from "../mocks/MocksList";
import { ConfidenceBadge } from "./ConfidenceBadge";
import { DraftDiff } from "./DraftDiff";
import { OperationEditor } from "./OperationEditor";
import { readOperation, type OpenApiDoc, type OperationRef } from "./openapi";

const col = columnHelper<DraftOperation>();

function targetOf(op: OperationRef): string {
  return `${op.method} ${op.path}`;
}

function plural(n: number, one: string, many: string): string {
  return `${n} ${n === 1 ? one : many}`;
}

/**
 * An imported contract version for review: where it came from, its warnings
 * and diff, its operations with confidence and provenance, a per-operation
 * editor and preview, and approval followed by activation.
 */
export function DraftReview({ ws, mock, version }: { ws: string; mock: string; version: number }) {
  const me = useMe();
  const draft = useDraft(ws, mock, version);
  const canEdit = me.data ? can(me.data, ws, "edit") : false;

  if (!draft.data) {
    return draft.error ? <ProblemAlert error={draft.error} /> : <p className="text-slate-700">Loading…</p>;
  }
  return <Review ws={ws} mock={mock} version={version} view={draft.data} canEdit={canEdit} />;
}

function Review({
  ws,
  mock,
  version,
  view,
  canEdit,
}: {
  ws: string;
  mock: string;
  version: number;
  view: DraftView;
  canEdit: boolean;
}) {
  const approve = useApproveDraft(ws, mock, version);
  const activate = useActivate(ws, mock);
  const preview = usePreview(ws, mock, version);
  const [editing, setEditing] = useState<OperationRef>();
  const [dirty, setDirty] = useState(false);
  const [previewing, setPreviewing] = useState<DraftOperation>();
  const [confirmApprove, setConfirmApprove] = useState(false);
  const approveHint = useId();

  const document = view.document as OpenApiDoc;
  const isDraft = view.status === "draft";
  const { meta } = view;
  const source = meta.source_name ?? meta.source_url ?? "—";
  // HAR, Postman and other deterministic imports read no chunks and spend no tokens.
  const deterministic =
    meta.chunk_count === 0 && meta.usage.prompt_tokens === 0 && meta.usage.completion_tokens === 0;

  const startPreview = (op: DraftOperation) => {
    setPreviewing(op);
    preview.reset();
    preview.mutate({ operation_id: op.id });
  };

  const columns = [
    col.accessor("method", { header: "Method" }),
    col.accessor("path", { header: "Path", cell: (info) => <code>{info.getValue()}</code> }),
    col.accessor("summary", { header: "Summary", cell: (info) => info.getValue() ?? "" }),
    col.accessor("confidence", {
      header: "Confidence",
      cell: (info) => <ConfidenceBadge confidence={info.getValue()} />,
    }),
    col.accessor("edited", {
      header: "Edited",
      cell: (info) =>
        info.getValue() ? (
          <span className="rounded border border-indigo-700 bg-indigo-50 px-1.5 py-0.5 text-xs font-medium text-indigo-900">
            edited
          </span>
        ) : null,
    }),
    col.display({
      id: "provenance",
      header: "Provenance",
      cell: ({ row }) => <Provenance op={row.original} />,
    }),
    ...(canEdit
      ? [
          col.display({
            id: "actions",
            header: "Actions",
            cell: ({ row }) => {
              const op = row.original;
              const target = targetOf(op);
              return (
                <div className="flex flex-wrap gap-2">
                  <button
                    type="button"
                    className={secondaryButton}
                    aria-label={`Edit ${target}`}
                    title={isDraft ? undefined : "Only a draft version can be edited"}
                    disabled={!isDraft}
                    onClick={() => {
                      setDirty(false);
                      setEditing({ method: op.method, path: op.path });
                    }}
                  >
                    Edit
                  </button>
                  <button
                    type="button"
                    className={secondaryButton}
                    aria-label={`Preview ${target}`}
                    onClick={() => startPreview(op)}
                  >
                    Preview
                  </button>
                </div>
              );
            },
          }),
        ]
      : []),
  ];

  return (
    <div className="space-y-6">
      <header className="space-y-3">
        <div className="flex flex-wrap items-center gap-3">
          <h2 className="text-lg font-semibold">{`Draft v${view.version}`}</h2>
          <p className="text-sm font-medium" aria-live="polite">{`Status: ${view.status}`}</p>
        </div>
        <dl className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-1 text-sm">
          <dt className="font-medium">Source</dt>
          <dd className="break-all">{`${meta.source_kind} · ${source}`}</dd>
          {deterministic ? (
            <>
              <dt className="font-medium">Generation</dt>
              <dd>Deterministic import (no LLM used)</dd>
            </>
          ) : (
            <>
              <dt className="font-medium">Chunks</dt>
              <dd>{`${meta.chunks_processed} of ${meta.chunk_count} chunks processed`}</dd>
              <dt className="font-medium">Tokens</dt>
              <dd>{`${meta.usage.prompt_tokens} prompt + ${meta.usage.completion_tokens} completion tokens`}</dd>
            </>
          )}
        </dl>
        {meta.warnings.length > 0 ? (
          <div className="space-y-1">
            <h3 className="text-sm font-semibold">{`Warnings (${meta.warnings.length})`}</h3>
            <ul
              role="list"
              aria-label="Warnings"
              className="list-disc space-y-1 rounded-md border border-amber-300 bg-amber-50 p-3 pl-8 text-sm text-amber-950"
            >
              {meta.warnings.map((w, i) => (
                <li key={i}>
                  <code className="font-semibold">{w.code}</code>: {w.message}
                </li>
              ))}
            </ul>
          </div>
        ) : null}
        {view.diff ? <DraftDiff diff={view.diff} /> : null}
      </header>

      <Approval
        ws={ws}
        mock={mock}
        view={view}
        canEdit={canEdit}
        dirty={dirty}
        hintId={approveHint}
        approving={approve.isPending}
        activating={activate.isPending}
        onApprove={() => setConfirmApprove(true)}
        onActivate={() => activate.mutate(version)}
      />
      <ProblemAlert error={approve.error ?? activate.error} />

      {/* The editor and the preview open inline, right under their operation's row. */}
      <DataTable
        caption="Draft operations"
        columns={columns}
        data={view.operations}
        empty="The draft has no operations."
        expanded={(op) => {
          const edit = editing !== undefined && isDraft && canEdit && targetOf(editing) === targetOf(op);
          const show = previewing !== undefined && canEdit && previewing.id === op.id;
          if (!edit && !show) return null;
          return (
            <div className="space-y-3 pt-1">
              {edit ? (
                <OperationEditor
                  key={targetOf(editing)}
                  ws={ws}
                  mock={mock}
                  version={version}
                  document={document}
                  operation={editing}
                  onClose={() => setEditing(undefined)}
                  onDirtyChange={setDirty}
                />
              ) : null}
              {show ? (
                <PreviewPanel
                  key={previewing.id}
                  op={previewing}
                  document={document}
                  preview={preview}
                  onClose={() => setPreviewing(undefined)}
                />
              ) : null}
            </div>
          );
        }}
      />

      {confirmApprove ? (
        <ConfirmDialog
          title={`Approve draft v${view.version}`}
          message={
            <>
              <p>{`Approving makes version ${view.version} read-only; it can then be activated.`}</p>
              {meta.warnings.length > 0 ? (
                <p className="mt-2 font-medium">
                  {`This draft has ${plural(meta.warnings.length, "warning", "warnings")}; review them before approving.`}
                </p>
              ) : null}
            </>
          }
          confirmLabel="Approve"
          busy={approve.isPending}
          onConfirm={() => approve.mutate(undefined, { onSettled: () => setConfirmApprove(false) })}
          onCancel={() => setConfirmApprove(false)}
        />
      ) : null}
    </div>
  );
}

/** Approve (a draft), Activate (an approved version), and the way back to Contracts. */
function Approval({
  ws,
  mock,
  view,
  canEdit,
  dirty,
  hintId,
  approving,
  activating,
  onApprove,
  onActivate,
}: {
  ws: string;
  mock: string;
  view: DraftView;
  canEdit: boolean;
  dirty: boolean;
  hintId: string;
  approving: boolean;
  activating: boolean;
  onApprove: () => void;
  onActivate: () => void;
}) {
  const back =
    view.status === "approved" || view.status === "active" ? (
      <Link to="/w/$ws/mocks/$mock/contracts" params={{ ws, mock }} className={linkClass}>
        Back to contracts
      </Link>
    ) : null;
  if (!canEdit) return back ? <div>{back}</div> : null;
  return (
    <div className="flex flex-wrap items-center gap-3">
      {view.status === "draft" ? (
        <>
          <button
            type="button"
            className={primaryButton}
            disabled={dirty || approving}
            aria-describedby={dirty ? hintId : undefined}
            onClick={onApprove}
          >
            Approve
          </button>
          {dirty ? (
            <p id={hintId} className="text-sm text-slate-700">
              Save or cancel the open edit before approving.
            </p>
          ) : null}
        </>
      ) : null}
      {view.status === "approved" ? (
        <button type="button" className={primaryButton} disabled={activating} onClick={onActivate}>
          Activate
        </button>
      ) : null}
      {back}
    </div>
  );
}

/** Shows an operation's provenance excerpts on demand. */
function Provenance({ op }: { op: DraftOperation }) {
  const [open, setOpen] = useState(false);
  const id = useId();
  if (op.provenance.length === 0) return <span className="text-slate-600">—</span>;
  return (
    <div className="space-y-2">
      <button
        type="button"
        className="text-sm text-indigo-700 underline focus-visible:outline-2 focus-visible:outline-indigo-700"
        aria-expanded={open}
        aria-controls={id}
        aria-label={`Show provenance of ${targetOf(op)}`}
        onClick={() => setOpen((o) => !o)}
      >
        Show provenance
      </button>
      <ul id={id} role="list" hidden={!open} className="space-y-2">
        {op.provenance.map((p, i) => (
          <li key={i} className="space-y-1">
            <blockquote className="border-l-4 border-slate-300 pl-3 text-sm whitespace-pre-wrap text-slate-800">
              {p.excerpt}
            </blockquote>
            <p className="text-xs text-slate-700">
              <cite>{p.location}</cite>
              {p.chunk != null ? <span>{` (chunk ${p.chunk})`}</span> : null}
            </p>
          </li>
        ))}
      </ul>
    </div>
  );
}

/** A generated response of one operation, optionally for a chosen declared status. */
function PreviewPanel({
  op,
  document,
  preview,
  onClose,
}: {
  op: DraftOperation;
  document: OpenApiDoc;
  preview: ReturnType<typeof usePreview>;
  onClose: () => void;
}) {
  const [status, setStatus] = useState("");
  const selectId = useId();
  const ref = useRef<HTMLElement>(null);
  useEffect(() => {
    ref.current?.scrollIntoView?.({ block: "nearest" });
  }, []);
  let statuses: string[] = [];
  try {
    statuses = readOperation(document, op).responses.map((r) => r.status);
  } catch {
    // Not in the document (it cannot be edited either): offer the default response only.
  }
  const generate = () =>
    preview.mutate(status === "" ? { operation_id: op.id } : { operation_id: op.id, status: Number(status) });
  const result = preview.data;
  return (
    <section
      ref={ref}
      aria-label={`Preview of ${targetOf(op)}`}
      className="space-y-3 rounded-md border border-slate-300 bg-white p-4"
    >
      <h3 className="text-base font-semibold">{`Preview of ${targetOf(op)}`}</h3>
      <div className="flex flex-wrap items-end gap-3">
        <div className="space-y-1">
          <label htmlFor={selectId} className="block text-sm font-medium">
            Response status
          </label>
          <select id={selectId} className={inputClass} value={status} onChange={(e) => setStatus(e.target.value)}>
            <option value="">Default</option>
            {statuses.map((s) => (
              <option key={s} value={s}>
                {s}
              </option>
            ))}
          </select>
        </div>
        <button type="button" className={primaryButton} disabled={preview.isPending} onClick={generate}>
          Generate preview
        </button>
        <button type="button" className={secondaryButton} onClick={onClose}>
          Close preview
        </button>
      </div>
      {preview.isPending ? <p className="text-sm text-slate-700">Generating…</p> : null}
      <ProblemAlert error={preview.error} />
      {result && !preview.isPending ? (
        <div className="space-y-2">
          <p className="text-sm font-medium">{`Status ${result.status} · source ${result.source}`}</p>
          <pre
            aria-label="Preview body"
            tabIndex={0}
            className="max-h-96 overflow-auto rounded-md bg-slate-100 p-3 text-xs text-slate-900"
          >
            {JSON.stringify(result.body, null, 2)}
          </pre>
        </div>
      ) : null}
    </section>
  );
}
