// Test helpers: a fake engine behind the bridge (Tauri's IPC mock), so no test reaches Rust or a disk.
import { mockIPC } from '@tauri-apps/api/mocks';

export type Call = { cmd: string; args: unknown };
type Handler = (args: unknown) => unknown;

/** Answers each command with its handler (an unknown command rejects with `{ code: 'unknown' }`); returns the calls. */
export function fakeBackend(handlers: Record<string, Handler>): Call[] {
  const calls: Call[] = [];
  mockIPC((cmd, args) => {
    calls.push({ cmd, args });
    const h = handlers[cmd];
    if (!h) throw { code: 'unknown' };
    return h(args);
  });
  return calls;
}
