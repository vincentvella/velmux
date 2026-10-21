//! Screenshots for the README: a demo scene built with the e2e harness (fake
//! Claude, fake `gh`), captured as SVG into `docs/screenshots/`.
//!
//! cargo test -p velmux-cli --test screenshots -- --ignored

mod common;

use std::path::{Path, PathBuf};

use common::*;

/// A Claude mid-task (velmux sees "esc to interrupt": working).
const WORKING: &str = "\
\x1b[1m> add a checkout flow that offers saved cards\x1b[0m

\x1b[37m⏺\x1b[0m I'll start with the cart store and the payment client.

\x1b[32m⏺\x1b[0m \x1b[1mRead\x1b[0m(src/cart/store.ts)
  ⎿  Read 142 lines

\x1b[32m⏺\x1b[0m \x1b[1mRead\x1b[0m(src/payments/client.ts)
  ⎿  Read 88 lines

\x1b[32m⏺\x1b[0m \x1b[1mUpdate\x1b[0m(src/checkout/CheckoutScreen.tsx)
  ⎿  Updated with \x1b[32m38 additions\x1b[0m and \x1b[31m4 removals\x1b[0m

\x1b[32m⏺\x1b[0m \x1b[1mBash\x1b[0m(bun test src/checkout)
  ⎿  \x1b[32m✓\x1b[0m 12 passed

\x1b[38;5;209m✻ Wiring up the saved-cards list… \x1b[2m(esc to interrupt)\x1b[0m

\x1b[2m╭─────────────────────────────────────────────────────────────────────────────╮\x1b[0m
\x1b[2m│\x1b[0m >                                                                           \x1b[2m│\x1b[0m
\x1b[2m╰─────────────────────────────────────────────────────────────────────────────╯\x1b[0m
";

/// A Claude waiting on a permission prompt (velmux: blocked).
const BLOCKED: &str = "\
\x1b[1m> the search results flicker when the query changes\x1b[0m

\x1b[37m⏺\x1b[0m The list re-mounts on every keystroke; keying it by query fixes that.

\x1b[32m⏺\x1b[0m \x1b[1mBash\x1b[0m(bun run lint --fix src/search)

\x1b[2m───────────────────────────────────────────────────────────────────────────────\x1b[0m
 \x1b[1mBash command\x1b[0m

   bun run lint --fix src/search

 Do you want to proceed?
 \x1b[36m❯ 1. Yes\x1b[0m
   2. Yes, and don't ask again for bun run lint
   3. No, and tell Claude what to do differently
";

/// A Claude that finished (idle).
const IDLE: &str = "\
\x1b[1m> rate-limit the public endpoints\x1b[0m

\x1b[37m⏺\x1b[0m Added a token bucket per API key (60/min, burst 20) in front of /v1/*,
  with a 429 and Retry-After when it runs dry. Tests cover the refill.

\x1b[2m╭─────────────────────────────────────────────────────────────────────────────╮\x1b[0m
\x1b[2m│\x1b[0m >                                                                           \x1b[2m│\x1b[0m
\x1b[2m╰─────────────────────────────────────────────────────────────────────────────╯\x1b[0m
";

fn out_dir() -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/screenshots");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn save(ui: &Ui, name: &str, title: &str) {
    std::fs::write(out_dir().join(format!("{name}.svg")), ui.svg(title)).unwrap();
}

/// A Claude in the base is asked `prompt` (so search finds it), claims
/// `branch`, then draws `screen` with `title`.
fn agent(env: &Env, ui: &Ui, branch: &str, prompt: &str, screen: &str, title: &str) {
    let file = env.root.join(format!("{}.ans", branch.replace('/', "-")));
    std::fs::write(&file, screen).unwrap();
    ui.prefix("a");
    ui.claude_id();
    ui.line(&format!("say {prompt}"));
    ui.wait_for("said ");
    ui.line(&format!("claim {branch}"));
    ui.wait_for("claimed ");
    ui.line(&format!("title {title}"));
    ui.line(&format!("show {}", file.display()));
}

#[test]
#[ignore = "writes docs/screenshots; run explicitly"]
fn screenshots() {
    let env = Env::new();
    let store = env.repo("storefront");
    let api = env.repo("api");
    // "On GitHub" for velmux and the fake gh, but fetching from a local copy.
    for (repo, name) in [(&store, "storefront"), (&api, "api")] {
        let bare = env.root.join(format!("remotes/{name}.git"));
        env.run_in(repo, &format!("git clone -q --bare . {0} && git remote add origin https://github.com/acme/{name}.git && git config --global url.{0}.insteadOf https://github.com/acme/{name}.git && git fetch -q origin", bare.display()));
    }
    let prs = r#"[
      {"headRefName":"feat/checkout","number":142,"url":"https://github.com/acme/storefront/pull/142","state":"OPEN","isDraft":false,
       "statusCheckRollup":[{"__typename":"CheckRun","name":"ci","status":"COMPLETED","conclusion":"SUCCESS"}]},
      {"headRefName":"fix/search-flicker","number":143,"url":"https://github.com/acme/storefront/pull/143","state":"OPEN","isDraft":false,
       "statusCheckRollup":[{"__typename":"CheckRun","name":"e2e","status":"COMPLETED","conclusion":"FAILURE"}]}
    ]"#;
    std::fs::write(env.root.join("prs.json"), prs).unwrap();

    // Parked work: released with a change on it.
    assert!(env.velmux(&store, &["init"]).status.success());
    env.velmux(&store, &["claim", "feat/promo-banner", "--no-fetch", "--no-install"]);
    let slot = env.slot(&store, "slot-1");
    env.run_in(Path::new(slot["path"].as_str().unwrap()), "echo wip > banner.tsx && echo wip > banner.css");
    env.velmux(&store, &["release", "slot-1"]);

    let ui = env.attach(&store);
    agent(&env, &ui, "feat/checkout", "add a checkout flow that offers saved cards | Saved cards come from the payment client's vault; I'll list them above the new-card form and default to the last one used.", WORKING, "✳ Checkout with saved cards");
    ui.wait_for("feat/checkout");
    agent(&env, &ui, "fix/search-flicker", "the search results flicker when the query changes | The result list re-mounts on every keystroke because it's keyed by index; keying it by query fixes the flicker.", BLOCKED, "✳ Search flicker");

    // A second repo, from the new-space chooser.
    ui.click_on("new space", false);
    ui.wait_for(" new space ");
    ui.send(b"api\r");
    ui.wait_for("api › base");
    agent(&env, &ui, "feat/rate-limits", "rate-limit the public endpoints, and the checkout webhook | Added a token bucket per API key (60/min, burst 20) in front of /v1/* and the checkout webhook, with a 429 and Retry-After.", IDLE, "✳ Rate limits");

    // A scratch session.
    let y = ui.find("──── +").map(|(_, y)| y).unwrap();
    ui.send(format!("\x1b[<0;30;{}M\x1b[<0;30;{}m", y + 1, y + 1).as_bytes());
    ui.wait_for("scratch ·");

    // Back to the working Claude, with PR marks in.
    ui.wait_for("#142✓");
    ui.wait_for("#143✗");
    ui.wait_for("▲");
    ui.click_on("feat/checkout", false);
    ui.wait_for("esc to interrupt");
    ui.wait_for("for keys"); // the status line has cleared
    save(&ui, "overview", "velmux — storefront › slot-2 · feat/checkout");

    ui.send(b"\x01");
    ui.wait_for(" esc cancel ");
    save(&ui, "prefix", "velmux — the prefix popup");
    ui.send(b"\x1b");

    ui.click_on("fix/search-fli", true);
    ui.wait_for("send failures to Claude");
    save(&ui, "slot-menu", "velmux — a slot's right-click menu");
    ui.send(b"\x1b");

    ui.prefix("/");
    ui.wait_for("find conversation");
    ui.send(b"checkout");
    ui.wait_for(" of ");
    save(&ui, "find", "velmux — find a conversation");
    ui.send(b"\x1b");

    ui.send(b"\x01\x14");
    ui.wait_for(" theme ");
    ui.send(b"j");
    ui.wait_for("rose-pine");
    save(&ui, "theme", "velmux — themes (rose-pine)");
    ui.send(b"\x1b");
}
