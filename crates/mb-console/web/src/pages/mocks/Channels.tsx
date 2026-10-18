import { useId, useState } from "react";
import {
  summarizeTrigger,
  useBrokers,
  useChannels,
  useEventSettings,
  usePutChannel,
  usePutEventSettings,
  type ChannelActivity,
  type ChannelView,
} from "../../api/channels";
import { useMock } from "../../api/queries";
import { useMe } from "../../auth/me";
import { can } from "../../auth/permissions";
import { inputClass, secondaryButton } from "../../components/ConfirmDialog";
import { DataTable, columnHelper } from "../../components/DataTable";
import { ProblemAlert } from "../../components/ProblemAlert";
import { ChannelTriggersEditor } from "./ChannelTriggersEditor";
import { Direction } from "./Direction";
import { EventFeed } from "./EventFeed";
import { PublishDialog } from "./PublishDialog";

const col = columnHelper<ChannelView>();

const FILE_MANAGED = "Managed by config file";
const NEEDS_EDITOR = "Read-only: changing channels needs the editor role.";

/** An on/off switch. */
function Switch({
  checked,
  label,
  disabled,
  title,
  onChange,
}: {
  checked: boolean;
  label: string;
  disabled: boolean;
  title?: string;
  onChange: (checked: boolean) => void;
}) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={checked}
      aria-label={label}
      title={title}
      disabled={disabled}
      onClick={() => onChange(!checked)}
      className={`relative inline-flex h-6 w-11 shrink-0 items-center rounded-full border-2 border-transparent focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-indigo-700 disabled:opacity-60 ${
        checked ? "bg-indigo-700" : "bg-slate-400"
      }`}
    >
      <span
        aria-hidden="true"
        className={`inline-block h-5 w-5 rounded-full bg-white shadow transition-transform ${
          checked ? "translate-x-5" : "translate-x-0"
        }`}
      />
    </button>
  );
}

/** Published/received counters and the last error, labelled in text (not only colour). */
function Activity({ activity }: { activity: ChannelActivity | null | undefined }) {
  if (!activity) return <span className="text-slate-600">Not running</span>;
  return (
    <div className="space-y-1">
      <div className="flex flex-wrap gap-x-3">
        <span>{`${activity.published} published`}</span>
        <span>{`${activity.received} received`}</span>
      </div>
      {activity.last_error ? (
        <p className="text-red-800">
          <span className="font-semibold">Last error:</span> <span>{activity.last_error}</span>
        </p>
      ) : null}
    </div>
  );
}

/** The mock's broker: automatic, or one of the configured brokers. */
function BrokerSelector({ ws, mock, readOnly }: { ws: string; mock: string; readOnly: string | undefined }) {
  const id = useId();
  const brokers = useBrokers();
  const settings = useEventSettings(ws, mock);
  const put = usePutEventSettings(ws, mock);
  const reason = settings.data?.source === "file" ? FILE_MANAGED : readOnly;
  const current = settings.data?.broker ?? "";
  // Keep a saved broker that is no longer configured, so the selection is honest.
  const names = brokers.data?.map((b) => b.name) ?? [];
  return (
    <div className="space-y-1">
      <label htmlFor={id} className="block text-sm font-medium">
        Broker
      </label>
      <select
        id={id}
        className={`${inputClass} max-w-sm`}
        value={current}
        disabled={reason !== undefined || settings.data === undefined || put.isPending}
        title={reason}
        onChange={(e) => put.mutate({ broker: e.target.value === "" ? null : e.target.value })}
      >
        <option value="">Automatic</option>
        {brokers.data?.map((b) => (
          <option key={b.name} value={b.name}>
            {`${b.name} (${b.kind}, ${b.status})`}
          </option>
        ))}
        {current !== "" && !names.includes(current) && brokers.data ? (
          <option value={current}>{`${current} (not configured)`}</option>
        ) : null}
      </select>
      <ProblemAlert error={settings.error ?? brokers.error ?? put.error} />
    </div>
  );
}

