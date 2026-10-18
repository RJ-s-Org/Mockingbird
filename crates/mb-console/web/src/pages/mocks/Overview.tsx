import { useNavigate } from "@tanstack/react-router";
import { useState, type FormEvent } from "react";
import {
  useDeleteMock,
  useMock,
  usePatchMock,
  useProfiles,
  type DataPlaneAuthMode,
  type Mock,
  type PatchMock,
} from "../../api/queries";
import { useMe } from "../../auth/me";
import { can } from "../../auth/permissions";
import { ConfirmDialog, dangerButton, inputClass, primaryButton } from "../../components/ConfirmDialog";
import { ProblemAlert } from "../../components/ProblemAlert";
import { ChaosLimits } from "./ChaosLimits";
import { ProfileSelect } from "./CreateMockDialog";

const MODES: { value: DataPlaneAuthMode; label: string }[] = [
  { value: "none", label: "none (open)" },
  { value: "api_key", label: "api_key (workspace data-plane key)" },
  { value: "passthrough_validate", label: "passthrough_validate (credentials present)" },
];

/** Why a mock cannot be edited here, if it cannot. */
export function readOnlyReason(mock: Mock, canEdit: boolean): string | undefined {
  if (mock.source === "file") return "Managed by config file";
  if (!canEdit) return "Read-only: editing a mock needs the editor role.";
  return undefined;
}

/** A mock's settings, then its chaos and limits; each Save sends its own patch. */
export function Overview({ ws, mock }: { ws: string; mock: string }) {
  const m = useMock(ws, mock);
  const me = useMe();
  const [savedAt, setSavedAt] = useState<string>();
  const [chaosSavedAt, setChaosSavedAt] = useState<string>();
  if (!m.data) return m.isPending ? <p className="text-slate-700 dark:text-slate-300">Loading…</p> : null;
  const canEdit = me.data ? can(me.data, ws, "edit") : false;
  // Re-seed each form when the stored values it edits change, and only
  // then: saving one form must not discard unsaved edits in the other.
  return (
    <div className="space-y-8">
      <OverviewForm
        key={settingsKey(m.data)}
        ws={ws}
        mock={m.data}
        saved={savedAt === m.data.updated_at}
        onSaved={(updated) => setSavedAt(updated.updated_at)}
      />
      <ChaosLimits
        key={chaosLimitsKey(m.data)}
        ws={ws}
        mock={m.data}
        canEdit={canEdit}
        reason={readOnlyReason(m.data, canEdit)}
        saved={chaosSavedAt === m.data.updated_at}
        onSaved={(updated) => setChaosSavedAt(updated.updated_at)}
      />
    </div>
  );
}

/** The stored values `OverviewForm` edits, serialized. */
function settingsKey(mock: Mock): string {
  return JSON.stringify([
    mock.route_prefix,
    mock.persona ?? null,
    mock.llm_profile ?? null,
    mock.header_overrides,
    mock.data_plane_auth.mode,
    mock.data_plane_auth.header,
  ]);
}

/** The stored values `ChaosLimits` edits, serialized. */
function chaosLimitsKey(mock: Mock): string {
  return `chaos-${JSON.stringify([mock.chaos ?? null, mock.limits ?? null])}`;
}

