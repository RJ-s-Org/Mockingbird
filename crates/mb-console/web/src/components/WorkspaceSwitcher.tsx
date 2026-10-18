import { useNavigate } from "@tanstack/react-router";
import { useState, type FormEvent } from "react";
import { rememberWorkspace, useCreateWorkspace, useWorkspaces } from "../api/queries";
import { Dialog, inputClass, primaryButton, secondaryButton } from "./ConfirmDialog";
import { ProblemAlert } from "./ProblemAlert";

/** Picks the current workspace; a sysadmin can also create one. */
export function WorkspaceSwitcher({
  current,
  missing = false,
  sysadmin,
}: {
  current?: string;
  /** `current` is not an existing workspace: it is shown as such, not as another one. */
  missing?: boolean;
  sysadmin: boolean;
}) {
  const workspaces = useWorkspaces();
  const navigate = useNavigate();
  const [creating, setCreating] = useState(false);

  const open = (ws: string) => {
    rememberWorkspace(ws);
    void navigate({ to: "/w/$ws/mocks", params: { ws } });
  };

  return (
    <div className="flex min-w-0 flex-wrap items-center gap-2">
      <label htmlFor="workspace-switcher" className="sr-only text-sm font-medium text-slate-200 sm:not-sr-only">
        Workspace
      </label>
      <select
        id="workspace-switcher"
        className="max-w-[11rem] min-w-0 rounded-md border border-slate-500 bg-slate-800 px-2 py-1 text-sm text-white sm:max-w-xs focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-white"
        value={current ?? ""}
        onChange={(e) => open(e.target.value)}
        disabled={!workspaces.data}
      >
        {current === undefined ? <option value="">Choose…</option> : null}
        {!workspaces.data && current !== undefined ? <option value={current}>{current}</option> : null}
        {workspaces.data && missing && current !== undefined ? (
          <option value={current} disabled>
            {`${current} (not found)`}
          </option>
        ) : null}
        {(workspaces.data ?? []).map((w) => (
          <option key={w.name} value={w.name}>
            {w.display_name}
          </option>
        ))}
      </select>
      {sysadmin ? (
        <button
          type="button"
          className="rounded-md border border-slate-400 px-2 py-1 text-sm whitespace-nowrap text-white hover:bg-slate-700 focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-white"
          onClick={() => setCreating(true)}
        >
          New workspace
        </button>
      ) : null}
      {creating ? (
        <NewWorkspaceDialog
          onClose={() => setCreating(false)}
          onCreated={(ws) => {
            setCreating(false);
            open(ws);
          }}
        />
      ) : null}
    </div>
  );
}

function NewWorkspaceDialog({ onClose, onCreated }: { onClose: () => void; onCreated: (ws: string) => void }) {
  const create = useCreateWorkspace();
  const [name, setName] = useState("");
  const [displayName, setDisplayName] = useState("");

  const submit = (e: FormEvent) => {
    e.preventDefault();
    create.mutate(
      { name, display_name: displayName.trim() || null },
      { onSuccess: (w) => onCreated(w.name) },
    );
  };

  return (
    <Dialog title="New workspace" onClose={onClose}>
      <form onSubmit={submit} className="space-y-4">
        <div className="space-y-1">
          <label htmlFor="new-workspace-name" className="block text-sm font-medium">
            Name
          </label>
          <input
            id="new-workspace-name"
            className={inputClass}
            value={name}
            onChange={(e) => setName(e.target.value)}
            required
            pattern="[a-z0-9][a-z0-9\-]{0,62}"
            aria-describedby="new-workspace-name-hint"
          />
          <p id="new-workspace-name-hint" className="text-xs text-slate-600">
            Lowercase letters, digits and dashes.
          </p>
        </div>
        <div className="space-y-1">
          <label htmlFor="new-workspace-display" className="block text-sm font-medium">
            Display name
          </label>
          <input
            id="new-workspace-display"
            className={inputClass}
            value={displayName}
            onChange={(e) => setDisplayName(e.target.value)}
          />
        </div>
        <ProblemAlert error={create.error} />
        <div className="flex justify-end gap-2">
          <button type="button" className={secondaryButton} onClick={onClose}>
            Cancel
          </button>
          <button type="submit" className={primaryButton} disabled={create.isPending}>
            Create
          </button>
        </div>
      </form>
    </Dialog>
  );
}
