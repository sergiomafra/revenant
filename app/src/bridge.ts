// The one door to the Rust side. Every failure arrives as `{ code }` (an engine or app error code, `snake_case`);
// the interface translates the code (src/i18n). Tests answer the commands with Tauri's IPC mock (src/test-utils.tsx).
import { invoke as tauriInvoke, type InvokeArgs } from '@tauri-apps/api/core';

export type EngineErrorView = { code: string };

function toErrorView(e: unknown): EngineErrorView {
  if (e && typeof e === 'object' && typeof (e as { code?: unknown }).code === 'string') {
    return { code: (e as { code: string }).code };
  }
  return { code: 'unknown' };
}

/** Calls a Rust command; rejects with an `EngineErrorView`. */
export async function invoke<T>(cmd: string, args?: InvokeArgs): Promise<T> {
  try {
    return await tauriInvoke<T>(cmd, args);
  } catch (e) {
    throw toErrorView(e);
  }
}
