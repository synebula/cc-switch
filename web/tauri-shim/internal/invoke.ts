import { downloadText, pickFileText } from "./storage";

type InvokeArgs = Record<string, unknown> | undefined;

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

async function invokeRemote<T>(
  command: string,
  args?: InvokeArgs,
): Promise<T> {
  const backend = getBackendConfig();
  if (!backend) {
    throw new Error("[web] backend url not configured");
  }

  const invokeUrl = backend.token
    ? `${backend.url}/invoke?token=${encodeURIComponent(backend.token)}`
    : `${backend.url}/invoke`;

  const res = await fetch(invokeUrl, {
    method: "POST",
    headers: {
      "content-type": "application/json",
      ...(backend.token ? { authorization: `Bearer ${backend.token}` } : {}),
    },
    body: JSON.stringify({ command, args: args ?? {} }),
  });

  const json = (await res.json().catch(() => null)) as
    | { ok: true; data: T }
    | { ok: false; error: string }
    | null;

  if (!res.ok) {
    throw new Error(
      `[web] backend http ${res.status}: ${json && "error" in json ? json.error : "request failed"}`,
    );
  }
  if (!json) throw new Error("[web] backend returned invalid json");
  if (!json.ok) throw new Error(json.error || "[web] backend error");
  return json.data;
}

const filePool = new Map<string, { name: string; text: string; ts: number }>();

async function openFileDialogWeb(): Promise<string | null> {
  const file = await pickFileText(".json,.sql,application/json,text/plain");
  if (!file) return null;
  const id = crypto.randomUUID?.() ?? `id_${Math.random().toString(16).slice(2)}_${Date.now()}`;
  filePool.set(id, { name: file.name, text: file.text, ts: Date.now() });
  return `__webfile__:${id}`;
}

async function saveFileDialogWeb(defaultName: string): Promise<string | null> {
  // Browser has no real "path". We keep the existing UI flow by returning a token.
  const name = (defaultName || "cc-switch-export.json").trim() || "cc-switch-export.json";
  return `__websave__:${name}`;
}

function resolveWebFile(token: string): { name: string; text: string } | null {
  if (!token.startsWith("__webfile__:")) return null;
  const id = token.slice("__webfile__:".length);
  const rec = filePool.get(id);
  return rec ? { name: rec.name, text: rec.text } : null;
}

function resolveWebSave(token: string): { name: string } | null {
  if (!token.startsWith("__websave__:")) return null;
  const name = token.slice("__websave__:".length) || "cc-switch-export.json";
  return { name };
}

export async function invokeBridge<T = unknown>(
  command: string,
  args?: InvokeArgs,
): Promise<T> {
  const backend = getBackendConfig();

  if (!backend) {
    throw new Error(
      "[web] missing VITE_CC_SWITCH_BACKEND_URL (web mode requires a server backend)",
    );
  }

  switch (command) {
    case "open_file_dialog":
      return (await openFileDialogWeb()) as T;
    case "save_file_dialog": {
      const defaultName = String(
        (args as any)?.defaultName ?? "cc-switch-export.json",
      );
      return (await saveFileDialogWeb(defaultName)) as T;
    }
    case "open_external": {
      const url = String((args as any)?.url ?? "");
      window.open(url, "_blank", "noopener,noreferrer");
      return undefined as T;
    }
    case "export_config_to_file": {
      const filePath = String((args as any)?.filePath ?? "");
      const resolved = resolveWebSave(filePath) ?? {
        name: "cc-switch-export.json",
      };
      const snapshot = await invokeRemote<any>("get_snapshot", {});

      const wantsSql =
        String(resolved.name).toLowerCase().endsWith(".sql") ||
        snapshot?.format === "sql";
      const outName = wantsSql
        ? String(resolved.name).toLowerCase().endsWith(".sql")
          ? resolved.name
          : `${resolved.name}.sql`
        : resolved.name;

      if (wantsSql && typeof snapshot?.sql === "string") {
        downloadText(outName, snapshot.sql);
      } else {
        downloadText(outName, JSON.stringify(snapshot, null, 2));
      }
      return {
        success: true,
        message: "exported",
        filePath: outName,
      } as T;
    }
    case "import_config_from_file": {
      const filePath = String((args as any)?.filePath ?? "");
      const file = resolveWebFile(filePath);
      if (!file) {
        return { success: false, message: "Invalid web file token" } as T;
      }
      try {
        const text = String(file.text ?? "");
        const isSqlName = String(file.name).toLowerCase().endsWith(".sql");

        let snapshot: any;
        if (!isSqlName) {
          try {
            snapshot = JSON.parse(text);
          } catch {
            snapshot = null;
          }
        }

        const sql =
          typeof snapshot?.sql === "string"
            ? snapshot.sql
            : typeof snapshot?.format === "string" &&
                snapshot.format.toLowerCase() === "sql" &&
                typeof snapshot.sql === "string"
              ? snapshot.sql
              : isSqlName || !snapshot
                ? text
                : null;

        if (typeof sql !== "string" || !sql.trim()) {
          return {
            success: false,
            message:
              "Invalid backup file: expected CC Switch snapshot JSON or a .sql backup",
          } as T;
        }

        const result = await invokeRemote<any>("apply_snapshot", {
          snapshot: { sql },
        });
        if (result?.success === false) {
          return {
            success: false,
            message: result?.message ?? "apply failed",
          } as T;
        }
        return { success: true, message: "imported", backupId: null } as T;
      } catch (e: any) {
        return { success: false, message: e?.message ?? "Invalid snapshot file" } as T;
      }
    }
    default:
      return await invokeRemote<T>(command, args);
  }
}
