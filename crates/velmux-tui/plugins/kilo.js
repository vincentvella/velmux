// velmux: tells velmux which Kilo session this pane is in, so it's resumed
// after a restart. Installed by `velmux skill install`, which rewrites it;
// `velmux skill uninstall` removes it. Adapted from herdr (Apache-2.0).
import { spawn } from "node:child_process";

const VELMUX = __VELMUX__;
let reported;

function report(id) {
  if (!id || id === reported) return;
  reported = id;
  try {
    const child = spawn(VELMUX, ["agent-hook", "kilo"], { stdio: ["pipe", "ignore", "ignore"] });
    child.on("error", () => {});
    child.stdin.on("error", () => {});
    child.stdin.end(JSON.stringify({ session_id: id }));
  } catch {}
}

export const VelmuxSessionPlugin = async () => {
  if (!process.env.VELMUX_PANE) return {};
  return {
    event: async ({ event }) => {
      if (event?.type !== "session.created" && event?.type !== "session.updated") return;
      const properties = event.properties ?? {};
      // A subagent's session isn't the one to resume.
      if (properties.info?.parentID) return;
      report(properties.sessionID ?? properties.info?.id);
    },
  };
};
