export async function homeDir(): Promise<string> {
  // Browser has no home directory; return a virtual path for display only.
  return "~";
}

export async function join(...parts: string[]): Promise<string> {
  const cleaned = parts
    .filter((p) => typeof p === "string")
    .map((p) => p.replace(/\\/g, "/"))
    .map((p) => p.replace(/\/+$/g, ""))
    .filter(Boolean);
  return cleaned.join("/").replace(/\/{2,}/g, "/");
}

