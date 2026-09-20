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
//! ### Analyzing on a thread
//!
//! Analyzing a unit takes long enough to be felt between two keys, so it runs on
//! a thread of its own, one unit at a time, and the loop goes on answering. A
//! unit keeps what it found until the next analysis of it is done: completion
//! answers from that as a key is typed, while hover and go-to-definition wait
//! for an analysis of the text as it is. A unit is stale when a file it read
//! changed since, which each file's version says, or when its workspace was
//! prepared again since, which the workspace's generation says.
//!
//! ### Preparing a workspace
//!
//! twig runs on a thread of its own, because building a dependency can take
//! seconds; the loop carries on and analyzes the workspace when the answer
//! arrives. A saved file may be a dependency's, so a save prepares every
//! workspace again, which costs a check per library when nothing changed.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, select};
use lsp_server::{Connection, ErrorCode, Message, Notification, Request, RequestId, Response};
use lsp_types::notification::{
    Cancel, DidChangeTextDocument, DidChangeWatchedFiles, DidCloseTextDocument,
    DidOpenTextDocument, DidSaveTextDocument, Exit, Notification as _, PublishDiagnostics,
};
use lsp_types::request::{
    Completion, GotoDefinition, HoverRequest, RegisterCapability, Request as _,
};
use lsp_types::{
    CancelParams, CompletionList, CompletionOptions, CompletionParams, CompletionResponse,
    DidChangeTextDocumentParams, DidChangeWatchedFilesParams,
    DidChangeWatchedFilesRegistrationOptions, DidCloseTextDocumentParams,
    DidOpenTextDocumentParams, DidSaveTextDocumentParams, FileChangeType, FileSystemWatcher,
    GlobPattern, GotoDefinitionParams, GotoDefinitionResponse, Hover, HoverContents, HoverParams,
    HoverProviderCapability, Location, MarkupContent, MarkupKind, NumberOrString, OneOf, Position,
    PublishDiagnosticsParams, Range, Registration, RegistrationParams, SaveOptions,
    ServerCapabilities, TextDocumentSyncCapability, TextDocumentSyncKind, TextDocumentSyncOptions,
    TextDocumentSyncSaveOptions, Uri,
};

use crate::analysis::{self, Outcome, path_to_uri, uri_to_path};
use crate::log;
use crate::workspace::{self, Metadata, Toolchain};
use crate::{complete, ide};

