import { useEffect, useRef, useState } from "react";
import { subscribeRequests, type LiveRequestView } from "../../api/sse";
import { primaryButton, secondaryButton } from "../../components/ConfirmDialog";
import { tableScroll } from "../../components/DataTable";

/** The feed keeps at most this many rows, newest first. */
export const MAX_EVENTS = 200;

interface Row {
  key: number;
  record: LiveRequestView;
}

const cell = "px-3 py-2 align-top";

/** The mock's event-channel messages as they are published and received, newest first. */
export function EventFeed({ ws, mock }: { ws: string; mock: string }) {
  const [rows, setRows] = useState<Row[]>([]);
  const [paused, setPaused] = useState(false);
  const [missed, setMissed] = useState(0);
  const [interrupted, setInterrupted] = useState(false);
  const pausedRef = useRef(false);
  const nextKey = useRef(0);

  useEffect(() => {
    return subscribeRequests(
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
  }, [ws, mock]);

  const togglePause = () => {
    pausedRef.current = !paused;
    setPaused(!paused);
    setMissed(0);
  };

  return (
    <section className="space-y-3">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <h2 className="text-lg font-semibold">Live events</h2>
        <button type="button" className={paused ? primaryButton : secondaryButton} onClick={togglePause}>
          {paused ? "Resume" : "Pause"}
        </button>
      </div>
      <div aria-live="polite" className="space-y-1 text-sm">
        {paused ? (
          <p className="text-slate-700">
            {missed} {missed === 1 ? "event" : "events"} arrived while paused.
          </p>
        ) : null}
        {interrupted ? <p className="text-amber-900">The event feed connection was interrupted.</p> : null}
      </div>
      <div className={tableScroll}>
        <table aria-label="Event feed" className="w-full border-collapse text-left text-sm">
          <thead className="border-b border-slate-300 text-slate-700">
            <tr>
              {["Time", "Channel", "Direction", "Status", "Source"].map((h) => (
                <th key={h} scope="col" className="px-3 py-2 font-semibold">
                  {h}
                </th>
              ))}
            </tr>
          </thead>
          <tbody>
            {rows.length === 0 ? (
              <tr>
                <td colSpan={5} className="px-3 py-6 text-center text-slate-600">
                  Waiting for events…
                </td>
              </tr>
            ) : (
              rows.map(({ key, record }) => (
                <tr key={key} className="border-b border-slate-200">
                  <td className={cell}>
                    <time dateTime={record.at}>{new Date(record.at).toLocaleTimeString()}</time>
                  </td>
                  <td className={cell}>
                    <code>{record.operation_id ?? "—"}</code>
                  </td>
                  <td className={cell}>
                    <code>{record.method}</code>
                  </td>
                  <td className={cell}>{record.status}</td>
                  <td className={cell}>{record.source ?? "—"}</td>
                </tr>
              ))
            )}
          </tbody>
        </table>
      </div>
    </section>
  );
}
