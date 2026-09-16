//! The message loop, and what the server knows between messages.
//!
//! ### Units
//!
//! A **unit** is one command line analyzed: a target of a workspace, or a file
//! with no manifest. Each open document is covered by the first unit that reads
//! it, and a unit is analyzed again only when one of the files it read changed,
//! or when its workspace was prepared again. A file's diagnostics are those of
//! every unit that read it.
//!
//! ### Preparing a workspace
//!
//! twig runs on a thread of its own, because building a dependency can take
//! seconds; the loop carries on and analyzes the workspace when the answer
//! arrives. A saved file may be a dependency's, so a save prepares every
//! workspace again, which costs a check per library when nothing changed.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use crossbeam_channel::{Sender, select};
use lsp_server::{Connection, ErrorCode, Message, Notification, Request, Response};
use lsp_types::notification::{
    DidChangeTextDocument, DidCloseTextDocument, DidOpenTextDocument, DidSaveTextDocument, Exit,
    Notification as _, PublishDiagnostics,
};
use lsp_types::{
    DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    DidSaveTextDocumentParams, PublishDiagnosticsParams, SaveOptions, ServerCapabilities,
    TextDocumentSyncCapability, TextDocumentSyncKind, TextDocumentSyncOptions,
    TextDocumentSyncSaveOptions, Uri,
};

use crate::analysis::{self, Outcome, path_to_uri, uri_to_path};
use crate::workspace::{self, Metadata, Toolchain};

/// Run the server over `conn` until the client says to exit. `toolchain` makes
/// one from the `initializationOptions`.
pub fn run(
    conn: &Connection,
    toolchain: impl FnOnce(&serde_json::Value) -> Arc<dyn Toolchain>,
) -> Result<(), String> {
    let capabilities = ServerCapabilities {
        text_document_sync: Some(TextDocumentSyncCapability::Options(TextDocumentSyncOptions {
            open_close: Some(true),
            change: Some(TextDocumentSyncKind::FULL),
            save: Some(TextDocumentSyncSaveOptions::SaveOptions(SaveOptions {
                include_text: Some(false),
            })),
            ..Default::default()
        })),
        ..Default::default()
    };
    let params = conn
        .initialize(serde_json::to_value(capabilities).expect("capabilities serialize"))
        .map_err(|e| e.to_string())?;
    let options = params.get("initializationOptions").cloned().unwrap_or_default();

    let (prepared_tx, prepared) = crossbeam_channel::unbounded();
    let mut server = Server {
        sender: conn.sender.clone(),
        toolchain: toolchain(&options),
        docs: HashMap::new(),
        workspaces: HashMap::new(),
        prepared_tx,
        units: HashMap::new(),
        changed: HashSet::new(),
        published: HashMap::new(),
    };
    loop {
        select! {
            recv(conn.receiver) -> msg => {
                let Ok(msg) = msg else { return Ok(()) };
                if server.handle(conn, msg)? == Flow::Exit {
                    return Ok(());
                }
            }
            recv(prepared) -> done => {
                let (root, result) = done.expect("the server holds a sender");
                server.prepared(root, result);
            }
        }
        // Everything already waiting is taken before analyzing, so that a burst
        // of edits is analyzed once.
        loop {
            if let Ok(msg) = conn.receiver.try_recv() {
                if server.handle(conn, msg)? == Flow::Exit {
                    return Ok(());
                }
            } else if let Ok((root, result)) = prepared.try_recv() {
                server.prepared(root, result);
            } else {
                break;
            }
        }
        server.analyze();
    }
}

#[derive(PartialEq, Eq)]
enum Flow {
    Continue,
    Exit,
}

/// Where a workspace's preparation is.
#[derive(Default)]
struct Workspace {
    /// The last answer.
    meta: Option<Result<Metadata, String>>,
    running: bool,
    /// Whether it has to run again once it finishes, because something was
    /// saved while it ran.
    again: bool,
}

/// A command line, and the workspace it came from.
#[derive(Clone, PartialEq, Eq, Hash)]
struct UnitKey {
    root: Option<PathBuf>,
    args: Vec<String>,
}

