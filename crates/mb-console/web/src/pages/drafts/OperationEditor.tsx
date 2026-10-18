import { useEffect, useId, useRef, useState, type FormEvent } from "react";
import { ApiError } from "../../api/client";
import { usePutDraft } from "../../api/imports";
import {
  ConfirmDialog,
  dangerButton,
  inputClass,
  primaryButton,
  secondaryButton,
} from "../../components/ConfirmDialog";
import { ProblemAlert } from "../../components/ProblemAlert";
import {
  HTTP_METHODS,
  applyOperationEdit,
  parseSchemaText,
  readOperation,
  removeOperation,
  type OpenApiDoc,
  type OperationEdit,
  type OperationRef,
} from "./openapi";

interface Form {
  method: string;
  path: string;
  summary: string;
  request: string;
  responses: { status: string; text: string }[];
}

function schemaText(schema: object | null): string {
  return schema === null ? "" : JSON.stringify(schema, null, 2);
}

function toForm(edit: OperationEdit): Form {
  return {
    method: edit.method,
    path: edit.path,
    summary: edit.summary,
    request: schemaText(edit.requestSchema),
    responses: edit.responses.map((r) => ({ status: r.status, text: schemaText(r.schema) })),
  };
}

/**
 * Edits one operation of a draft: method, path, summary and the JSON schemas
 * of its request and responses. Save and Remove PUT the whole document.
 */
export function OperationEditor({
  ws,
  mock,
  version,
  document,
  operation,
  onClose,
  onDirtyChange,
}: {
  ws: string;
  mock: string;
  version: number;
  document: OpenApiDoc;
  operation: OperationRef;
  onClose: () => void;
  onDirtyChange: (dirty: boolean) => void;
}) {
  const title = `Edit ${operation.method} ${operation.path}`;
  let initial: OperationEdit;
  try {
    initial = readOperation(document, operation);
  } catch (e) {
    return (
      <section aria-label={title} className="space-y-3 rounded-md border border-slate-300 bg-white p-4">
        <ProblemAlert error={e} />
        <button type="button" className={secondaryButton} onClick={onClose}>
          Close
        </button>
      </section>
    );
  }
  return (
    <EditorForm
      ws={ws}
      mock={mock}
      version={version}
      document={document}
      operation={operation}
      initial={initial}
      title={title}
      onClose={onClose}
      onDirtyChange={onDirtyChange}
    />
  );
}