/** A mock's event channels: settings, triggers, manual publishing and the live event feed. */
export function Channels({ ws, mock }: { ws: string; mock: string }) {
  const me = useMe();
  const m = useMock(ws, mock);
  const channels = useChannels(ws, mock);
  const put = usePutChannel(ws, mock);
  const [editing, setEditing] = useState<ChannelView>();
  const [publishing, setPublishing] = useState<string>();

  const canEdit = me.data ? can(me.data, ws, "edit") : false;
  const readOnly = m.data?.source === "file" ? FILE_MANAGED : canEdit ? undefined : NEEDS_EDITOR;
  /** Why `c`'s settings cannot be changed, if they cannot. */
  const lockedReason = (c: ChannelView) => readOnly ?? (c.settings_source === "file" ? FILE_MANAGED : undefined);

  const columns = [
    col.accessor("id", {
      header: "Channel",
      cell: ({ row }) => (
        <div>
          <code className="font-semibold">{row.original.id}</code>
          {row.original.title ? <p className="text-slate-700">{row.original.title}</p> : null}
        </div>
      ),
    }),
    col.accessor("direction", { header: "Direction", cell: (info) => <Direction value={info.getValue()} /> }),
    col.accessor("protocol", { header: "Protocol" }),
    col.accessor("address", { header: "Address", cell: (info) => <code className="break-all">{info.getValue()}</code> }),
    col.accessor("enabled", {
      header: "Enabled",
      cell: ({ row }) => {
        const c = row.original;
        const reason = lockedReason(c);
        return (
          <Switch
            checked={c.enabled}
            label={`Enabled ${c.id}`}
            disabled={reason !== undefined || put.isPending}
            title={reason}
            onChange={(enabled) =>
              // Keep the channel's triggers: the contract's (null) unless overridden.
              put.mutate({ id: c.id, enabled, triggers: c.settings_source === "contract" ? null : c.triggers })
            }
          />
        );
      },
    }),
    col.accessor("triggers", {
      header: "Triggers",
      cell: (info) => {
        const triggers = info.getValue();
        if (triggers.length === 0) return <span className="text-slate-600">none</span>;
        return (
          <ul className="space-y-0.5">
            {triggers.map((t, i) => (
              <li key={i}>{summarizeTrigger(t)}</li>
            ))}
          </ul>
        );
      },
    }),
    col.accessor("activity", { header: "Activity", cell: (info) => <Activity activity={info.getValue()} /> }),
    col.display({
      id: "actions",
      header: "Actions",
      cell: ({ row }) => {
        const c = row.original;
        if (c.direction !== "publish") return null;
        const reason = lockedReason(c);
        return (
          <div className="flex flex-wrap gap-2">
            <button
              type="button"
              className={secondaryButton}
              aria-label={`Edit triggers of ${c.id}`}
              disabled={reason !== undefined}
              title={reason}
              onClick={() => setEditing(c)}
            >
              Edit triggers
            </button>
            <button
              type="button"
              className={secondaryButton}
              aria-label={`Publish now on ${c.id}`}
              disabled={readOnly !== undefined}
              title={readOnly}
              onClick={() => setPublishing(c.id)}
            >
              Publish now
            </button>
          </div>
        );
      },
    }),
  ];

  return (
    <div className="space-y-6">
      {readOnly ? (
        <p className="rounded-md border border-amber-300 bg-amber-50 p-3 text-sm text-amber-900">{readOnly}</p>
      ) : null}
      <BrokerSelector ws={ws} mock={mock} readOnly={readOnly} />
      <ProblemAlert error={channels.error ?? put.error} />
      {channels.data ? (
        channels.data.length === 0 ? (
          <p className="text-slate-700">This mock&apos;s active contract declares no channels.</p>
        ) : (
          <DataTable caption="Channels" columns={columns} data={channels.data} />
        )
      ) : channels.isPending ? (
        <p className="text-slate-700">Loading…</p>
      ) : null}
      <EventFeed ws={ws} mock={mock} />
      {editing ? (
        <ChannelTriggersEditor ws={ws} mock={mock} channel={editing} onClose={() => setEditing(undefined)} />
      ) : null}
      {publishing !== undefined ? (
        <PublishDialog ws={ws} mock={mock} channel={publishing} onClose={() => setPublishing(undefined)} />
      ) : null}
    </div>
  );
}
