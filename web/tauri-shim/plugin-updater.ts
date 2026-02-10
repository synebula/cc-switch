export type Update = {
  version?: string;
  notes?: string;
  date?: string;
  downloadAndInstall?: (cb?: any) => Promise<void>;
  download?: () => Promise<void>;
  install?: () => Promise<void>;
};

export async function check(_opts?: any): Promise<Update | null> {
  // Web mode: no native updater. Return null so UI shows "up to date".
  return null;
}