function EditorForm({
  ws,
  mock,
  version,
  document,
  operation,
  initial,
  title,
  onClose,
  onDirtyChange,
}: {
  ws: string;
  mock: string;
  version: number;
  document: OpenApiDoc;
  operation: OperationRef;
  initial: OperationEdit;
  title: string;
  onClose: () => void;
  onDirtyChange: (dirty: boolean) => void;
}) {
  const put = usePutDraft(ws, mock, version);
  const id = useId();
  const [start] = useState(() => toForm(initial));
  const [form, setForm] = useState(start);
  const [editError, setEditError] = useState<string>();
  const [confirmRemove, setConfirmRemove] = useState(false);

  const formRef = useRef<HTMLFormElement>(null);
  // Opened inline under its row: bring it into view and put focus on its first field.
  useEffect(() => {
    const el = formRef.current;
    el?.scrollIntoView?.({ block: "nearest" });
    el?.querySelector<HTMLElement>("select, input, textarea")?.focus();
  }, []);

  const dirty = JSON.stringify(form) !== JSON.stringify(start);
  useEffect(() => {
    onDirtyChange(dirty);
  }, [dirty, onDirtyChange]);
  useEffect(() => () => onDirtyChange(false), [onDirtyChange]);

  const request = parseSchemaText(form.request);
  const responses = form.responses.map((r) => ({ status: r.status, parsed: parseSchemaText(r.text) }));
  const invalid = request.error !== undefined || responses.some((r) => r.parsed.error !== undefined);

  const update = (patch: Partial<Form>) => {
    setForm((f) => ({ ...f, ...patch }));
    setEditError(undefined);
  };

  const save = (e: FormEvent) => {
    e.preventDefault();
    if (invalid) return;
    const edit: OperationEdit = {
      method: form.method,
      path: form.path.trim(),
      summary: form.summary,
      requestSchema: request.schema,
      responses: responses.map((r) => ({ status: r.status, schema: r.parsed.schema })),
    };
    let next: OpenApiDoc;
    try {
      next = applyOperationEdit(document, operation, edit);
    } catch (err) {
      setEditError(err instanceof Error ? err.message : String(err));
      return;
    }
    put.mutate(next, { onSuccess: onClose });
  };

  const remove = () => {
    put.mutate(removeOperation(document, operation), {
      onSuccess: onClose,
      onSettled: () => setConfirmRemove(false),
    });
  };

  return (
    <form
      ref={formRef}
      aria-label={title}
      onSubmit={save}
      noValidate
      className="space-y-4 rounded-md border border-slate-300 bg-white p-4"
    >
      <h3 className="text-base font-semibold">{title}</h3>
      <div className="grid gap-4 sm:grid-cols-[8rem_1fr]">
        <div className="space-y-1">
          <label htmlFor={`${id}-method`} className="block text-sm font-medium">
            Method
          </label>
          <select
            id={`${id}-method`}
            className={inputClass}
            value={form.method}
            onChange={(e) => update({ method: e.target.value })}
          >
            {HTTP_METHODS.map((m) => (
              <option key={m} value={m}>
                {m}
              </option>
            ))}
          </select>
        </div>
        <div className="space-y-1">
          <label htmlFor={`${id}-path`} className="block text-sm font-medium">
            Path
          </label>
          <input
            id={`${id}-path`}
            className={`${inputClass} font-mono`}
            value={form.path}
            onChange={(e) => update({ path: e.target.value })}
          />
        </div>
      </div>
      <div className="space-y-1">
        <label htmlFor={`${id}-summary`} className="block text-sm font-medium">
          Summary
        </label>
        <input
          id={`${id}-summary`}
          className={inputClass}
          value={form.summary}
          onChange={(e) => update({ summary: e.target.value })}
        />
      </div>
      <SchemaField
        id={`${id}-request`}
        label="Request schema"
        value={form.request}
        error={request.error}
        onChange={(text) => update({ request: text })}
      />
      {form.responses.map((r, i) => (
        <SchemaField
          key={r.status}
          id={`${id}-response-${r.status}`}
          label={`Response ${r.status} schema`}
          value={r.text}
          error={responses[i]?.parsed.error}
          onChange={(text) =>
            update({ responses: form.responses.map((x, j) => (j === i ? { status: x.status, text } : x)) })
          }
        />
      ))}
      {editError ? (
        <div role="alert" className="rounded-md border border-red-300 bg-red-50 p-3 text-sm text-red-900">
          {editError}
        </div>
      ) : null}
      <SaveError error={put.error} />
      <div className="flex flex-wrap gap-2">
        <button type="submit" className={primaryButton} disabled={invalid || put.isPending}>
          Save
        </button>
        <button type="button" className={secondaryButton} onClick={onClose}>
          Cancel
        </button>
        <button
          type="button"
          className={`${dangerButton} ml-auto`}
          disabled={put.isPending}
          onClick={() => setConfirmRemove(true)}
        >
          Remove operation
        </button>
      </div>
      {confirmRemove ? (
        <ConfirmDialog
          title="Remove operation"
          message={`Remove ${operation.method} ${operation.path} from the draft?`}
          confirmLabel="Remove"
          busy={put.isPending}
          onConfirm={remove}
          onCancel={() => setConfirmRemove(false)}
        />
      ) : null}
    </form>
  );
}

/** A JSON schema textarea; empty means no schema. */
function SchemaField({
  id,
  label,
  value,
  error,
  onChange,
}: {
  id: string;
  label: string;
  value: string;
  error?: string;
  onChange: (text: string) => void;
}) {
  const errorId = `${id}-error`;
  return (
    <div className="space-y-1">
      <label htmlFor={id} className="block text-sm font-medium">
        {label}
      </label>
      <textarea
        id={id}
        rows={6}
        spellCheck={false}
        className={`${inputClass} font-mono text-xs`}
        value={value}
        aria-invalid={error ? true : undefined}
        aria-describedby={error ? errorId : undefined}
        onChange={(e) => onChange(e.target.value)}
      />
      {error ? (
        <p id={errorId} className="text-sm text-red-800">
          {error}
        </p>
      ) : null}
    </div>
  );
}

/** A failed save; a 422 lists each problem as `pointer: message`. */
function SaveError({ error }: { error: unknown }) {
  const problems = error instanceof ApiError ? (error.problem?.errors ?? []) : [];
  if (!error) return null;
  if (problems.length === 0) return <ProblemAlert error={error} />;
  const problem = (error as ApiError).problem;
  return (
    <div role="alert" className="rounded-md border border-red-300 bg-red-50 p-3 text-sm text-red-900">
      <p className="font-semibold">{problem?.title || "The draft was not saved"}</p>
      {problem?.detail ? <p className="mt-1">{problem.detail}</p> : null}
      <ul role="list" aria-label="Problems" className="mt-2 list-disc space-y-1 pl-5">
        {problems.map((p, i) => (
          <li key={i}>
            <code>{p.pointer}</code>: {p.message}
          </li>
        ))}
      </ul>
    </div>
  );
}