impl UnitKey {
    /// Whether an open document at `doc` would show this unit's setup error:
    /// every document of its workspace, or the one file it is.
    fn is_about(&self, doc: &Path) -> bool {
        match &self.root {
            Some(root) => doc.starts_with(root),
            None => self.args.first().is_some_and(|a| Path::new(a) == doc),
        }
    }
}

struct Server {
    sender: Sender<Message>,
    toolchain: Arc<dyn Toolchain>,
    /// The open documents' text.
    docs: HashMap<PathBuf, String>,
    workspaces: HashMap<PathBuf, Workspace>,
    prepared_tx: Sender<(PathBuf, Result<Metadata, String>)>,
    /// What each unit found, or why it could not be analyzed.
    units: HashMap<UnitKey, Result<Outcome, String>>,
    /// Files edited since the last analysis.
    changed: HashSet<PathBuf>,
    /// What was last published, by file.
    published: HashMap<PathBuf, Vec<lsp_types::Diagnostic>>,
}

impl Server {
    fn handle(&mut self, conn: &Connection, msg: Message) -> Result<Flow, String> {
        match msg {
            Message::Request(req) => {
                if conn.handle_shutdown(&req).map_err(|e| e.to_string())? {
                    return Ok(Flow::Exit);
                }
                self.request(req);
            }
            Message::Notification(n) => {
                if n.method == Exit::METHOD {
                    return Ok(Flow::Exit);
                }
                self.notification(n);
            }
            Message::Response(_) => {}
        }
        Ok(Flow::Continue)
    }

    fn request(&mut self, req: Request) {
        let response = Response::new_err(
            req.id,
            ErrorCode::MethodNotFound as i32,
            format!("`{}` is not supported", req.method),
        );
        self.send(response.into());
    }

    fn notification(&mut self, n: Notification) {
        match n.method.as_str() {
            DidOpenTextDocument::METHOD => {
                let Ok(p) = serde_json::from_value::<DidOpenTextDocumentParams>(n.params) else { return };
                self.set(&p.text_document.uri, Some(p.text_document.text));
            }
            DidChangeTextDocument::METHOD => {
                let Ok(mut p) = serde_json::from_value::<DidChangeTextDocumentParams>(n.params) else { return };
                // Full synchronization: the last change is the whole text.
                if let Some(change) = p.content_changes.pop() {
                    self.set(&p.text_document.uri, Some(change.text));
                }
            }
            DidCloseTextDocument::METHOD => {
                let Ok(p) = serde_json::from_value::<DidCloseTextDocumentParams>(n.params) else { return };
                self.set(&p.text_document.uri, None);
            }
            DidSaveTextDocument::METHOD => {
                let Ok(_) = serde_json::from_value::<DidSaveTextDocumentParams>(n.params) else { return };
                let roots: Vec<PathBuf> = self.workspaces.keys().cloned().collect();
                for root in roots {
                    self.prepare(&root);
                }
            }
            _ => {}
        }
    }

    /// The document at `uri` is now `text`, or closed.
    fn set(&mut self, uri: &Uri, text: Option<String>) {
        let Some(path) = uri_to_path(uri) else { return };
        match text {
            Some(text) => self.docs.insert(path.clone(), text),
            // A closed document is read from disk again, which may be different.
            None => self.docs.remove(&path),
        };
        self.changed.insert(path);
    }

    /// Start preparing the workspace at `root`, or ask for it to run again when
    /// it is already running.
    fn prepare(&mut self, root: &Path) {
        let ws = self.workspaces.entry(root.to_path_buf()).or_default();
        if ws.running {
            ws.again = true;
            return;
        }
        ws.running = true;
        let toolchain = self.toolchain.clone();
        let done = self.prepared_tx.clone();
        let root = root.to_path_buf();
        std::thread::spawn(move || {
            let result = toolchain.prepare(&root);
            let _ = done.send((root, result));
        });
    }

    fn prepared(&mut self, root: PathBuf, result: Result<Metadata, String>) {
        let ws = self.workspaces.entry(root.clone()).or_default();
        ws.meta = Some(result);
        ws.running = false;
        if std::mem::take(&mut ws.again) {
            self.prepare(&root);
        }
        self.units.retain(|key, _| key.root.as_ref() != Some(&root));
    }

