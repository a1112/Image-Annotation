import { spawn } from "node:child_process";
import process from "node:process";

const processes = [];

function start(name, command, args) {
  const child = spawn(command, args, {
    cwd: process.cwd(),
    shell: process.platform === "win32",
    stdio: "inherit",
  });
  processes.push(child);
  child.on("exit", (code, signal) => {
    if (shuttingDown) return;
    console.error(`${name} exited`, { code, signal });
    shutdown(code ?? 1);
  });
}

let shuttingDown = false;

function shutdown(code = 0) {
  shuttingDown = true;
  for (const child of processes) {
    if (!child.killed) child.kill();
  }
  process.exit(code);
}

process.on("SIGINT", () => shutdown(0));
process.on("SIGTERM", () => shutdown(0));

start("rust-backend", "cargo", [
  "run",
  "--manifest-path",
  "src-tauri/Cargo.toml",
  "--example",
  "image-annotation-backend",
]);
start("vite", "vite", ["--host", "127.0.0.1", "--port", "1440"]);
