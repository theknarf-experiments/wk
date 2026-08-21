//! automergefs — a node that *is* a synced filesystem.
//!
//! The served tree is an Automerge repo in pushwork's `vfs` shape (one
//! directory doc mapping relative paths to per-file docs), kept live over a
//! subduction websocket. The subduction server stays wherever it already
//! runs — for the flow-page setup, a Docker container on host localhost,
//! reached through a HostService node as `ws://subduction:8080` — and this
//! guest is the sync client; nodes wired to it mount the repo like any other
//! provider tree. Reads come from the synced docs; writes become Automerge
//! changes pushed back, so a mounted `echo hi > note.txt` lands in the same
//! repo the browser and `pushwork clone` see.
//!
//! One thread does everything, cooperatively: the serve loop uses
//! `poll-request` (a bounded wait) and pumps the sync engine between
//! requests. The blocking `next-request` would park us inside a host call
//! and starve the connection.
//!
//! Usage: `automergefs <ws-url> <doc-url> [service-name]`
//! The service name must match the server's `--service-name` (the handshake
//! audience is hashed from it); it defaults to the url's host:port, which is
//! only right when no proxy or name-rewrite sits between — for the flow-page
//! compose it is `localhost:8080`.

mod fs;
mod repo;
mod rt;
mod ws;

#[allow(warnings)]
mod bindings {
    wit_bindgen::generate!({
        world: "automergefs",
        path: "wit",
        generate_all,
    });
}

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use automerge::Automerge;
use bindings::wk::fs::provider::{self, Polled};
use future_form::Sendable;
use sedimentree_core::id::SedimentreeId;
use subduction_core::{
    handshake::{self, audience::Audience},
    policy::open::OpenPolicy,
    storage::memory::MemoryStorage,
    subduction::builder::SubductionBuilder,
    timeout::call::CallTimeout,
    transport::message::MessageTransport,
};
use subduction_crypto::nonce::Nonce;
use subduction_crypto::signer::memory::MemorySigner;

fn now_secs() -> subduction_core::timestamp::TimestampSeconds {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    subduction_core::timestamp::TimestampSeconds::new(secs)
}

fn main() {
    let mut args = std::env::args().skip(1);
    let server = args
        .next()
        .unwrap_or_else(|| "ws://subduction:8080".to_string());
    let Some(doc_url) = args.next() else {
        eprintln!("usage: automergefs <ws-url> <automerge-url> [service-name]");
        return;
    };
    let service_name = args.next().unwrap_or_else(|| {
        server
            .strip_prefix("ws://")
            .unwrap_or(&server)
            .split('/')
            .next()
            .unwrap_or("localhost")
            .to_string()
    });
    println!("[automergefs] {server} doc {doc_url} (service name {service_name})");

    let root = match repo::parse_doc_id(&doc_url) {
        Ok(id) => id,
        Err(e) => {
            eprintln!("[automergefs] {e}");
            return;
        }
    };

    if let Err(e) = run(&server, &service_name, root, &doc_url) {
        eprintln!("[automergefs] {e}");
    }
}

