import { useRef, useState, type FormEvent } from "react";
import {
  fetchSessionExport,
  useResetSession,
  useSeedSession,
  useSessionExport,
  useSessions,
  type SessionInfo,
  type StoredResource,
} from "../../api/queries";
import { useMe } from "../../auth/me";
import { can } from "../../auth/permissions";
import {
  ConfirmDialog,
  dangerButton,
  inputClass,
  primaryButton,
  secondaryButton,
} from "../../components/ConfirmDialog";
import { DataTable, columnHelper } from "../../components/DataTable";
import { Drawer } from "../../components/Drawer";
import { ProblemAlert } from "../../components/ProblemAlert";
import { parseJson } from "../../lib/json";

const col = columnHelper<SessionInfo>();

/** Saves `value` as pretty JSON named `fileName`, through a Blob URL. */
export function downloadJson(fileName: string, value: unknown): void {
  const blob = new Blob([JSON.stringify(value, null, 2)], { type: "application/json" });
  const url = URL.createObjectURL(blob);
  const a = document.createElement("a");
  a.href = url;
  a.download = fileName;
  document.body.appendChild(a);
  a.click();
  a.remove();
  setTimeout(() => URL.revokeObjectURL(url), 0);
}

/** A mock's live sessions: inspect, export, reset and seed. */
export function Sessions({ ws, mock }: { ws: string; mock: string }) {
  const me = useMe();
  const sessions = useSessions(ws, mock);
  const reset = useResetSession(ws, mock);
  const [inspecting, setInspecting] = useState<string>();
  const [resetting, setResetting] = useState<string>();
  const [exportError, setExportError] = useState<unknown>();

  const canEdit = me.data ? can(me.data, ws, "edit") : false;

  const exportSession = async (session: string) => {
    setExportError(undefined);
    try {
      downloadJson(`${mock}-${session}.json`, await fetchSessionExport(ws, mock, session));
    } catch (e) {
      setExportError(e);
    }
  };

  const columns = [
    col.accessor("session", { header: "Session", cell: (info) => <code>{info.getValue()}</code> }),
    col.accessor("resources", { header: "Resources" }),
    col.accessor("last_touched", {
      header: "Last touched",
      cell: (info) => <time dateTime={info.getValue()}>{new Date(info.getValue()).toLocaleString()}</time>,
    }),
    col.display({
      id: "actions",
      header: "Actions",
      cell: ({ row }) => {
        const s = row.original.session;
        return (
          <div className="flex flex-wrap gap-2">
            <button
              type="button"
              className={secondaryButton}
              aria-label={`Inspect session ${s}`}
              onClick={() => setInspecting(s)}
            >
              Inspect
            </button>
            <button
              type="button"
              className={secondaryButton}
              aria-label={`Export session ${s}`}
              onClick={() => void exportSession(s)}
            >
              Export
            </button>
            {canEdit ? (
              <button
                type="button"
                className={dangerButton}
                aria-label={`Reset session ${s}`}
                onClick={() => setResetting(s)}
              >
                Reset
              </button>
            ) : null}
          </div>
        );
      },
    }),
  ];

  return (
    <div className="space-y-6">
      {canEdit ? <SeedForm ws={ws} mock={mock} /> : null}
      <ProblemAlert error={sessions.error ?? reset.error ?? exportError} />
      {sessions.data ? (
        <DataTable caption="Sessions" columns={columns} data={sessions.data} empty="No sessions yet." />
      ) : sessions.isPending ? (
        <p className="text-slate-700 dark:text-slate-300">Loading…</p>
      ) : null}
      {inspecting !== undefined ? (
        <SessionDrawer ws={ws} mock={mock} session={inspecting} onClose={() => setInspecting(undefined)} />
      ) : null}
      {resetting !== undefined ? (
        <ConfirmDialog
          title={`Reset session ${resetting}?`}
          message="Every resource stored in this session is deleted."
          confirmLabel="Reset"
          busy={reset.isPending}
          onConfirm={() => reset.mutate(resetting, { onSettled: () => setResetting(undefined) })}
          onCancel={() => setResetting(undefined)}
        />
      ) : null}
    </div>
  );
}

