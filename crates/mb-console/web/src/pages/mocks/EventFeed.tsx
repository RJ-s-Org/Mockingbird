import { useEffect, useRef, useState } from "react";
import { olderThan, recentRequests, subscribeRequests, type LiveRequestView } from "../../api/sse";
import { Direction } from "./Direction";
import {
  TableScroll,
  emptyCellClass,
  tableClass,
  tdClass,
  thClass,
  theadClass,
  trClass,
} from "../../components/DataTable";
import { ResponseSource, StatusCode } from "../../components/http";
import { primaryButton, secondaryButton } from "../../components/ui";

/** The feed keeps at most this many rows, newest first. */
export const MAX_EVENTS = 200;

interface Row {
  key: number;
  record: LiveRequestView;
}

const nowrapCell = `${tdClass} whitespace-nowrap`;

/** The mock's event-channel messages as they are published and received, newest first. */
export function EventFeed({ ws, mock }: { ws: string; mock: string }) {
  const [rows, setRows] = useState<Row[]>([]);
  const [paused, setPaused] = useState(false);
  const [missed, setMissed] = useState(0);
  const [interrupted, setInterrupted] = useState(false);
  const pausedRef = useRef(false);
  const nextKey = useRef(0);

  const [history, setHistory] = useState<"loading" | "loaded">("loading");
  const feed = `${ws}/${mock}`;
  const [shownFeed, setShownFeed] = useState(feed);
  if (shownFeed !== feed) {
    setShownFeed(feed);
    setRows([]);
    setHistory("loading");
  }

  useEffect(() => {
    // Recent events first, then the live tail on top.
    let cancelled = false;
    recentRequests(ws, mock, { kind: "event" }).then((recent) => {
      if (cancelled) return;
      setRows((current) => {
        const older = olderThan(
          current.map((r) => r.record),
          recent,
        ).map((record, i) => ({ key: -1 - i, record }));
        return [...current, ...older].slice(0, MAX_EVENTS);
      });
      setHistory("loaded");
    });
    const close = subscribeRequests(
      ws,
      mock,
      { kind: "event" },
      (event) => {
        setInterrupted(false);
        if (event.kind === "lagged") {
          setMissed((n) => n + event.dropped);
          return;
        }
        if (pausedRef.current) {
          setMissed((n) => n + 1);
          return;
        }
        const key = nextKey.current;
        nextKey.current += 1;
        setRows((current) => [{ key, record: event.record }, ...current].slice(0, MAX_EVENTS));
      },
      () => setInterrupted(true),
    );
    return () => {
      cancelled = true;
      close();
    };
  }, [ws, mock]);

  const togglePause = () => {
    pausedRef.current = !paused;
    setPaused(!paused);
    setMissed(0);
  };

  return (
    <section className="space-y-3">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <h2 className="text-base font-semibold">Live events</h2>
        <button type="button" className={paused ? primaryButton : secondaryButton} onClick={togglePause}>
          {paused ? "Resume" : "Pause"}
        </button>
      </div>
      {history === "loaded" && rows.length > 0 ? (
        <p className="text-sm text-fg-muted">Showing the latest recorded events, then new ones as they arrive.</p>
      ) : null}
      <div aria-live="polite" className="space-y-1 text-sm">
        {paused ? (
          <p className="text-fg-muted">
            {missed} {missed === 1 ? "event" : "events"} arrived while paused.
          </p>
        ) : null}
        {interrupted ? <p className="text-warn-fg">The event feed connection was interrupted.</p> : null}
      </div>
      <TableScroll>
        <table aria-label="Event feed" className={tableClass}>
          <thead className={theadClass}>
            <tr>
              {["Time", "Channel", "Direction", "Status", "Source"].map((h) => (
                <th key={h} scope="col" className={thClass}>
                  {h}
                </th>
              ))}
            </tr>
          </thead>
          <tbody>
            {rows.length === 0 ? (
              <tr>
                <td colSpan={5} className={emptyCellClass}>
                  {history === "loading"
                    ? "Loading recent events…"
                    : "No events recorded yet. New ones appear here as they are published or received."}
                </td>
              </tr>
            ) : (
              rows.map(({ key, record }) => (
                <tr key={key} className={trClass}>
                  <td className={`${nowrapCell} tabular-nums text-fg-muted`}>
                    <time dateTime={record.at}>{new Date(record.at).toLocaleTimeString()}</time>
                  </td>
                  <td className={nowrapCell}>
                    <code>{record.operation_id ?? "—"}</code>
                  </td>
                  <td className={tdClass}>
                    <Direction value={record.method} />
                  </td>
                  <td className={tdClass}>
                    <StatusCode status={record.status} />
                  </td>
                  <td className={tdClass}>
                    <ResponseSource source={record.source} />
                  </td>
                </tr>
              ))
            )}
          </tbody>
        </table>
      </TableScroll>
    </section>
  );
}
