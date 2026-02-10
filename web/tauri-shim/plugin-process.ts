export async function relaunch(): Promise<void> {
  // Web mode: no relaunch. Best effort reload.
  try {
    window.location.reload();
  } catch {
    // ignore
  }
}

export async function exit(_code?: number): Promise<void> {
  // Web mode: cannot close tab programmatically in most browsers.
  return;
}

