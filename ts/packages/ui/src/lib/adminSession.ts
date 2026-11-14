/**
 * The operator token the core accepts, kept for the life of the tab.
 *
 * It is deliberately not in localStorage: closing the tab should end the
 * session, and a token is not something that ought to outlive the browser.
 * Nothing here is a secret store — the token is typed by a human and only ever
 * compared by the core.
 */
const TOKEN_KEY = 'openether.admin.token';

export function readAdminToken(): string {
  try {
    return window.sessionStorage.getItem(TOKEN_KEY) ?? '';
  } catch {
    // A blocked storage quota must not break the dashboard; the operator can
    // still use the panel for this page load.
    return '';
  }
}

export function writeAdminToken(token: string): void {
  try {
    if (token) {
      window.sessionStorage.setItem(TOKEN_KEY, token);
    } else {
      window.sessionStorage.removeItem(TOKEN_KEY);
    }
  } catch {
    // Same as above: an unavailable sessionStorage is not fatal.
  }
}

/** The header the core's admin routes expect, when a token is present. */
export function adminHeaders(): Record<string, string> {
  const token = readAdminToken();
  return token ? { authorization: `Bearer ${token}` } : {};
}