function OverviewForm({
  ws,
  mock,
  saved,
  onSaved,
}: {
  ws: string;
  mock: Mock;
  saved: boolean;
  onSaved: (updated: Mock) => void;
}) {
  const me = useMe();
  const profiles = useProfiles(ws);
  const patch = usePatchMock(ws, mock.name);
  const remove = useDeleteMock(ws);
  const navigate = useNavigate();
  const [confirming, setConfirming] = useState(false);

  const [prefix, setPrefix] = useState(mock.route_prefix);
  const [persona, setPersona] = useState(mock.persona ?? "");
  const [profile, setProfile] = useState(mock.llm_profile ?? "");
  const [headerOverrides, setHeaderOverrides] = useState(mock.header_overrides);
  const [mode, setMode] = useState<DataPlaneAuthMode>(mock.data_plane_auth.mode);
  const [header, setHeader] = useState(mock.data_plane_auth.header);

  const canEdit = me.data ? can(me.data, ws, "edit") : false;
  const reason = readOnlyReason(mock, canEdit);

  const changes: PatchMock = {};
  if (prefix !== mock.route_prefix) changes.route_prefix = prefix;
  if (persona !== (mock.persona ?? "")) changes.persona = persona === "" ? null : persona;
  if (profile !== (mock.llm_profile ?? "")) changes.llm_profile = profile || null;
  if (headerOverrides !== mock.header_overrides) changes.header_overrides = headerOverrides;
  if (mode !== mock.data_plane_auth.mode || header !== mock.data_plane_auth.header) {
    changes.data_plane_auth = { mode, header };
  }
  const dirty = Object.keys(changes).length > 0;

  const submit = (e: FormEvent) => {
    e.preventDefault();
    patch.mutate(changes, { onSuccess: onSaved });
  };

  const del = () =>
    remove.mutate(mock.name, {
      onSuccess: () => void navigate({ to: "/w/$ws/mocks", params: { ws } }),
    });

  return (
    <form onSubmit={submit} className="max-w-2xl space-y-4">
      {reason ? (
        <p className="rounded-md border border-amber-300 dark:border-amber-700 bg-amber-50 dark:bg-amber-950/60 p-3 text-sm text-amber-900 dark:text-amber-200">{reason}</p>
      ) : null}
      {saved && !dirty ? (
        <p role="status" className="text-sm text-green-800 dark:text-green-300">
          Saved.
        </p>
      ) : null}
      <fieldset disabled={reason !== undefined} className="space-y-4">
        <legend className="sr-only">Settings of {mock.name}</legend>
        <div className="space-y-1">
          <label htmlFor="mock-prefix" className="block text-sm font-medium">
            Route prefix
          </label>
          <input
            id="mock-prefix"
            className={inputClass}
            value={prefix}
            required
            onChange={(e) => setPrefix(e.target.value)}
          />
        </div>
        <div className="space-y-1">
          <label htmlFor="mock-persona" className="block text-sm font-medium">
            Persona
          </label>
          <textarea
            id="mock-persona"
            className={inputClass}
            rows={4}
            value={persona}
            onChange={(e) => setPersona(e.target.value)}
          />
        </div>
        <ProfileSelect id="mock-profile" value={profile} onChange={setProfile} profiles={profiles.data} />
        <div className="flex items-center gap-2">
          <input
            id="mock-overrides"
            type="checkbox"
            checked={headerOverrides}
            onChange={(e) => setHeaderOverrides(e.target.checked)}
          />
          <label htmlFor="mock-overrides" className="text-sm font-medium">
            Header overrides
          </label>
        </div>
        <div className="grid gap-4 sm:grid-cols-2">
          <div className="space-y-1">
            <label htmlFor="mock-dp-mode" className="block text-sm font-medium">
              Data-plane auth
            </label>
            <select
              id="mock-dp-mode"
              className={inputClass}
              value={mode}
              onChange={(e) => setMode(e.target.value as DataPlaneAuthMode)}
            >
              {MODES.map((o) => (
                <option key={o.value} value={o.value}>
                  {o.label}
                </option>
              ))}
            </select>
          </div>
          <div className="space-y-1">
            <label htmlFor="mock-dp-header" className="block text-sm font-medium">
              Key header
            </label>
            <input
              id="mock-dp-header"
              className={inputClass}
              value={header}
              required
              onChange={(e) => setHeader(e.target.value)}
              aria-describedby="mock-dp-header-hint"
            />
            <p id="mock-dp-header-hint" className="text-xs text-slate-600 dark:text-slate-400">
              Carries the key in api_key mode (or send <code>?api_key=</code>).
            </p>
          </div>
        </div>
      </fieldset>
      <ProblemAlert error={patch.error ?? remove.error} />
      {canEdit ? (
        <div className="flex justify-between gap-2">
          <button type="submit" className={primaryButton} disabled={reason !== undefined || !dirty || patch.isPending}>
            Save
          </button>
          <button
            type="button"
            className={dangerButton}
            disabled={reason !== undefined}
            onClick={() => setConfirming(true)}
          >
            Delete mock
          </button>
        </div>
      ) : null}
      {confirming ? (
        <ConfirmDialog
          title={`Delete mock ${mock.name}?`}
          message="Its contract versions, scenarios, sessions and request log are deleted too."
          confirmLabel="Delete"
          busy={remove.isPending}
          onConfirm={del}
          onCancel={() => setConfirming(false)}
        />
      ) : null}
    </form>
  );
}
