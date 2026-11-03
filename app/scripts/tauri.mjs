// Runs the Tauri CLI (the local devDependency) against crates/revenant-app, the Rust side of the app, with this
// folder as the interface. Usage: npm run tauri -- <command> [options]
import { spawnSync } from 'node:child_process';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const app = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const cli = path.join(app, 'node_modules', '@tauri-apps', 'cli', 'tauri.js');
const env = {
  ...process.env,
  TAURI_APP_PATH: path.join(app, '..', 'crates', 'revenant-app'),
  TAURI_FRONTEND_PATH: app,
};
const run = spawnSync(process.execPath, [cli, ...process.argv.slice(2)], { stdio: 'inherit', env });
process.exit(run.status ?? 1);