/// Everything lives in one function so the engine's (large, inferred)
/// generic types never need naming: build it, connect, sync, then serve.
fn run(server: &str, service_name: &str, root: SedimentreeId, doc_url: &str) -> Result<(), String> {
    let exec = rt::Exec::new();
    let signer = MemorySigner::generate();

    let (subduction, _handler, listener_fut, manager_fut) =
        SubductionBuilder::<_, _, _, _, _, 256>::new()
            .signer(signer.clone())
            .storage(MemoryStorage::default(), Arc::new(OpenPolicy))
            .spawner(exec.clone())
            .timer(rt::Timer)
            .build::<Sendable, MessageTransport<ws::WsTransport>>();
    {
        use subduction_core::spawn::Spawn;
        let _ = exec.spawn(Box::pin(async move {
            if let Err(e) = listener_fut.await {
                eprintln!("[automergefs] listener died: {e}");
            }
        }));
        let _ = exec.spawn(Box::pin(async move {
            let _ = manager_fut.await;
        }));
    }

    // Dial, then authenticate: the audience is hashed from the *service
    // name*, not the address we dialed — see the module docs.
    let transport = ws::WsTransport::new(ws::connect(server)?);
    let audience = Audience::discover(service_name.as_bytes());
    let (authenticated, ()) = exec
        .block_on(
            handshake::initiate::<Sendable, _, _, _, _>(
                transport.clone(),
                |t: ws::WsTransport, _peer| (t, ()),
                &signer,
                audience,
                now_secs(),
                Nonce::random(),
            ),
            Duration::from_secs(20),
        )
        .map_err(|_| "handshake timed out".to_string())?
        .map_err(|e| format!("handshake refused: {e}"))?;
    let peer = authenticated.peer_id();
    println!("[automergefs] authenticated to peer {peer}");

    exec.block_on(
        subduction.add_connection(authenticated.map(MessageTransport::new)),
        Duration::from_secs(10),
    )
    .map_err(|_| "add_connection timed out".to_string())?
    .map_err(|e| format!("add_connection: {e}"))?;

    // ── Small sync helpers; all engine types stay inferred. ──

    // Every blob subduction holds for a doc, briefly polling: the batch-sync
    // response may be processed by the listener after `sync_with_peer`
    // resolves.
    let load = |id: SedimentreeId, wait: Duration| -> Result<Automerge, String> {
        let deadline = Instant::now() + wait;
        let blobs = loop {
            let blobs: Vec<Vec<u8>> = exec
                .block_on(subduction.get_blobs(id), Duration::from_secs(10))
                .map_err(|_| "get_blobs timed out".to_string())?
                .map_err(|e| format!("get_blobs: {e}"))?
                .map(|ne| ne.into_iter().map(|b| b.as_slice().to_vec()).collect())
                .unwrap_or_default();
            if !blobs.is_empty() || Instant::now() >= deadline {
                break blobs;
            }
            exec.pump();
            std::thread::sleep(Duration::from_millis(20));
        };
        repo::load_doc(blobs)
    };
    let fetch = |id: SedimentreeId| -> Result<Automerge, String> {
        exec.block_on(
            subduction.sync_with_peer(&peer, id, true, CallTimeout::Default),
            Duration::from_secs(30),
        )
        .map_err(|_| format!("sync of {id} timed out"))?
        .map_err(|e| format!("sync of {id}: {e}"))?;
        load(id, Duration::from_secs(5))
    };
    let blob_count = |id: SedimentreeId| -> usize {
        exec.block_on(subduction.get_blobs(id), Duration::from_secs(10))
            .ok()
            .and_then(|r| r.ok())
            .flatten()
            .map(|ne| ne.len())
            .unwrap_or(0)
    };
    // Push the last local change of `doc` (stores locally + syncs to peers).
    let push_change = |doc: &mut Automerge, id: SedimentreeId| -> Result<(), String> {
        let Some((head, parents, blob)) = repo::last_change(doc) else {
            return Ok(());
        };
        exec.block_on(
            subduction.add_commits_batch(id, vec![(head, parents, blob)], CallTimeout::Default),
            Duration::from_secs(30),
        )
        .map_err(|_| "push timed out".to_string())?
        .map_err(|e| format!("push: {e}"))?;
        Ok(())
    };
    // Upload a brand-new doc wholesale.
    let push_new = |doc: &Automerge, id: SedimentreeId| -> Result<(), String> {
        let (tree, blobs) = repo::ingest_doc(doc, id);
        exec.block_on(
            subduction.add_sedimentree(id, tree, blobs, CallTimeout::Default),
            Duration::from_secs(30),
        )
        .map_err(|_| "upload timed out".to_string())?
        .map_err(|e| format!("upload: {e}"))?;
        Ok(())
    };

    // ── Initial sync: the directory doc, then every file doc it names. ──
    let mut dir_doc = fetch(root)?;
    let mut tree = fs::MemTree::empty();
    let mut file_ids: HashMap<String, SedimentreeId> = HashMap::new();
    let mut docs: HashMap<SedimentreeId, Automerge> = HashMap::new();
    for (path, url) in repo::dir_leaves(&dir_doc) {
        match repo::parse_doc_id(&url).and_then(&fetch) {
            Ok(doc) => {
                let id = repo::parse_doc_id(&url).expect("parsed once already");
                tree.files.insert(path.clone(), repo::file_content(&doc));
                docs.insert(id, doc);
                file_ids.insert(path, id);
            }
            Err(e) => eprintln!("[automergefs] {path}: {e}"),
        }
    }
    let mut counts: HashMap<SedimentreeId, usize> = file_ids
        .values()
        .copied()
        .chain([root])
        .map(|id| (id, blob_count(id)))
        .collect();
    println!(
        "[automergefs] serving {} files from {doc_url}",
        tree.files.len()
    );

    // ── The serve loop: answer ops, flush local edits, pull remote ones. ──
    let mut last_flush = Instant::now();
    let mut last_check = Instant::now();
    loop {
        match provider::poll_request(5) {
            Polled::Request(req) => {
                let outcome = tree.handle(req.op);
                provider::reply(req.id, outcome.as_ref());
            }
            Polled::Empty => {
                exec.pump();

                // Local edits → automerge changes, debounced a touch so a
                // write burst (an editor saving) flushes once.
                if !tree.dirty.is_empty() && last_flush.elapsed() >= Duration::from_millis(400) {
                    last_flush = Instant::now();
                    let dirty: Vec<String> = tree.dirty.drain().collect();
                    for path in dirty {
                        let outcome = flush_path(
                            &path,
                            &mut tree,
                            &mut dir_doc,
                            &mut docs,
                            &mut file_ids,
                            root,
                            &push_change,
                            &push_new,
                        );
                        if let Err(e) = outcome {
                            eprintln!("[automergefs] flush {path}: {e}");
                        }
                    }
                    counts.insert(root, blob_count(root));
                    for id in file_ids.values() {
                        counts.insert(*id, blob_count(*id));
                    }
                }

                // Remote edits → refreshed files (skipping locally-dirty
                // paths so a slow flush isn't clobbered).
                if last_check.elapsed() >= Duration::from_millis(1000) {
                    last_check = Instant::now();
                    let root_now = blob_count(root);
                    if counts.get(&root) != Some(&root_now) {
                        counts.insert(root, root_now);
                        if let Ok(fresh) = load(root, Duration::from_millis(100)) {
                            dir_doc = fresh;
                            let current: HashMap<String, SedimentreeId> =
                                repo::dir_leaves(&dir_doc)
                                    .into_iter()
                                    .filter_map(|(p, u)| {
                                        repo::parse_doc_id(&u).ok().map(|id| (p, id))
                                    })
                                    .collect();
                            tree.files
                                .retain(|p, _| current.contains_key(p) || tree.dirty.contains(p));
                            for (path, id) in &current {
                                if !file_ids.contains_key(path) {
                                    if let Ok(doc) = fetch(*id) {
                                        tree.files.insert(path.clone(), repo::file_content(&doc));
                                        counts.insert(*id, blob_count(*id));
                                        docs.insert(*id, doc);
                                    }
                                }
                            }
                            file_ids = current;
                        }
                    }
                    for (path, id) in &file_ids {
                        if tree.dirty.contains(path) {
                            continue;
                        }
                        let now = blob_count(*id);
                        if counts.get(id) != Some(&now) {
                            counts.insert(*id, now);
                            if let Ok(doc) = load(*id, Duration::from_millis(100)) {
                                tree.files.insert(path.clone(), repo::file_content(&doc));
                                docs.insert(*id, doc);
                                println!("[automergefs] refreshed {path}");
                            }
                        }
                    }
                }
            }
            Polled::Shutdown => break,
        }
    }
    println!("[automergefs] shutting down");
    Ok(())
}