/** The pretty-printed export of one session. */
function SessionDrawer({
  ws,
  mock,
  session,
  onClose,
}: {
  ws: string;
  mock: string;
  session: string;
  onClose: () => void;
}) {
  const exported = useSessionExport(ws, mock, session);
  return (
    <Drawer title={`Session ${session}`} onClose={onClose}>
      <ProblemAlert error={exported.error} />
      {exported.data ? (
        <pre tabIndex={0} className="overflow-x-auto rounded-md bg-slate-100 dark:bg-slate-800 p-3 text-xs text-slate-900 dark:text-slate-100">
          {JSON.stringify(exported.data, null, 2)}
        </pre>
      ) : exported.isPending ? (
        <p className="text-slate-700 dark:text-slate-300">Loading…</p>
      ) : null}
    </Drawer>
  );
}

/** The file's resources, or why they cannot be seeded. */
function parseResources(text: string): { resources: StoredResource[] } | { error: string } {
  const parsed = parseJson("The file", text);
  if ("error" in parsed) return parsed;
  const value = parsed.value;
  if (!Array.isArray(value)) return { error: "The file must contain a JSON array of resources." };
  return { resources: value as StoredResource[] };
}

/** Replaces a session's resources with a JSON file (a session export). */
function SeedForm({ ws, mock }: { ws: string; mock: string }) {
  const seed = useSeedSession(ws, mock);
  const input = useRef<HTMLInputElement>(null);
  const [session, setSession] = useState("");
  const [file, setFile] = useState<File>();
  const [error, setError] = useState<string>();
  const [seeded, setSeeded] = useState<string>();

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    if (!file) return;
    setSeeded(undefined);
    const parsed = parseResources(await file.text());
    if ("error" in parsed) {
      setError(parsed.error);
      return;
    }
    setError(undefined);
    const n = parsed.resources.length;
    seed.mutate(
      { session, resources: parsed.resources },
      {
        onSuccess: () => {
          setSeeded(`Seeded session ${session} with ${n} ${n === 1 ? "resource" : "resources"}.`);
          setFile(undefined);
          if (input.current) input.current.value = "";
        },
      },
    );
  };

  return (
    <form
      onSubmit={(e) => void submit(e)}
      className="space-y-3 rounded-md border border-slate-300 dark:border-slate-700 bg-white dark:bg-slate-900 p-4"
    >
      <h2 className="text-lg font-semibold">Seed a session</h2>
      <div className="flex flex-wrap items-end gap-4">
        <div className="space-y-1">
          <label htmlFor="seed-session" className="block text-sm font-medium">
            Session to seed
          </label>
          <input
            id="seed-session"
            className={inputClass}
            value={session}
            required
            maxLength={128}
            onChange={(e) => setSession(e.target.value)}
          />
        </div>
        <div className="space-y-1">
          <label htmlFor="seed-file" className="block text-sm font-medium">
            Resources file
          </label>
          <input
            id="seed-file"
            ref={input}
            type="file"
            accept=".json,application/json"
            className="text-sm"
            onChange={(e) => {
              setFile(e.target.files?.[0]);
              setError(undefined);
            }}
            aria-invalid={error !== undefined}
            aria-describedby={error ? "seed-error" : "seed-file-hint"}
          />
          <p id="seed-file-hint" className="text-xs text-slate-600 dark:text-slate-400">
            A JSON array of resources, as Export saves it. Replaces the session&apos;s contents.
          </p>
        </div>
        <button type="submit" className={primaryButton} disabled={!file || !session || seed.isPending}>
          Seed
        </button>
      </div>
      {error ? (
        <p id="seed-error" className="text-sm text-red-800 dark:text-red-300">
          {error}
        </p>
      ) : null}
      {seeded && !seed.isPending ? (
        <p role="status" className="text-sm text-green-800 dark:text-green-300">
          {seeded}
        </p>
      ) : null}
      <ProblemAlert error={seed.error} focus />
    </form>
  );
}
