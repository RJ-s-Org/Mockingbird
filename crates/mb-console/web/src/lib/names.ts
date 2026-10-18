/**
 * The server's rule for mock, workspace and profile names
 * (`^[a-z0-9][a-z0-9-]{0,62}$`, see `mb_store::validate_mock_name`).
 */
export const NAME_PATTERN = "[a-z0-9][a-z0-9\\-]{0,62}";

const NAME_RE = /^[a-z0-9][a-z0-9-]{0,62}$/;

/** The rule in words, shown next to name fields. */
export const NAME_RULE = "Up to 63 lowercase letters, digits and dashes, starting with a letter or digit.";

/** Whether `name` satisfies the server's name rule. */
export function isValidName(name: string): boolean {
  return NAME_RE.test(name);
}