/// Run the server over `conn` until the client says to exit. `toolchain` makes
/// one from the `initializationOptions`.
pub fn run(
    conn: &Connection,
    toolchain: impl FnOnce(&serde_json::Value) -> Arc<dyn Toolchain>,
) -> Result<(), String> {
    log::open();
    let capabilities = ServerCapabilities {
        text_document_sync: Some(TextDocumentSyncCapability::Options(
            TextDocumentSyncOptions {
                open_close: Some(true),
                change: Some(TextDocumentSyncKind::FULL),
                save: Some(TextDocumentSyncSaveOptions::SaveOptions(SaveOptions {
                    include_text: Some(false),
                })),
                ..Default::default()
            },
        )),
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        definition_provider: Some(OneOf::Left(true)),
        completion_provider: Some(CompletionOptions {
            trigger_characters: Some(vec![".".to_string()]),
            ..Default::default()
        }),
        ..Default::default()
    };
    let params = conn
        .initialize(serde_json::to_value(capabilities).expect("capabilities serialize"))
        .map_err(|e| e.to_string())?;
    let options = params
        .get("initializationOptions")
        .cloned()
        .unwrap_or_default();
    // Files change on disk without the editor having them open — a checkout, a
    // formatter, another editor — so the client is asked to say when, where it
    // can be asked.
    if params.pointer("/capabilities/workspace/didChangeWatchedFiles/dynamicRegistration")
        == Some(&serde_json::Value::Bool(true))
    {
        let watchers: Vec<FileSystemWatcher> = ["**/*.nest", "**/nest.toml"]
            .iter()
            .map(|glob| FileSystemWatcher {
                glob_pattern: GlobPattern::String(glob.to_string()),
                kind: None,
            })
            .collect();
        let registration = Registration {
            id: "nest-files".to_string(),
            method: DidChangeWatchedFiles::METHOD.to_string(),
            register_options: Some(
                serde_json::to_value(DidChangeWatchedFilesRegistrationOptions { watchers })
                    .expect("options serialize"),
            ),
        };
        let request = Request::new(
            RequestId::from("nest-files".to_string()),
            RegisterCapability::METHOD.to_string(),
            RegistrationParams {
                registrations: vec![registration],
            },
        );
        let _ = conn.sender.send(request.into());
    }

    let (prepared_tx, prepared) = crossbeam_channel::unbounded();
    let (jobs, jobs_rx) = crossbeam_channel::unbounded::<Job>();
    let (analyzed_tx, analyzed) = crossbeam_channel::unbounded();
    std::thread::spawn(move || {
        for job in jobs_rx {
            let outcome = analysis::analyze(&job.key.args, job.buffers.clone());
            if analyzed_tx.send((job, outcome)).is_err() {
                break;
            }
        }
    });
    // Whether the editor understands a snippet, which is what lets a chosen
    // function be written with its parentheses and the cursor between them.
    let snippets = params
        .pointer("/capabilities/textDocument/completion/completionItem/snippetSupport")
        == Some(&serde_json::Value::Bool(true));
    let mut server = Server {
        sender: conn.sender.clone(),
        snippets,
        toolchain: toolchain(&options),
        docs: HashMap::new(),
        versions: HashMap::new(),
        edits: HashMap::new(),
        workspaces: HashMap::new(),
        prepared_tx,
        prepared_rx: prepared.clone(),
        units: HashMap::new(),
        jobs,
        analyzed,
        busy: false,
        published: HashMap::new(),
        touched: HashMap::new(),
    };
    loop {
        let mut batch = Vec::new();
        // A document that was typed in a moment ago is analyzed once it is
        // quiet, and nothing else may arrive to notice that it is.
        let idle = server.hot().unwrap_or(Duration::from_secs(3600));
        select! {
            recv(conn.receiver) -> msg => {
                let Ok(msg) = msg else { return Ok(()) };
                batch.push(msg);
            }
            default(idle) => {}
            recv(prepared) -> done => {
                let (root, result) = done.expect("the server holds a sender");
                server.prepared(root, result);
            }
            recv(server.analyzed) -> done => {
                let (job, outcome) = done.expect("the analyzing thread runs while the server does");
                server.finished(job, outcome);
            }
        }
        // Everything arriving within a moment is taken before analyzing, so
        // that a burst of edits is analyzed once. The client's messages come
        // through a channel with no room in it, so nothing is ever already
        // waiting; a moment of quiet is how a burst ends.
        loop {
            if let Ok((root, result)) = prepared.try_recv() {
                server.prepared(root, result);
                continue;
            }
            if let Ok((job, outcome)) = server.analyzed.try_recv() {
                server.finished(job, outcome);
                continue;
            }
            match conn.receiver.recv_timeout(QUIET) {
                Ok(msg) => batch.push(msg),
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => return Ok(()),
            }
        }
        // A request the client gave up on while it waited is not answered.
        let cancelled: HashSet<RequestId> = batch
            .iter()
            .filter_map(|msg| match msg {
                Message::Notification(n) if n.method == Cancel::METHOD => {
                    let p = serde_json::from_value::<CancelParams>(n.params.clone()).ok()?;
                    Some(match p.id {
                        NumberOrString::Number(n) => RequestId::from(n),
                        NumberOrString::String(s) => RequestId::from(s),
                    })
                }
                _ => None,
            })
            .collect();
        for msg in batch {
            if let Message::Request(req) = &msg
                && cancelled.contains(&req.id)
            {
                let response = Response::new_err(
                    req.id.clone(),
                    ErrorCode::RequestCanceled as i32,
                    "cancelled".to_string(),
                );
                server.send(response.into());
                continue;
            }
            if server.handle(conn, msg)? == Flow::Exit {
                return Ok(());
            }
        }
        server.analyze();
    }
}

