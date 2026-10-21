// velmux: tells velmux which __AGENT__ session this pane is in, so it's
// resumed after a restart. Installed by `velmux skill install`, which
// rewrites it; `velmux skill uninstall` removes it. Adapted from herdr
// (Apache-2.0).
// @ts-nocheck
import { spawn } from "node:child_process";

const VELMUX = __VELMUX__;
const AGENT = "__AGENT__";
let reported;
let root = false;

// The session file when there is one (it resumes by path), else its id.
function sessionRef(ctx) {
  try {
    const file = ctx?.sessionManager?.getSessionFile?.();
    if (typeof file === "string" && file.startsWith("/")) return file;
  } catch {}
  try {
    const id = ctx?.sessionManager?.getSessionId?.();
    if (typeof id === "string" && id) return id;
  } catch {}
  return undefined;
}

function report(ctx) {
  const ref = sessionRef(ctx);
  if (!ref || ref === reported) return;
  reported = ref;
  try {
    const child = spawn(VELMUX, ["agent-hook", AGENT], { stdio: ["pipe", "ignore", "ignore"] });
    child.on("error", () => {});
    child.stdin.on("error", () => {});
    child.stdin.end(JSON.stringify({ session_id: ref }));
  } catch {}
}

// The session on screen: pi's TUI (not its RPC, JSON or print modes), or
// omp's root session with a UI (not one nested in an omp shell).
function interactive(ctx) {
  if (AGENT === "omp") return process.env.OMPCODE !== "1" && ctx?.hasUI === true;
  return ctx?.mode === "tui";
}

export default function (pi) {
  if (!process.env.VELMUX_PANE) return;
  const start = (_event, ctx) => {
    if (interactive(ctx)) {
      root = true;
      report(ctx);
    }
  };
  pi.on("session_start", start);
  pi.on("session_switch", start);
  // The session file can appear only once the first turn starts.
  pi.on("agent_start", (_event, ctx) => {
    if (root) report(ctx);
  });
}
