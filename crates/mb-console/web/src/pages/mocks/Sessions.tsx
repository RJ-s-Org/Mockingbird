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
import { ConfirmDialog } from "../../components/ConfirmDialog";
import { DataTable, columnHelper } from "../../components/DataTable";
import { Drawer } from "../../components/Drawer";
import { ProblemAlert } from "../../components/ProblemAlert";
import { parseJson } from "../../lib/json";
import {
  Card,
  Loading,
  SavedNote,
  errorTextClass,
  hintClass,
  inputClass,
  labelClass,
  preClass,
  primaryButton,
  secondaryButton,
} from "../../components/ui";

const col = columnHelper<SessionInfo>();

/** Saves `value` as pretty JSON named `fileName`, through a Blob URL. */
export function downloadJson(fileName: string, value: unknown): void {
  const blob = new Blob([JSON.stringify(value, null, 2)], {
    type: "application/json",
  });
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
    col.accessor("session", {
      header: "Session",
      cell: (info) => <code className="font-medium whitespace-nowrap">{info.getValue()}</code>,
    }),
    col.accessor("resources", {
      header: "Resources",
      cell: (info) => <span className="tabular-nums">{info.getValue()}</span>,
    }),
    col.accessor("last_touched", {
      header: "Last touched",
      cell: (info) => (
        <time dateTime={info.getValue()} className="whitespace-nowrap text-fg-muted">
          {new Date(info.getValue()).toLocaleString()}
        </time>
      ),
    }),
    col.display({
      id: "actions",
      header: "Actions",
      cell: ({ row }) => {
        const s = row.original.session;
        return (
          <div className="flex flex-nowrap gap-2">
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
                className={secondaryButton}
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
        <Loading />
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
          onConfirm={() =>
            reset.mutate(resetting, {
              onSettled: () => setResetting(undefined),
            })
          }
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
        <pre tabIndex={0} className={preClass}>
          {JSON.stringify(exported.data, null, 2)}
        </pre>
      ) : exported.isPending ? (
        <Loading />
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
    <Card title="Seed a session">
      <form onSubmit={(e) => void submit(e)} className="space-y-4">
        <div className="flex flex-wrap items-start gap-5">
          <div className="min-w-48 space-y-1.5">
            <label htmlFor="seed-session" className={labelClass}>
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
          <div className="min-w-0 flex-1 space-y-1.5">
            <label htmlFor="seed-file" className={labelClass}>
              Resources file
            </label>
            <input
              id="seed-file"
              ref={input}
              type="file"
              accept=".json,application/json"
              className="block w-full text-sm text-fg-muted file:mr-3 file:rounded-md file:border file:border-line-strong file:bg-surface file:px-3 file:py-1.5 file:text-sm file:font-medium file:text-fg hover:file:bg-surface-2"
              onChange={(e) => {
                setFile(e.target.files?.[0]);
                setError(undefined);
              }}
              aria-invalid={error !== undefined}
              aria-describedby={error ? "seed-error" : "seed-file-hint"}
            />
            <p id="seed-file-hint" className={hintClass}>
              A JSON array of resources, as Export saves it. Replaces the session&apos;s contents.
            </p>
          </div>
        </div>
        {error ? (
          <p id="seed-error" className={errorTextClass}>
            {error}
          </p>
        ) : null}
        <ProblemAlert error={seed.error} focus />
        <div className="flex flex-wrap items-center gap-3 border-t border-line pt-4">
          <button type="submit" className={primaryButton} disabled={!file || !session || seed.isPending}>
            Seed
          </button>
          {seeded && !seed.isPending ? <SavedNote>{seeded}</SavedNote> : null}
        </div>
      </form>
    </Card>
  );
}
