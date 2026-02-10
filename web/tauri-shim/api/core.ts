import { invokeBridge } from "../internal/invoke";

export async function invoke<T = unknown>(
  command: string,
  args?: Record<string, unknown>,
): Promise<T> {
  return await invokeBridge<T>(command, args);
}
