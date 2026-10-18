import { useEffect, useRef, useState, type KeyboardEvent } from "react";
import { useActiveOperations } from "../../api/queries";
import { subscribeRequests, type LiveFilters, type LiveRequestView } from "../../api/sse";
import { inputClass, primaryButton, secondaryButton } from "../../components/ConfirmDialog";
import { RequestDrawer } from "./RequestDrawer";
import { tableScroll } from "../../components/DataTable";

/** The table keeps at most this many rows, newest first. */
export const MAX_ROWS = 500;

const SOURCES = ["llm", "repaired", "fallback"] as const;

interface Row {
  key: number;
  record: LiveRequestView;
}

const cell = "px-3 py-2 align-top";
const rowButton =
  "rounded-sm text-left font-medium text-indigo-800 underline-offset-2 hover:underline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-indigo-700";

/** Moves focus between the row buttons of `tbody` with the arrow keys. */
function onArrowKeys(e: KeyboardEvent<HTMLTableSectionElement>) {
  if (e.key !== "ArrowDown" && e.key !== "ArrowUp") return;
  const buttons = Array.from(e.currentTarget.querySelectorAll<HTMLButtonElement>("button[data-row]"));
  const at = buttons.indexOf(e.target as HTMLButtonElement);
  if (at < 0) return;
  const next = buttons[e.key === "ArrowDown" ? at + 1 : at - 1];
  if (next) {
    e.preventDefault();
    next.focus();
  }
}

/** The live tail of a mock's served calls, filtered, pausable, newest first. */
export function LiveRequests({ ws, mock }: { ws: string; mock: string }) {
  const operations = useActiveOperations(ws, mock);
  const [filters, setFilters] = useState<LiveFilters>({});
  const [rows, setRows] = useState<Row[]>([]);
  const [paused, setPaused] = useState(false);
  const [missed, setMissed] = useState(0);
  const [dropped, setDropped] = useState<number>();
  const [interrupted, setInterrupted] = useState(false);
  const [selected, setSelected] = useState<LiveRequestView>();
  const pausedRef = useRef(false);
  const nextKey = useRef(0);

  const { operation, session, source } = filters;
  useEffect(() => {
    return subscribeRequests(
      ws,
      mock,
      { operation, session, source },
      (event) => {
        setInterrupted(false);
        if (event.kind === "lagged") {
          setDropped((d) => (d ?? 0) + event.dropped);
          return;
        }
        if (pausedRef.current) {
          setMissed((n) => n + 1);
          return;
        }
        const key = nextKey.current;
        nextKey.current += 1;
        setRows((current) => [{ key, record: event.record }, ...current].slice(0, MAX_ROWS));
      },
      () => setInterrupted(true),
    );
  }, [ws, mock, operation, session, source]);

  const togglePause = () => {
    pausedRef.current = !paused;
    setPaused(!paused);
    setMissed(0);
  };

  const filter = (key: keyof LiveFilters) => (value: string) =>
    setFilters((f) => ({ ...f, [key]: value === "" ? undefined : value }));

  return (
    <div className="space-y-4">
      <div className="flex flex-wrap items-end gap-4">
        <div className="space-y-1">
          <label htmlFor="live-operation" className="block text-sm font-medium">
            Operation
          </label>
          <select
            id="live-operation"
            className={inputClass}
            value={operation ?? ""}
            onChange={(e) => filter("operation")(e.target.value)}
          >
            <option value="">All operations</option>
            {operations.map((op) => (
              <option key={op} value={op}>
                {op}
              </option>
            ))}
          </select>
        </div>
        <div className="space-y-1">
          <label htmlFor="live-session" className="block text-sm font-medium">
            Session
          </label>
          <input
            id="live-session"
            className={inputClass}
            value={session ?? ""}
            onChange={(e) => filter("session")(e.target.value)}
          />
        </div>
        <div className="space-y-1">
          <label htmlFor="live-source" className="block text-sm font-medium">
            Source
          </label>
          <select
            id="live-source"
            className={inputClass}
            value={source ?? ""}
            onChange={(e) => filter("source")(e.target.value)}
          >
            <option value="">All sources</option>
            {SOURCES.map((s) => (
              <option key={s} value={s}>
                {s}
              </option>
            ))}
          </select>
        </div>
        <button type="button" className={paused ? primaryButton : secondaryButton} onClick={togglePause}>
          {paused ? "Resume" : "Pause"}
        </button>
      </div>
      <div aria-live="polite" className="space-y-2 text-sm">
        {paused ? (
          <p className="text-slate-700">
            {missed} {missed === 1 ? "request" : "requests"} arrived while paused.
          </p>
        ) : null}
        {interrupted ? <p className="text-amber-900">The live feed connection was interrupted.</p> : null}
      </div>
      {dropped !== undefined ? (
        <div
          role="status"
          className="flex items-center justify-between gap-4 rounded-md border border-amber-300 bg-amber-50 p-3 text-sm text-amber-900"
        >
          <span>{dropped} requests were dropped</span>
          <button type="button" className={secondaryButton} onClick={() => setDropped(undefined)}>
            Dismiss
          </button>
        </div>
      ) : null}
      <div className={tableScroll}>
        <table className="w-full border-collapse text-left text-sm">
          <caption className="sr-only">Live requests</caption>
          <thead className="border-b border-slate-300 text-slate-700">
            <tr>
              {["Time", "Method", "Path", "Status", "Source", "Latency", "Session"].map((h) => (
                <th key={h} scope="col" className="px-3 py-2 font-semibold">
                  {h}
                </th>
              ))}
            </tr>
          </thead>
          <tbody onKeyDown={onArrowKeys}>
            {rows.length === 0 ? (
              <tr>
                <td colSpan={7} className="px-3 py-6 text-center text-slate-600">
                  Waiting for requests…
                </td>
              </tr>
            ) : (
              rows.map(({ key, record }) => {
                const time = new Date(record.at).toLocaleTimeString();
                return (
                  <tr
                    key={key}
                    className="cursor-pointer border-b border-slate-200 hover:bg-slate-100"
                    onClick={() => setSelected(record)}
                  >
                    <td className={cell}>
                      <button
                        type="button"
                        data-row
                        className={rowButton}
                        aria-label={`Open ${record.method} ${record.path} at ${time}`}
                        onClick={(e) => {
                          e.stopPropagation();
                          setSelected(record);
                        }}
                      >
                        <time dateTime={record.at}>{time}</time>
                      </button>
                    </td>
                    <td className={cell}>
                      <code>{record.method}</code>
                    </td>
                    <td className={cell}>
                      <code className="break-all">{record.path}</code>
                    </td>
                    <td className={cell}>{record.status}</td>
                    <td className={cell}>{record.source ?? "—"}</td>
                    <td className={cell}>{`${record.latency_ms} ms`}</td>
                    <td className={cell}>
                      <code>{record.session}</code>
                    </td>
                  </tr>
                );
              })
            )}
          </tbody>
        </table>
      </div>
      {selected ? <RequestDrawer record={selected} onClose={() => setSelected(undefined)} /> : null}
    </div>
  );
}
