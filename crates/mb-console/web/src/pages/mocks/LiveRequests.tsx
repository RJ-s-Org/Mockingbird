import { useEffect, useRef, useState, type KeyboardEvent } from "react";
import { useActiveOperations } from "../../api/queries";
import { olderThan, recentRequests, subscribeRequests, type LiveFilters, type LiveRequestView } from "../../api/sse";
import { RequestDrawer } from "./RequestDrawer";
import {
  TableScroll,
  emptyCellClass,
  tableClass,
  tdClass,
  thClass,
  theadClass,
  trClickableClass,
} from "../../components/DataTable";
import { Method, ResponseSource, StatusCode } from "../../components/http";
import {
  Notice,
  cardClass,
  focusRing,
  inputClass,
  labelClass,
  primaryButton,
  secondaryButton,
} from "../../components/ui";

/** The table keeps at most this many rows, newest first. */
export const MAX_ROWS = 500;

const SOURCES = ["llm", "patched", "repaired", "fallback"] as const;

interface Row {
  key: number;
  record: LiveRequestView;
}

const nowrapCell = `${tdClass} whitespace-nowrap`;
const rowButton = `rounded-sm text-left font-medium text-accent tabular-nums underline-offset-2 hover:underline ${focusRing}`;

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

  const [history, setHistory] = useState<"loading" | "loaded">("loading");
  const { operation, session, source } = filters;
  // Each filter starts over: recent matching calls, then the live tail on top.
  const feed = JSON.stringify([ws, mock, operation, session, source]);
  const [shownFeed, setShownFeed] = useState(feed);
  if (shownFeed !== feed) {
    setShownFeed(feed);
    setRows([]);
    setHistory("loading");
  }

  useEffect(() => {
    let cancelled = false;
    recentRequests(ws, mock, { operation, session, source }).then((recent) => {
      if (cancelled) return;
      setRows((current) => {
        const older = olderThan(
          current.map((r) => r.record),
          recent,
        ).map((record, i) => ({ key: -1 - i, record }));
        return [...current, ...older].slice(0, MAX_ROWS);
      });
      setHistory("loaded");
    });
    const close = subscribeRequests(
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
    return () => {
      cancelled = true;
      close();
    };
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
      <div className={`${cardClass} flex flex-wrap items-end gap-4 p-4`}>
        <div className="min-w-48 space-y-1.5">
          <label htmlFor="live-operation" className={labelClass}>
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
        <div className="min-w-40 space-y-1.5">
          <label htmlFor="live-session" className={labelClass}>
            Session
          </label>
          <input
            id="live-session"
            className={inputClass}
            value={session ?? ""}
            onChange={(e) => filter("session")(e.target.value)}
          />
        </div>
        <div className="min-w-36 space-y-1.5">
          <label htmlFor="live-source" className={labelClass}>
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
        <button
          type="button"
          className={`${paused ? primaryButton : secondaryButton} sm:ml-auto`}
          onClick={togglePause}
        >
          <span
            aria-hidden="true"
            className={`size-2 rounded-full ${paused ? "bg-white/80" : interrupted ? "bg-warn-fg" : "bg-ok-fg"}`}
          />
          {paused ? "Resume" : "Pause"}
        </button>
      </div>
      {history === "loaded" && rows.length > 0 ? (
        <p className="text-sm text-fg-muted">Showing the latest recorded requests, then new ones as they arrive.</p>
      ) : null}
      <div aria-live="polite" className="space-y-2 text-sm">
        {paused ? (
          <p className="text-fg-muted">
            {missed} {missed === 1 ? "request" : "requests"} arrived while paused.
          </p>
        ) : null}
        {interrupted ? <p className="text-warn-fg">The live feed connection was interrupted.</p> : null}
      </div>
      {dropped !== undefined ? (
        <Notice tone="warn" role="status" className="flex items-center justify-between gap-4">
          <span>{dropped} requests were dropped</span>
          <button type="button" className={secondaryButton} onClick={() => setDropped(undefined)}>
            Dismiss
          </button>
        </Notice>
      ) : null}
      <TableScroll>
        <table className={tableClass}>
          <caption className="sr-only">Live requests</caption>
          <thead className={theadClass}>
            <tr>
              {["Time", "Method", "Path", "Status", "Source", "Latency", "Session"].map((h) => (
                <th key={h} scope="col" className={thClass}>
                  {h}
                </th>
              ))}
            </tr>
          </thead>
          <tbody onKeyDown={onArrowKeys}>
            {rows.length === 0 ? (
              <tr>
                <td colSpan={7} className={emptyCellClass}>
                  {history === "loading"
                    ? "Loading recent requests…"
                    : "No requests recorded yet. New ones appear here as they are served."}
                </td>
              </tr>
            ) : (
              rows.map(({ key, record }) => {
                const time = new Date(record.at).toLocaleTimeString();
                return (
                  <tr key={key} className={trClickableClass} onClick={() => setSelected(record)}>
                    <td className={nowrapCell}>
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
                    <td className={nowrapCell}>
                      <Method method={record.method} />
                    </td>
                    <td className={nowrapCell}>
                      <code>{record.path}</code>
                    </td>
                    <td className={tdClass}>
                      <StatusCode status={record.status} />
                    </td>
                    <td className={tdClass}>
                      <ResponseSource source={record.source} />
                    </td>
                    <td className={`${nowrapCell} tabular-nums text-fg-muted`}>{`${record.latency_ms} ms`}</td>
                    <td className={nowrapCell}>
                      <code className="text-fg-muted">{record.session}</code>
                    </td>
                  </tr>
                );
              })
            )}
          </tbody>
        </table>
      </TableScroll>
      {selected ? <RequestDrawer record={selected} onClose={() => setSelected(undefined)} /> : null}
    </div>
  );
}
