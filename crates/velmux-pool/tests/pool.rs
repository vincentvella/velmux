use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use velmux_pool::sys::Canonical as _;
use velmux_pool::{claude, ClaimOpts, Env, Pool, Status};
use tempfile::TempDir;

struct Fixture {
    _tmp: TempDir,
    base: PathBuf,
    env: Env,
}

fn sh(dir: &Path, cmd: &str) -> String {
    let out = Command::new("sh").arg("-c").arg(cmd).current_dir(dir).output().unwrap();
    assert!(out.status.success(), "`{cmd}` failed: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A base repo `<tmp>/ws/app` with one commit, a gitignored `.env`, and a
/// `.velmux.toml`; slots land in `<tmp>/ws/.pools/app`.
fn fixture(velmux_toml: &str) -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonical().unwrap();
    let base = root.join("ws/app");
    std::fs::create_dir_all(&base).unwrap();
    sh(&base, "git init -q -b main && git config user.email t@t && git config user.name t && git config core.autocrlf false");
    std::fs::write(base.join(".gitignore"), ".env\nnode_modules\n").unwrap();
    std::fs::write(base.join(".env"), "SECRET=1\n").unwrap();
    std::fs::write(base.join("README.md"), "hi\n").unwrap();
    std::fs::write(base.join(".velmux.toml"), velmux_toml).unwrap();
    sh(&base, "git add -A && git commit -qm init");
    let env = Env {
        global_config: root.join("no-global.toml"),
        claude_dir: root.join("claude"),
    };
    Fixture { _tmp: tmp, base, env }
}

fn quiet(_: &str) {}

fn opts(branch: Option<&str>) -> ClaimOpts {
    ClaimOpts { branch: branch.map(String::from), by: None, fetch: false, install: true }
}

#[test]
fn init_creates_slots_beside_base() {
    let f = fixture("slots = 3\n");
    let pool = Pool::discover(f.env.clone(), &f.base).unwrap();
    let slots = pool.init(&quiet).unwrap();
    assert_eq!(slots.len(), 3);
    let expected = f.base.parent().unwrap().join(".pools/app/slot-1");
    assert_eq!(slots[0].path, expected);
    assert!(slots.iter().all(|s| s.status == Status::Free));
    // env file shared from base, Claude transcripts grouped under base
    // (Windows without Developer Mode can't symlink a file: it's a hard link there.)
    assert!(cfg!(windows) || std::fs::symlink_metadata(expected.join(".env")).unwrap().file_type().is_symlink());
    assert_eq!(std::fs::read_to_string(expected.join(".env")).unwrap(), "SECRET=1\n");
    let link = claude::project_dir(&f.env.claude_dir, &expected);
    assert_eq!(std::fs::read_link(link).unwrap(), claude::project_dir(&f.env.claude_dir, &f.base));
    // idempotent, and discoverable from inside a slot
    assert_eq!(pool.init(&quiet).unwrap().len(), 3);
    let from_slot = Pool::discover(f.env.clone(), &expected).unwrap();
    assert_eq!(from_slot.base, f.base);
}

#[test]
fn release_parks_work_and_reclaim_restores_it() {
    let f = fixture("slots = 2\n");
    let pool = Pool::discover(f.env.clone(), &f.base).unwrap();
    pool.init(&quiet).unwrap();

    let s = pool.claim(&opts(Some("feat/x")), &quiet).unwrap();
    assert_eq!(s.status, Status::InUse);
    assert_eq!(s.branch.as_deref(), Some("feat/x"));
    std::fs::create_dir_all(s.path.join("node_modules/pkg")).unwrap();
    std::fs::write(s.path.join("node_modules/pkg/index.js"), "warm").unwrap();
    std::fs::write(s.path.join("README.md"), "edited\n").unwrap();
    std::fs::write(s.path.join("new.txt"), "untracked\n").unwrap();
    std::fs::create_dir_all(s.path.join("dist")).unwrap();
    std::fs::write(s.path.join("dist/out.js"), "build").unwrap();

    let r = pool.release(&s.id, false).unwrap();
    assert!(r.stashed.is_some());
    let after = pool.view(&s.id).unwrap();
    assert_eq!(after.status, Status::Free);
    assert_eq!(sh(&s.path, "git status --porcelain"), "");
    assert!(s.path.join("node_modules/pkg/index.js").exists(), "deps stay warm");
    assert!(s.path.join(".env").exists(), "linked env survives clean");
    assert!(!s.path.join("dist").exists(), "build output is cleaned");
    assert!(pool.release(&s.id, false).is_err(), "double release refused");

    // Reclaiming the branch (in whichever slot) brings the parked work back.
    let again = pool.claim(&opts(Some("feat/x")), &quiet).unwrap();
    assert_eq!(std::fs::read_to_string(again.path.join("README.md")).unwrap(), "edited\n");
    assert!(again.path.join("new.txt").exists());
    assert_eq!(sh(&again.path, "git stash list"), "");
}

#[test]
fn released_work_is_listed_as_parked_until_resumed_or_discarded() {
    let f = fixture("slots = 2\n");
    let pool = Pool::discover(f.env.clone(), &f.base).unwrap();
    pool.init(&quiet).unwrap();

    // Dirty work: parked with its stash and the sessions that worked on it.
    let s = pool.claim(&opts(Some("feat/a")), &quiet).unwrap();
    std::fs::write(s.path.join("README.md"), "edited\n").unwrap();
    std::fs::write(s.path.join("new.txt"), "untracked\n").unwrap();
    pool.release_with(&s.id, false, &["sess-1".into()]).unwrap();
    // Committed-only work: parked by its commits.
    let t = pool.claim(&opts(Some("feat/b")), &quiet).unwrap();
    sh(&t.path, "echo b > b && git add b && git commit -qm b");
    pool.release(&t.id, false).unwrap();
    // Nothing done: not parked.
    let u = pool.claim(&opts(Some("feat/c")), &quiet).unwrap();
    pool.release(&u.id, false).unwrap();

    let parked = pool.parked().unwrap();
    let names: Vec<&str> = parked.iter().map(|p| p.branch.as_str()).collect();
    assert_eq!(names.len(), 2, "{names:?}");
    let a = parked.iter().find(|p| p.branch == "feat/a").unwrap();
    assert_eq!((a.files, a.ahead, a.sessions.clone()), (2, 0, vec!["sess-1".to_string()]));
    let b = parked.iter().find(|p| p.branch == "feat/b").unwrap();
    assert_eq!((b.files, b.ahead), (0, 1));

    // Resuming (claiming the branch) restores it and takes it off the list.
    let again = pool.claim(&opts(Some("feat/a")), &quiet).unwrap();
    assert!(again.path.join("new.txt").exists());
    assert!(pool.parked().unwrap().iter().all(|p| p.branch != "feat/a"));

    // Discarding drops the stash; the branch stays.
    pool.release(&again.id, false).unwrap();
    assert!(pool.parked().unwrap().iter().any(|p| p.branch == "feat/a"));
    assert_eq!(pool.discard("feat/a").unwrap(), 1);
    assert_eq!(sh(&f.base, "git stash list"), "");
    assert!(pool.parked().unwrap().iter().all(|p| p.branch != "feat/a"));
    assert_eq!(sh(&f.base, "git branch --list feat/a"), "feat/a");
}

#[test]
fn slots_report_how_far_behind_and_sync_brings_them_up_to_date() {
    let f = fixture("slots = 1\nbase_ref = \"main\"\n");
    let pool = Pool::discover(f.env.clone(), &f.base).unwrap();
    pool.init(&quiet).unwrap();
    let s = pool.claim(&opts(Some("feat/s")), &quiet).unwrap();
    sh(&s.path, "echo mine > mine.txt && git add mine.txt && git commit -qm mine");
    sh(&f.base, "echo base > base.txt && git add base.txt && git commit -qm base1 && echo 2 >> base.txt && git commit -qam base2");
    std::fs::write(s.path.join("wip.txt"), "uncommitted\n").unwrap();
    let v = pool.view(&s.id).unwrap();
    assert_eq!((v.behind, v.ahead, v.conflict.clone()), (2, 1, None));

    let msg = pool.sync(&s.id, &quiet).unwrap();
    assert!(msg.contains("rebased"), "{msg}");
    let v = pool.view(&s.id).unwrap();
    assert_eq!((v.behind, v.ahead), (0, 1));
    assert!(s.path.join("base.txt").exists() && s.path.join("wip.txt").exists(), "rebased, wip kept");
    assert!(pool.sync(&s.id, &quiet).unwrap().contains("up to date"));
}

#[test]
fn sync_backs_out_of_conflicts() {
    let f = fixture("slots = 1\nbase_ref = \"main\"\n");
    let pool = Pool::discover(f.env.clone(), &f.base).unwrap();
    pool.init(&quiet).unwrap();
    let s = pool.claim(&opts(Some("feat/c")), &quiet).unwrap();
    sh(&s.path, "echo slot > README.md && git commit -qam slot");
    sh(&f.base, "echo base > README.md && git commit -qam base");
    let head = sh(&s.path, "git rev-parse HEAD");
    let err = pool.sync(&s.id, &quiet).unwrap_err().to_string();
    assert!(err.contains("conflicts in 1 file") && err.contains("README.md"), "{err}");
    assert_eq!(sh(&s.path, "git rev-parse HEAD"), head, "left as it was");
    assert_eq!(pool.view(&s.id).unwrap().conflict, None);
}

#[test]
fn each_slot_gets_its_own_ports() {
    let f = fixture("slots = 2\npost_claim = [\"echo $PORT $METRO > port.txt\"]\n[ports]\nstride = 100\nenv = { PORT = 3000, METRO = 8081 }\n");
    let pool = Pool::discover(f.env.clone(), &f.base).unwrap();
    pool.init(&quiet).unwrap();
    let env = pool.slot_env("slot-2");
    assert!(env.contains(&("PORT".into(), "3200".into())) && env.contains(&("METRO".into(), "8281".into())), "{env:?}");
    assert!(env.contains(&("VELMUX_SLOT".into(), "slot-2".into())));
    assert_eq!(pool.slot_port("slot-1"), Some(3100));
    // Hooks see them.
    let s = pool.claim(&opts(Some("feat/p")), &quiet).unwrap();
    let n = velmux_pool::config::Ports { enabled: true, stride: 100, env: Default::default() };
    assert!(n.for_slot(1).is_empty());
    let want = if s.id == "slot-1" { "3100 8181" } else { "3200 8281" };
    assert_eq!(std::fs::read_to_string(s.path.join("port.txt")).unwrap().trim(), want);
    // Off, and the default (PORT = 3000 + 10·n).
    let off = fixture("[ports]\nenabled = false\n");
    assert_eq!(Pool::discover(off.env.clone(), &off.base).unwrap().slot_port("slot-1"), None);
    let plain = fixture("");
    assert_eq!(Pool::discover(plain.env.clone(), &plain.base).unwrap().slot_port("slot-3"), Some(3030));
}

/// The fixture's repo pushed to a bare "origin" (tracked by main), and a
/// second clone to push new commits from.
fn with_origin(f: &Fixture) -> PathBuf {
    let root = f.base.parent().unwrap().to_path_buf();
    sh(&root, "git clone -q --bare app origin.git");
    sh(&f.base, "git remote add origin ../origin.git && git fetch -q origin && git branch -q --set-upstream-to=origin/main");
    sh(&root, "git clone -q origin.git other && cd other && git config user.email t@t && git config user.name t && git config core.autocrlf false");
    root.join("other")
}

#[test]
fn the_checkout_fast_forwards_to_its_upstream_when_it_can() {
    use velmux_pool::BaseUpdate;
    let f = fixture("slots = 1\nbase_ref = \"origin/main\"\n");
    let other = with_origin(&f);
    let pool = Pool::discover(f.env.clone(), &f.base).unwrap();
    assert_eq!(pool.update_base(), BaseUpdate::UpToDate);

    sh(&other, "echo a > a && git add a && git commit -qm a && echo b > b && git add b && git commit -qm b && git push -q");
    pool.fetch().unwrap();
    // A local edit to an unrelated file doesn't get in the way.
    std::fs::write(f.base.join("README.md"), "local\n").unwrap();
    assert_eq!(pool.update_base(), BaseUpdate::FastForwarded(2));
    assert!(f.base.join("b").exists());
    assert_eq!(std::fs::read_to_string(f.base.join("README.md")).unwrap(), "local\n");

    // One that the new commit touches does: left as it was.
    sh(&other, "echo theirs > README.md && git commit -qam readme && git push -q");
    pool.fetch().unwrap();
    assert_eq!(pool.update_base(), BaseUpdate::Blocked(1));
    assert_eq!(std::fs::read_to_string(f.base.join("README.md")).unwrap(), "local\n");

    // Commits of its own: never touched.
    sh(&f.base, "git checkout -q README.md && echo mine > mine && git add mine && git commit -qm mine");
    assert_eq!(pool.update_base(), BaseUpdate::Diverged { behind: 1, ahead: 1 });
}

#[test]
fn changes_in_the_checkout_move_to_a_slot() {
    let f = fixture("slots = 1\n");
    let pool = Pool::discover(f.env.clone(), &f.base).unwrap();
    pool.init(&quiet).unwrap();
    assert!(pool.base_changes().is_empty());
    std::fs::write(f.base.join("README.md"), "edited in the checkout\n").unwrap();
    std::fs::write(f.base.join("new.txt"), "new\n").unwrap();
    assert_eq!(pool.base_changes().len(), 2);

    pool.stash_base_for("feat/moved").unwrap();
    assert!(pool.base_changes().is_empty(), "the checkout is clean again");
    let s = pool.claim(&opts(Some("feat/moved")), &quiet).unwrap();
    assert_eq!(std::fs::read_to_string(s.path.join("README.md")).unwrap(), "edited in the checkout\n");
    assert!(s.path.join("new.txt").exists());
    assert!(pool.stash_base_for("x").is_err(), "nothing left to move");
}

#[test]
fn detached_commits_are_rescued_on_release() {
    let f = fixture("slots = 1\n");
    let pool = Pool::discover(f.env.clone(), &f.base).unwrap();
    pool.init(&quiet).unwrap();
    let s = pool.claim(&opts(None), &quiet).unwrap();
    sh(&s.path, "echo x > a && git add a && git commit -qm detached-work");
    let sha = sh(&s.path, "git rev-parse HEAD");
    let r = pool.release(&s.id, false).unwrap();
    let rescue = r.rescued.expect("rescue branch");
    assert_eq!(sh(&f.base, &format!("git rev-parse {rescue}")), sha);
}

#[test]
fn concurrent_claims_get_distinct_slots() {
    let f = fixture("slots = 5\n");
    let pool = Arc::new(Pool::discover(f.env.clone(), &f.base).unwrap());
    pool.init(&quiet).unwrap();
    let handles: Vec<_> = (0..5)
        .map(|i| {
            let pool = pool.clone();
            std::thread::spawn(move || {
                pool.claim(&opts(Some(&format!("w/{i}"))), &quiet).unwrap().id
            })
        })
        .collect();
    let ids: HashSet<String> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(ids.len(), 5);
    let err = pool.claim(&opts(Some("w/6")), &quiet).unwrap_err();
    assert!(format!("{err:#}").contains("in use"));
}

#[test]
fn install_is_skipped_while_lockfile_is_unchanged() {
    let f = fixture("slots = 1\ninstall_cmd = \"mkdir -p node_modules && echo run >> ../installs.log\"\n");
    std::fs::write(f.base.join("package-lock.json"), "{}").unwrap();
    sh(&f.base, "git add -A && git commit -qm lock");
    let pool = Pool::discover(f.env.clone(), &f.base).unwrap();
    pool.init(&quiet).unwrap();
    let log = pool.dir.join("installs.log");
    let count = || std::fs::read_to_string(&log).unwrap_or_default().lines().count();
    assert_eq!(count(), 1, "init installs once");

    let s = pool.claim(&opts(Some("a")), &quiet).unwrap();
    pool.release(&s.id, false).unwrap();
    pool.claim(&opts(Some("b")), &quiet).unwrap();
    assert_eq!(count(), 1, "unchanged lockfile → no reinstall");
    pool.release(&s.id, false).unwrap();

    // A branch that changes the lockfile triggers an install on its next claim.
    let s = pool.claim(&opts(Some("c")), &quiet).unwrap();
    sh(&s.path, "echo '{\"x\":1}' > package-lock.json && git commit -qam bump");
    pool.release(&s.id, false).unwrap();
    pool.claim(&opts(Some("c")), &quiet).unwrap();
    assert_eq!(count(), 2);
}

#[test]
fn a_failed_install_is_a_warning_and_tried_again() {
    let f = fixture("slots = 1\ninstall_cmd = \"echo run >> ../installs.log; [ -f ../fixed ] || { echo broken >&2; exit 3; }\"\n");
    std::fs::write(f.base.join("package-lock.json"), "{}").unwrap();
    sh(&f.base, "git add -A && git commit -qm lock");
    let pool = Pool::discover(f.env.clone(), &f.base).unwrap();
    let said = std::sync::Mutex::new(Vec::new());
    let report = |m: &str| said.lock().unwrap().push(m.to_string());
    // Setting the slots up doesn't fail on it, and neither does claiming one.
    pool.init(&report).unwrap();
    let s = pool.claim(&opts(Some("a")), &report).unwrap();
    assert_eq!(s.status, Status::InUse);
    assert!(s.install_error.as_deref().is_some_and(|e| e.contains("broken")), "{:?}", s.install_error);
    assert!(said.lock().unwrap().iter().any(|m| m.contains("warning: installing dependencies failed")));
    // The lockfile hasn't changed, but a failed install isn't skipped next time…
    pool.release(&s.id, false).unwrap();
    std::fs::write(pool.dir.join("fixed"), "").unwrap();
    let s = pool.claim(&opts(Some("a")), &quiet).unwrap();
    let runs = std::fs::read_to_string(pool.dir.join("installs.log")).unwrap().lines().count();
    assert_eq!(runs, 3, "init, the first claim, and this one");
    // …and once it works, the slot stops saying it failed.
    assert_eq!(s.install_error, None);
}

#[test]
fn adopts_a_pool_laid_out_beside_the_repo() {
    let f = fixture("slots = 3\n");
    let ws = f.base.parent().unwrap().to_path_buf();
    // Another tool's pool: `app-pool-N` next to the repo, one free, one at work.
    sh(&f.base, "git worktree add -q --detach ../app-pool-1 && git worktree add -q -b feat ../app-pool-2");
    sh(&ws, "mkdir app-pool-9"); // not a worktree: left alone
    let pool = Pool::discover(f.env.clone(), &f.base).unwrap();
    assert_eq!(pool.siblings().len(), 2);
    let slots = pool.init(&quiet).unwrap();
    assert_eq!(slots.len(), 3);
    assert_eq!((slots[0].id.as_str(), slots[0].path.clone(), slots[0].status), ("slot-1", ws.join("app-pool-1"), Status::Free));
    assert_eq!((slots[1].path.clone(), slots[1].status, slots[1].branch.as_deref()), (ws.join("app-pool-2"), Status::InUse, Some("feat")));
    // New slots follow the same layout.
    assert_eq!(slots[2].path, ws.join("app-pool-3"));
    assert!(ws.join("app-pool-3/README.md").exists());
    assert!(!ws.join(".pools/app/slot-1").exists());
    // Found from inside one, idempotent, and agents get each folder.
    let again = Pool::discover(f.env.clone(), &ws.join("app-pool-2")).unwrap();
    assert_eq!(again.slot_of(&ws.join("app-pool-2")).unwrap().as_deref(), Some("slot-2"));
    assert_eq!(again.adopt(&quiet).unwrap(), 0);
    assert_eq!(again.slot_dirs().len(), 4);
    // Claiming uses the free one.
    let claimed = pool.claim(&opts(Some("next")), &quiet).unwrap();
    assert_eq!(claimed.path, ws.join("app-pool-1"));
}

#[test]
fn old_parked_work_can_be_discarded_in_one_go() {
    let f = fixture("slots = 2\n");
    let pool = Pool::discover(f.env.clone(), &f.base).unwrap();
    pool.init(&quiet).unwrap();
    for branch in ["feat/old", "feat/new"] {
        let slot = pool.claim(&opts(Some(branch)), &quiet).unwrap();
        std::fs::write(slot.path.join("wip.txt"), "wip").unwrap();
        pool.release(&slot.id, false).unwrap();
    }
    // Make feat/old look parked 40 days ago.
    let state = pool.dir.join("pool.json");
    let mut v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&state).unwrap()).unwrap();
    for p in v["parked"].as_array_mut().unwrap() {
        if p["branch"] == "feat/old" {
            p["parked_at"] = serde_json::json!(velmux_pool::state::now() - 40 * 86400);
        }
    }
    std::fs::write(&state, serde_json::to_string(&v).unwrap()).unwrap();
    assert_eq!(pool.discard_older_than(30).unwrap(), vec!["feat/old".to_string()]);
    let left: Vec<String> = pool.parked().unwrap().into_iter().map(|p| p.branch).collect();
    assert_eq!(left, vec!["feat/new".to_string()]);
}