    /// Analyze whatever is out of date, and publish what changed.
    fn analyze(&mut self) {
        let changed = std::mem::take(&mut self.changed);
        let docs = &self.docs;
        self.units.retain(|_, outcome| match outcome {
            Ok(o) => o.files.iter().any(|f| docs.contains_key(f)) && o.files.is_disjoint(&changed),
            // An error is about the setup, which an edit does not change; it
            // goes when no open document is in its workspace any more.
            Err(_) => true,
        });

        let buffers: analysis::Buffers = Rc::new(self.docs.clone());
        let mut paths: Vec<PathBuf> = self.docs.keys().cloned().collect();
        paths.sort();
        for path in paths {
            if self.covers(&path) {
                continue;
            }
            let Some(root) = workspace::find_root(&path) else {
                let key = UnitKey { root: None, args: vec![path.display().to_string()] };
                let outcome = analysis::analyze(&key.args, buffers.clone());
                self.units.insert(key, outcome);
                continue;
            };
            let meta = match self.workspaces.get(&root).and_then(|ws| ws.meta.as_ref()) {
                None => {
                    if !self.workspaces.get(&root).is_some_and(|ws| ws.running) {
                        self.prepare(&root);
                    }
                    continue;
                }
                Some(Err(why)) => {
                    let key = UnitKey { root: Some(root), args: Vec::new() };
                    self.units.insert(key, Err(why.clone()));
                    continue;
                }
                Some(Ok(meta)) => meta.clone(),
            };
            for args in workspace::candidates(&meta, &path) {
                let key = UnitKey { root: Some(root.clone()), args };
                if !self.units.contains_key(&key) {
                    let outcome = analysis::analyze(&key.args, buffers.clone());
                    self.units.insert(key.clone(), outcome);
                }
                if self.units[&key].as_ref().is_ok_and(|o| o.files.contains(&path)) {
                    break;
                }
            }
        }
        // A setup error stays while a document it is about is open.
        let docs = &self.docs;
        self.units.retain(|key, outcome| outcome.is_ok() || docs.keys().any(|d| key.is_about(d)));
        self.publish();
    }

    /// Whether some unit already read `path`.
    fn covers(&self, path: &Path) -> bool {
        self.units.values().any(|o| o.as_ref().is_ok_and(|o| o.files.contains(path)))
    }

    /// Publish every file's diagnostics that changed, and clear the ones that
    /// have none now.
    fn publish(&mut self) {
        let mut all: HashMap<PathBuf, Vec<lsp_types::Diagnostic>> = HashMap::new();
        for (key, outcome) in &self.units {
            match outcome {
                Ok(o) => {
                    for (path, diags) in &o.diagnostics {
                        all.entry(path.clone()).or_default().extend(diags.iter().cloned());
                    }
                }
                // No file to pin it to, so it goes on every open document it
                // is about, at the top.
                Err(why) => {
                    for doc in self.docs.keys().filter(|d| key.is_about(d)) {
                        all.entry(doc.clone()).or_default().push(lsp_types::Diagnostic {
                            severity: Some(lsp_types::DiagnosticSeverity::ERROR),
                            source: Some("nest-lsp".to_string()),
                            message: why.clone(),
                            ..Default::default()
                        });
                    }
                }
            }
        }
        for diags in all.values_mut() {
            diags.dedup();
        }
        let mut gone: Vec<PathBuf> = self.published.keys().filter(|p| !all.contains_key(*p)).cloned().collect();
        gone.sort();
        for path in gone {
            self.publish_one(&path, Vec::new());
        }
        let mut files: Vec<(&PathBuf, &Vec<lsp_types::Diagnostic>)> = all.iter().collect();
        files.sort_by(|a, b| a.0.cmp(b.0));
        for (path, diags) in files {
            if self.published.get(path) != Some(diags) {
                self.publish_one(path, diags.clone());
            }
        }
        self.published = all;
    }

    fn publish_one(&self, path: &Path, diagnostics: Vec<lsp_types::Diagnostic>) {
        let Some(uri) = path_to_uri(path) else { return };
        let params = PublishDiagnosticsParams { uri, diagnostics, version: None };
        self.send(Notification::new(PublishDiagnostics::METHOD.to_string(), params).into());
    }

    fn send(&self, msg: Message) {
        let _ = self.sender.send(msg);
    }
}

#[cfg(test)]
mod tests;