/// How many edits of a document are remembered.
const EDITS: usize = 256;

/// How long the client is quiet before what it sent is acted on.
const QUIET: Duration = Duration::from_millis(15);

/// How long a document is quiet before it is analyzed.
///
/// Analyzing what is half typed costs an analysis and produces diagnostics about
/// text the person is in the middle of writing — "no field `d` on `*mut T`"
/// while `decode` is being typed. Completion does not wait for any of it
/// (`from_analysis`), so the only thing this delays is the diagnostics, which
/// are worth having only once a thought is finished.
const IDLE: Duration = Duration::from_millis(400);

/// A unit to analyze, with the text it is analyzed over and what that text was.
struct Job {
    key: UnitKey,
    buffers: analysis::Buffers,
    /// Each file's version when the job was made.
    versions: HashMap<PathBuf, u64>,
    /// Its workspace's generation, when it has one.
    generation: u64,
}

/// What a unit found, and what it was found over.
struct Unit {
    outcome: Result<Outcome, String>,
    /// The last outcome before it in which every open document parsed, while
    /// this one's did not, and the versions it was found over: what completion
    /// asks while a line is half typed.
    parsed: Option<(Outcome, HashMap<PathBuf, u64>)>,
    versions: HashMap<PathBuf, u64>,
    generation: u64,
}

/// A unit named as the file it is rooted at, which is what a log is read by.
fn unit_name(key: &UnitKey) -> String {
    let entry = key.args.first().map(String::as_str).unwrap_or("?");
    Path::new(entry)
        .file_name()
        .map_or(entry.to_string(), |n| n.to_string_lossy().into_owned())
}

/// A message's parameters as one short line: what it is about, not all of it. A
/// `didChange` carries the whole document, which is not what a log is for.
fn brief(params: &serde_json::Value) -> String {
    let file = params
        .pointer("/textDocument/uri")
        .and_then(|u| u.as_str())
        .and_then(|u| u.rsplit('/').next())
        .unwrap_or("");
    let version = params
        .pointer("/textDocument/version")
        .and_then(|v| v.as_u64());
    let line = params.pointer("/position/line").and_then(|v| v.as_u64());
    let column = params
        .pointer("/position/character")
        .and_then(|v| v.as_u64());
    let mut out = file.to_string();
    if let Some(v) = version {
        out.push_str(&format!(" v{v}"));
    }
    if let (Some(l), Some(c)) = (line, column) {
        out.push_str(&format!(" at {}:{}", l + 1, c + 1));
    }
    out
}

/// What completion writes at the cursor before analyzing: a name nothing
/// declares.
const PLACEHOLDER: &str = "__nest_lsp_complete";

fn to_value<T: serde::Serialize>(v: Option<T>) -> serde_json::Value {
    serde_json::to_value(v).expect("a response serializes")
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
    /// How many answers it has had, which the units analyzed with the last one
    /// remember.
    generation: u64,
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
    /// Whether the editor understands snippets (see where it is read).
    snippets: bool,
    toolchain: Arc<dyn Toolchain>,
    /// The open documents' text.
    docs: HashMap<PathBuf, String>,
    /// How many times each file changed, open or on disk, which says whether a
    /// unit that read it is out of date.
    versions: HashMap<PathBuf, u64>,
    /// The last edits of each open document, by the version each made.
    edits: HashMap<PathBuf, Vec<(u64, complete::Edit)>>,
    workspaces: HashMap<PathBuf, Workspace>,
    prepared_tx: Sender<(PathBuf, Result<Metadata, String>)>,
    /// The same answers the main loop waits for, so that a question asked
    /// before a workspace is ready can wait for it too.
    prepared_rx: Receiver<(PathBuf, Result<Metadata, String>)>,
    /// What each unit last found, or why it could not be analyzed.
    units: HashMap<UnitKey, Unit>,
    jobs: Sender<Job>,
    analyzed: Receiver<(Job, Result<Outcome, String>)>,
    /// Whether the analyzing thread has a job.
    busy: bool,
    /// What was last published, by file.
    published: HashMap<PathBuf, Vec<lsp_types::Diagnostic>>,
    /// When each open document was last edited, so that analysis waits for the
    /// typing to stop.
    touched: HashMap<PathBuf, Instant>,
}

