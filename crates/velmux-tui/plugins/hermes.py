"""velmux: tells velmux which Hermes session this pane is in, so it's resumed
after a restart. Installed by `velmux skill install`, which rewrites it;
`velmux skill uninstall` removes it. Adapted from herdr (Apache-2.0)."""

from __future__ import annotations

import json
import os
import subprocess

_VELMUX = __VELMUX__
_INTERACTIVE = {"cli", "tui", "desktop", "acp"}
_reported = None


def _report(**kwargs) -> None:
    global _reported
    if not os.environ.get("VELMUX_PANE") or kwargs.get("platform") not in _INTERACTIVE:
        return
    session_id = kwargs.get("session_id")
    if not isinstance(session_id, str) or not session_id or session_id == _reported:
        return
    _reported = session_id
    try:
        subprocess.run(
            [_VELMUX, "agent-hook", "hermes"],
            input=json.dumps({"session_id": session_id}).encode(),
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            timeout=2,
            check=False,
        )
    except Exception:
        pass


def _observed(**kwargs) -> None:
    if kwargs.get("platform") == "cli":
        _report(**kwargs)


def register(ctx):
    ctx.register_hook("on_session_start", _report)
    ctx.register_hook("on_session_reset", _report)
    ctx.register_hook("pre_llm_call", _observed)
