// Token storage. The single admin token is the only credential the
// UI holds. We store it in sessionStorage by default (cleared when
// the tab closes); operators can opt into localStorage ("remember
// me") so the token survives reloads. The single super-user token
// model means we keep the surface minimal and never log the value.

const KEY = "tiygate.admin.token";

export function getToken(): string | null {
  return (
    window.sessionStorage.getItem(KEY) ?? window.localStorage.getItem(KEY)
  );
}

export function setToken(token: string, remember: boolean): void {
  if (remember) {
    window.localStorage.setItem(KEY, token);
    window.sessionStorage.removeItem(KEY);
  } else {
    window.sessionStorage.setItem(KEY, token);
    window.localStorage.removeItem(KEY);
  }
}

export function clearToken(): void {
  window.sessionStorage.removeItem(KEY);
  window.localStorage.removeItem(KEY);
}