/// Turn one dirty path into pushed changes: modify its file doc, create a
/// new doc (and directory entry) for a new file, or drop the entry for a
/// deleted one. Renames arrive as delete + create — the new path gets a new
/// doc (history does not follow the rename; pushwork tolerates this).
#[allow(clippy::too_many_arguments)]
fn flush_path(
    path: &str,
    tree: &mut fs::MemTree,
    dir_doc: &mut Automerge,
    docs: &mut HashMap<SedimentreeId, Automerge>,
    file_ids: &mut HashMap<String, SedimentreeId>,
    root: SedimentreeId,
    push_change: &impl Fn(&mut Automerge, SedimentreeId) -> Result<(), String>,
    push_new: &impl Fn(&Automerge, SedimentreeId) -> Result<(), String>,
) -> Result<(), String> {
    match (tree.files.get(path), file_ids.get(path).copied()) {
        // Modified: update the file doc's content, push the change.
        (Some(content), Some(id)) => {
            let doc = docs
                .get_mut(&id)
                .ok_or_else(|| "no cached doc".to_string())?;
            repo::set_content(doc, content)?;
            push_change(doc, id)
        }
        // New file: a fresh doc, uploaded whole, then named in the dir doc.
        (Some(content), None) => {
            let (url, id) = repo::new_doc_url();
            let doc = repo::make_file_doc(path, content)?;
            push_new(&doc, id)?;
            docs.insert(id, doc);
            file_ids.insert(path.to_string(), id);
            repo::set_dir_entry(dir_doc, path, Some(&url))?;
            push_change(dir_doc, root)?;
            println!("[automergefs] created {path} as {url}");
            Ok(())
        }
        // Deleted: drop the directory entry (the file doc stays server-side,
        // like flow-page's best-effort delete).
        (None, Some(id)) => {
            repo::set_dir_entry(dir_doc, path, None)?;
            push_change(dir_doc, root)?;
            file_ids.remove(path);
            docs.remove(&id);
            println!("[automergefs] removed {path}");
            Ok(())
        }
        // Created and deleted between flushes: nothing to sync.
        (None, None) => Ok(()),
    }
}