impl Server {
    fn handle(&mut self, conn: &Connection, msg: Message) -> Result<Flow, String> {
        match &msg {
            Message::Request(r) => log::line!("< {} #{} {}", r.method, r.id, brief(&r.params)),
            Message::Notification(n) => log::line!("< {} {}", n.method, brief(&n.params)),
            Message::Response(_) => {}
        }
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
        // A question is about the text as it is now, so it waits for the
        // analysis of it. Completion is asked as a key is typed, and answers from
        // the analysis before that key, unless the file is in none yet.
        let path = req
            .params
            .pointer("/textDocument/uri")
            .and_then(|u| u.as_str()?.parse::<Uri>().ok())
            .and_then(|u| uri_to_path(&u));
        if req.method != Completion::METHOD || path.is_none_or(|p| !self.covers(&p)) {
            self.settle();
        }
        let result = match req.method.as_str() {
            HoverRequest::METHOD => serde_json::from_value::<HoverParams>(req.params).map(|p| {
                to_value(self.hover(
                    &p.text_document_position_params.text_document.uri,
                    p.text_document_position_params.position,
                ))
            }),
            GotoDefinition::METHOD => serde_json::from_value::<GotoDefinitionParams>(req.params)
                .map(|p| {
                    to_value(self.definition(
                        &p.text_document_position_params.text_document.uri,
                        p.text_document_position_params.position,
                    ))
                }),
            Completion::METHOD => serde_json::from_value::<CompletionParams>(req.params).map(|p| {
                to_value(self.completion(
                    &p.text_document_position.text_document.uri,
                    p.text_document_position.position,
                ))
            }),
            _ => {
                let response = Response::new_err(
                    req.id,
                    ErrorCode::MethodNotFound as i32,
                    format!("`{}` is not supported", req.method),
                );
                self.send(response.into());
                return;
            }
        };
        let response = match result {
            Ok(value) => Response::new_ok(req.id, value),
            Err(e) => Response::new_err(req.id, ErrorCode::InvalidParams as i32, e.to_string()),
        };
        self.send(response.into());
    }

    /// The unit that read `path`, and the file it is there.
    fn unit_for(&self, path: &Path) -> Option<(&UnitKey, &Outcome, nestc::common::source::FileId)> {
        self.units.iter().find_map(|(key, unit)| {
            let o = unit.outcome.as_ref().ok()?;
            let file = ide::file_of(&o.session, path)?;
            Some((key, o, file))
        })
    }

    /// What completion in `path` asks: the unit that read it, and its last
    /// outcome in which `path` parsed.
    fn parsed_for(
        &self,
        path: &Path,
    ) -> Option<(&Outcome, nestc::common::source::FileId, complete::Edits)> {
        self.units.values().find_map(|unit| {
            let latest = unit.outcome.as_ref().ok()?;
            ide::file_of(&latest.session, path)?;
            let (o, versions) = match latest.parsed.contains(path) {
                true => (latest, &unit.versions),
                false => unit.parsed.as_ref().map(|(o, v)| (o, v))?,
            };
            let edits = self.edits_since(path, versions.get(path).copied()?)?;
            Some((o, ide::file_of(&o.session, path)?, edits))
        })
    }

