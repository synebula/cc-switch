function getBackendConfig(): { url: string; token: string } | null {
  const env = (import.meta as any)?.env ?? {};
  const url = String(env.VITE_CC_SWITCH_BACKEND_URL ?? "").trim();
  const token = String(env.VITE_CC_SWITCH_BACKEND_TOKEN ?? "").trim();
  const fallback =
    typeof window !== "undefined" ? String(window.location.origin) : "";
  const base = (url || fallback).trim();
  if (!base) return null;
  return { url: base.replace(/\/+$/g, ""), token };
}

export type UnlistenFn = () => void;

export async function listen<T = unknown>(
  event: string,
  handler: (event: { event: string; id: number; payload: T }) => void,
): Promise<UnlistenFn> {
  const backend = getBackendConfig();
  if (!backend) {
    return () => {};
  }

  // Server-sent events (SSE) from web backend.
  // Note: Authorization headers are not supported by EventSource in browsers.
  // If you need auth, put the UI + backend behind same origin, or use a WS transport.
  const eventsUrl = backend.token
    ? `${backend.url}/events?token=${encodeURIComponent(backend.token)}`
    : `${backend.url}/events`;
  const es = new EventSource(eventsUrl);
  const onEvt = (msg: MessageEvent) => {
    try {
      const payload = JSON.parse(String(msg.data ?? "null")) as T;
      handler({ event, id: 0, payload });
    } catch (e) {
      console.error("[web][event] failed to parse event payload", e);
    }
  };

  es.addEventListener(event, onEvt as any);
  es.onerror = (e) => {
    // Keep silent to avoid noisy logs when backend disconnects/restarts.
    // Users will see functional degradation (no auto-refresh) only.
    void e;
  };

  return () => {
    es.removeEventListener(event, onEvt as any);
    es.close();
  };
}
