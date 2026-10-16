import { describe, expect, it } from "vitest";
import type { Me } from "../auth/me";
import { can, type Permission, type Role } from "../auth/permissions";

function me(role: Role | null, sysadmin = false): Me {
  return {
    subject: "alice",
    display_name: "Alice",
    kind: "user",
    sysadmin,
    workspaces: role ? [{ name: "team", role }] : [],
  };
}

const permissions: Permission[] = ["read", "edit", "manageProfiles", "manageApiKeys", "manageWorkspace"];

// Mirrors the server matrix: Read viewer, Edit editor, ManageProfiles admin,
// ManageApiKeys admin, ManageWorkspace owner.
const matrix: Record<Role, Permission[]> = {
  viewer: ["read"],
  editor: ["read", "edit"],
  admin: ["read", "edit", "manageProfiles", "manageApiKeys"],
  owner: ["read", "edit", "manageProfiles", "manageApiKeys", "manageWorkspace"],
};

describe("can", () => {
  it("permissions mirror the server matrix", () => {
    for (const role of Object.keys(matrix) as Role[]) {
      for (const p of permissions) {
        expect(can(me(role), "team", p), `${role} ${p}`).toBe(matrix[role].includes(p));
        // A role in one workspace grants nothing in another.
        expect(can(me(role), "other", p), `${role} ${p} elsewhere`).toBe(false);
      }
    }
    for (const p of permissions) {
      expect(can(me(null), "team", p)).toBe(false);
      expect(can(me(null, true), "team", p), `sysadmin ${p}`).toBe(true);
      expect(can(me(null, true), "missing", p), `sysadmin ${p}`).toBe(true);
    }
  });
});