    /// The edits that made `path`'s version `since` what the editor has now,
    /// when every one of them is still remembered.
    fn edits_since(&self, path: &Path, since: u64) -> Option<complete::Edits> {
        let now = self.versions.get(path).copied()?;
        let edits: Vec<complete::Edit> = self
            .edits
            .get(path)?
            .iter()
            .filter(|(v, _)| *v > since)
            .map(|(_, e)| *e)
            .collect();
        (edits.len() as u64 == now - since).then_some(complete::Edits(edits))
    }

    fn hover(&self, uri: &Uri, position: Position) -> Option<Hover> {
        let path = uri_to_path(uri)?;
        let (_, o, file) = self.unit_for(&path)?;
        let src = ide::source(&o.session, file)?;
        let offset = analysis::offset(&src, position);
        // A tuple's member is a position and not a definition, so it is asked
        // for separately — `find` has nothing to return for one.
        let (value, span) = match ide::find(&o.session, file, offset) {
            Some(found) => (ide::hover(&o.session, file, found), found.span),
            None => ide::tuple_member(&o.session, file, offset)?,
        };
        Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value,
            }),
            range: Some(analysis::range(&src, span.start, span.end)),
        })
    }

    fn definition(&self, uri: &Uri, position: Position) -> Option<GotoDefinitionResponse> {
        let path = uri_to_path(uri)?;
        let (_, o, file) = self.unit_for(&path)?;
        let s = &o.session;
        let src = ide::source(s, file)?;
        let found = ide::find(s, file, analysis::offset(&src, position))?;
        let def = ide::target(s, found.def);
        let d = s.defs.get(def);
        let there = d.file?;
        let text = ide::source(s, there)?;
        let range = match ide::name_span(s, def).or(d.span) {
            Some(span) => analysis::range(&text, span.start, span.end),
            // A whole file, which has nowhere in particular to point.
            None => Range::default(),
        };
        let uri = path_to_uri(Path::new(&s.sources.file(there)?.name))?;
        Some(GotoDefinitionResponse::Scalar(Location::new(uri, range)))
    }

    fn completion(&self, uri: &Uri, position: Position) -> Option<CompletionResponse> {
        let path = uri_to_path(uri)?;
        let Some((key, _, _)) = self.unit_for(&path) else {
            log::line!("  completion: nothing has analyzed this file yet");
            return None;
        };
        let text = self.docs.get(&path)?;
        let offset = analysis::offset(text, position);
        // Incomplete, because what an import would bring in is offered by what
        // is typed so far.
        let list = |items| {
            Some(CompletionResponse::List(CompletionList {
                is_incomplete: true,
                items,
            }))
        };
        if let Some((o, file, edits)) = self.parsed_for(&path)
            && let Some(items) =
                complete::from_analysis(&o.session, file, text, offset, &edits, self.snippets)
        {
            log::line!(
                "  completion: {} items from the analysis already made",
                items.len()
            );
            return list(items);
        }
        log::line!("  completion: analyzing again, with a name written at the cursor");
        // Analyzed again with a name written where the cursor is, so that
        // `p.` is a member access rather than a syntax error, and the tree
        // says what `p` is.
        let mut buffers = self.docs.clone();
        let mut written = text.clone();
        written.insert_str(offset, PLACEHOLDER);
        buffers.insert(path.clone(), written);
        let o = analysis::analyze(&key.args, Arc::new(buffers)).ok()?;
        let file = ide::file_of(&o.session, &path)?;
        let items = complete::complete(&o.session, file, text, offset, PLACEHOLDER, self.snippets);
        log::line!("  completion: {} items from that analysis", items.len());
        list(items)
    }

    fn notification(&mut self, n: Notification) {
        match n.method.as_str() {
            DidOpenTextDocument::METHOD => {
                let Ok(p) = serde_json::from_value::<DidOpenTextDocumentParams>(n.params) else {
                    return;
                };
                self.set(&p.text_document.uri, Some(p.text_document.text));
            }
            DidChangeTextDocument::METHOD => {
                let Ok(mut p) = serde_json::from_value::<DidChangeTextDocumentParams>(n.params)
                else {
                    return;
                };
                // Full synchronization: the last change is the whole text.
                if let Some(change) = p.content_changes.pop() {
                    self.set(&p.text_document.uri, Some(change.text));
                }
            }
            DidCloseTextDocument::METHOD => {
                let Ok(p) = serde_json::from_value::<DidCloseTextDocumentParams>(n.params) else {
                    return;
                };
                self.set(&p.text_document.uri, None);
            }
            DidChangeWatchedFiles::METHOD => {
                let Ok(p) = serde_json::from_value::<DidChangeWatchedFilesParams>(n.params) else {
                    return;
                };
                self.changed_on_disk(p);
            }
            DidSaveTextDocument::METHOD => {
                let Ok(_) = serde_json::from_value::<DidSaveTextDocumentParams>(n.params) else {
                    return;
                };
                let roots: Vec<PathBuf> = self.workspaces.keys().cloned().collect();
                for root in roots {
                    self.prepare(&root);
                }
            }
            _ => {}
        }
    }

    /// Files changed on disk. An open document is the editor's, and a save of it
    /// says so on its own; any other file may be one a unit read, or one a
    /// dependency's library was built from, so the units that read it are
    /// analyzed again and every workspace is prepared again. A file created or
    /// deleted may change what an import finds, so every unit of its
    /// workspace goes.
    fn changed_on_disk(&mut self, p: DidChangeWatchedFilesParams) {
        let mut any = false;
        for change in p.changes {
            let Some(path) = uri_to_path(&change.uri) else {
                continue;
            };
            if self.docs.contains_key(&path) {
                continue;
            }
            any = true;
            if change.typ != FileChangeType::CHANGED {
                self.units
                    .retain(|key, _| !key.root.as_ref().is_some_and(|root| path.starts_with(root)));
            }
            *self.versions.entry(path).or_default() += 1;
        }
        if any {
            let roots: Vec<PathBuf> = self.workspaces.keys().cloned().collect();
            for root in roots {
                self.prepare(&root);
            }
        }
    }

    /// The document at `uri` is now `text`, or closed.
    fn set(&mut self, uri: &Uri, text: Option<String>) {
        let Some(path) = uri_to_path(uri) else { return };
        let version = self.versions.entry(path.clone()).or_default();
        *version += 1;
        let version = *version;
        match text {
            Some(text) => {
                let edits = self.edits.entry(path.clone()).or_default();
                match self.docs.get(&path) {
                    Some(old) => edits.push((version, complete::Edit::between(old, &text))),
                    None => edits.clear(),
                }
                if edits.len() > EDITS {
                    edits.drain(..edits.len() - EDITS);
                }
                self.touched.insert(path.clone(), Instant::now());
                self.docs.insert(path, text);
            }
            // A closed document is read from disk again, which may be different.
            None => {
                self.docs.remove(&path);
                self.edits.remove(&path);
                self.touched.remove(&path);
            }
        }
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
        log::line!("  preparing {} with twig", root.display());
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
        // The generation is what makes every unit of the workspace stale, so it
        // moves only when the answer is a different one. A save prepares the
        // workspace again, and the usual answer is the one it already had:
        // nothing about how the files are compiled changed, and analyzing them
        // again would find what is already known.
        let changed = ws.meta.as_ref() != Some(&result);
        ws.meta = Some(result);
        ws.running = false;
        if changed {
            ws.generation += 1;
        }
        if std::mem::take(&mut ws.again) {
            self.prepare(&root);
        }
    }

    /// What `root`'s workspace was last prepared as, and how many times it was.
    fn generation(&self, root: Option<&PathBuf>) -> u64 {
        root.and_then(|r| self.workspaces.get(r))
            .map_or(0, |ws| ws.generation)
    }

    /// Whether `unit`, analyzed as `key`, was analyzed over text that changed
    /// since, or with a workspace prepared again since.
    fn is_stale(&self, key: &UnitKey, unit: &Unit) -> bool {
        if unit.generation != self.generation(key.root.as_ref()) {
            return true;
        }
        let Ok(o) = &unit.outcome else { return false };
        o.files
            .iter()
            .any(|f| self.versions.get(f) != unit.versions.get(f))
    }

    /// Analyze whatever is out of date and wait for it, publishing as it goes.
    ///
    /// A question about the text as it is now cannot wait for the typing to
    /// stop, so this is the one path that analyzes a document that was just
    /// edited.
    fn settle(&mut self) {
        log::line!("  waiting for what is being analyzed");
        self.touched.clear();
        self.analyze();
        loop {
            if self.busy {
                let (job, outcome) = self
                    .analyzed
                    .recv()
                    .expect("the analyzing thread runs while the server does");
                self.finished(job, outcome);
                continue;
            }
            // Nothing can be analyzed until twig has said how, so a question
            // asked in the first moments of a session waits for that rather
            // than answering nothing.
            if self.workspaces.values().any(|ws| ws.running) {
                log::line!("  waiting for twig");
                let (root, result) = self.prepared_rx.recv().expect("the server holds a sender");
                self.prepared(root, result);
                self.analyze();
                continue;
            }
            log::line!("  nothing left to wait for");
            return;
        }
    }

    /// An analysis is done.
    fn finished(&mut self, job: Job, outcome: Result<Outcome, String>) {
        self.busy = false;
        match &outcome {
            Ok(o) => log::line!(
                "  analyzed {}: {} files, {} with diagnostics",
                unit_name(&job.key),
                o.files.len(),
                o.diagnostics.len()
            ),
            Err(why) => log::line!("  analyzing {} failed: {why}", unit_name(&job.key)),
        }
        let clean = |o: &Result<Outcome, String>| {
            o.as_ref().is_ok_and(|o| {
                o.files
                    .iter()
                    .filter(|f| self.docs.contains_key(*f))
                    .all(|f| o.parsed.contains(f))
            })
        };
        let parsed = match self.units.remove(&job.key) {
            _ if clean(&outcome) => None,
            Some(old) if clean(&old.outcome) => old.outcome.ok().map(|o| (o, old.versions)),
            Some(old) => old.parsed,
            None => None,
        };
        let unit = Unit {
            outcome,
            parsed,
            versions: job.versions,
            generation: job.generation,
        };
        self.units.insert(job.key, unit);
        self.analyze();
    }

    /// Start analyzing what is out of date, when nothing is being analyzed, and
    /// publish what changed.
    fn analyze(&mut self) {
        let docs = &self.docs;
        self.units.retain(|key, unit| match &unit.outcome {
            Ok(o) => o.files.iter().any(|f| docs.contains_key(f)),
            // An error is about the setup; it goes when no open document is in
            // its workspace any more.
            Err(_) => docs.keys().any(|d| key.is_about(d)),
        });
        if !self.busy
            && self.hot().is_none()
            && let Some(key) = self.next()
        {
            let versions = self.versions.clone();
            let generation = self.generation(key.root.as_ref());
            log::line!("  analyzing {}", unit_name(&key));
            let job = Job {
                key,
                buffers: Arc::new(self.docs.clone()),
                versions,
                generation,
            };
            self.busy = self.jobs.send(job).is_ok();
        }
        self.publish();
    }

    /// The unit to analyze next: one an open document needs and no unit read
    /// yet, then one that is stale. A stale unit its workspace no longer has
    /// goes, and a setup error is found out here rather than analyzed.
    fn next(&mut self) -> Option<UnitKey> {
        let mut paths: Vec<PathBuf> = self.docs.keys().cloned().collect();
        paths.sort();
        for path in &paths {
            if self.covers(path) {
                continue;
            }
            let Some(root) = workspace::find_root(path) else {
                let key = UnitKey {
                    root: None,
                    args: vec![path.display().to_string()],
                };
                if self.units.contains_key(&key) {
                    continue;
                }
                return Some(key);
            };
            let meta = match self.workspaces.get(&root).and_then(|ws| ws.meta.as_ref()) {
                None => {
                    if !self.workspaces.get(&root).is_some_and(|ws| ws.running) {
                        self.prepare(&root);
                    }
                    continue;
                }
                Some(Err(why)) => {
                    let key = UnitKey {
                        root: Some(root.clone()),
                        args: Vec::new(),
                    };
                    let unit = Unit {
                        outcome: Err(why.clone()),
                        parsed: None,
                        versions: HashMap::new(),
                        generation: self.generation(Some(&root)),
                    };
                    self.units.insert(key, unit);
                    continue;
                }
                Some(Ok(meta)) => meta.clone(),
            };
            // The first of its candidates not analyzed yet; one that was, and
            // did not read it, is not the one.
            for args in workspace::candidates(&meta, path) {
                let key = UnitKey {
                    root: Some(root.clone()),
                    args,
                };
                if !self.units.contains_key(&key) {
                    return Some(key);
                }
            }
        }

        let mut stale: Vec<UnitKey> = self
            .units
            .iter()
            .filter(|(k, u)| self.is_stale(k, u))
            .map(|(k, _)| k.clone())
            .collect();
        stale.sort_by(|a, b| (&a.root, &a.args).cmp(&(&b.root, &b.args)));
        for key in stale {
            let Some(root) = &key.root else {
                return Some(key);
            };
            let still = match self.workspaces.get(root).and_then(|ws| ws.meta.as_ref()) {
                // Being prepared: it is analyzed once that is done.
                None => continue,
                Some(Err(_)) => false,
                Some(Ok(meta)) => match &self.units[&key].outcome {
                    Ok(o) => paths.iter().any(|p| {
                        o.files.contains(p) && workspace::candidates(meta, p).contains(&key.args)
                    }),
                    Err(_) => false,
                },
            };
            if still {
                return Some(key);
            }
            // What replaces it is found on the next look.
            self.units.remove(&key);
            return self.next();
        }
        None
    }

    /// How long until the document being typed in is quiet, when one is.
    ///
    /// Analysis waits for it. Everything else — completion, hover — reads what
    /// is already analyzed and does not.
    fn hot(&self) -> Option<Duration> {
        self.touched
            .values()
            .map(|at| at.elapsed())
            .filter(|since| *since < IDLE)
            .map(|since| IDLE - since)
            .max()
    }

    /// Whether some unit already read `path`.
    fn covers(&self, path: &Path) -> bool {
        self.units
            .values()
            .any(|u| u.outcome.as_ref().is_ok_and(|o| o.files.contains(path)))
    }

    /// Publish every file's diagnostics that changed, and clear the ones that
    /// have none now.
    fn publish(&mut self) {
        let mut all: HashMap<PathBuf, Vec<lsp_types::Diagnostic>> = HashMap::new();
        for (key, unit) in &self.units {
            match &unit.outcome {
                Ok(o) => {
                    for (path, diags) in &o.diagnostics {
                        all.entry(path.clone())
                            .or_default()
                            .extend(diags.iter().cloned());
                    }
                }
                // No file to pin it to, so it goes on every open document it
                // is about, at the top.
                Err(why) => {
                    for doc in self.docs.keys().filter(|d| key.is_about(d)) {
                        all.entry(doc.clone())
                            .or_default()
                            .push(lsp_types::Diagnostic {
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
        let mut gone: Vec<PathBuf> = self
            .published
            .keys()
            .filter(|p| !all.contains_key(*p))
            .cloned()
            .collect();
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
        log::line!(
            "> diagnostics {} ({})",
            path.file_name().map_or_else(
                || path.display().to_string(),
                |n| n.to_string_lossy().into_owned()
            ),
            diagnostics.len()
        );
        let params = PublishDiagnosticsParams {
            uri,
            diagnostics,
            version: None,
        };
        self.send(Notification::new(PublishDiagnostics::METHOD.to_string(), params).into());
    }

    fn send(&self, msg: Message) {
        let _ = self.sender.send(msg);
    }
}

#[cfg(test)]
mod tests;
