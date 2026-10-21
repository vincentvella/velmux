use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

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
    let root = tmp.path().canonicalize().unwrap();
    let base = root.join("ws/app");
    std::fs::create_dir_all(&base).unwrap();
    sh(&base, "git init -q -b main && git config user.email t@t && git config user.name t");
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
    assert!(std::fs::symlink_metadata(expected.join(".env")).unwrap().file_type().is_symlink());
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
