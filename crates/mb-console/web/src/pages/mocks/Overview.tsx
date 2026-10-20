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
import { ConfirmDialog } from "../../components/ConfirmDialog";
import { ProblemAlert } from "../../components/ProblemAlert";
import { ChaosLimits } from "./ChaosLimits";
import { ProfileSelect } from "./CreateMockDialog";
import { Card, Checkbox, Field, Loading, Notice, SavedNote, dangerButton, inputClass, primaryButton } from "../../components/ui";

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
  if (!m.data) return m.isPending ? <Loading /> : null;
  const canEdit = me.data ? can(me.data, ws, "edit") : false;
  // Re-seed each form when the stored values it edits change, and only
  // then: saving one form must not discard unsaved edits in the other.
  return (
    <div className="max-w-3xl space-y-6">
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
      {canEdit ? <DeleteMock ws={ws} mock={m.data} disabled={readOnlyReason(m.data, canEdit) !== undefined} /> : null}
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

  return (
    <Card title="Settings" description="How the mock is mounted, what it sounds like and who may call it.">
      <form onSubmit={submit} className="space-y-5">
        {reason ? <Notice tone="warn">{reason}</Notice> : null}
        <fieldset disabled={reason !== undefined} className="space-y-5">
          <legend className="sr-only">Settings of {mock.name}</legend>
          <Field id="mock-prefix" label="Route prefix">
            <input
              id="mock-prefix"
              className={inputClass}
              value={prefix}
              required
              onChange={(e) => setPrefix(e.target.value)}
            />
          </Field>
          <Field id="mock-persona" label="Persona">
            <textarea
              id="mock-persona"
              className={inputClass}
              rows={4}
              value={persona}
              onChange={(e) => setPersona(e.target.value)}
            />
          </Field>
          <ProfileSelect id="mock-profile" value={profile} onChange={setProfile} profiles={profiles.data} />
          <Checkbox
            id="mock-overrides"
            label="Header overrides"
            checked={headerOverrides}
            onChange={setHeaderOverrides}
          />
          <div className="grid gap-5 sm:grid-cols-2">
            <Field id="mock-dp-mode" label="Data-plane auth">
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
            </Field>
            <Field
              id="mock-dp-header"
              label="Key header"
              hint={
                <>
                  Carries the key in api_key mode (or send <code>?api_key=</code>).
                </>
              }
            >
              <input
                id="mock-dp-header"
                className={inputClass}
                value={header}
                required
                onChange={(e) => setHeader(e.target.value)}
                aria-describedby="mock-dp-header-hint"
              />
            </Field>
          </div>
        </fieldset>
        <ProblemAlert error={patch.error} focus />
        {canEdit ? (
          <div className="flex flex-wrap items-center gap-3 border-t border-line pt-4">
            <button
              type="submit"
              className={primaryButton}
              disabled={reason !== undefined || !dirty || patch.isPending}
            >
              Save
            </button>
            {saved && !dirty ? <SavedNote /> : null}
          </div>
        ) : null}
      </form>
    </Card>
  );
}

/** Deletes the mock after confirmation, then goes back to the list. */
function DeleteMock({ ws, mock, disabled }: { ws: string; mock: Mock; disabled: boolean }) {
  const remove = useDeleteMock(ws);
  const navigate = useNavigate();
  const [confirming, setConfirming] = useState(false);

  const del = () =>
    remove.mutate(mock.name, {
      onSuccess: () => void navigate({ to: "/w/$ws/mocks", params: { ws } }),
    });

  return (
    <Card title="Delete this mock" description="Its contract versions, scenarios, sessions and request log go with it.">
      <div className="space-y-4">
        <ProblemAlert error={remove.error} focus />
        <button type="button" className={dangerButton} disabled={disabled} onClick={() => setConfirming(true)}>
          Delete mock
        </button>
      </div>
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
    </Card>
  );
}
