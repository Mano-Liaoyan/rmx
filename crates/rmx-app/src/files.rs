//! A file pane: two listings and a queue of copies, with everything that waits off the main thread.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use bytes::Bytes;
use rmx_layout::PaneId;
use rmx_transfer::{
    DirStream, Entry, EntryKind, Error as FsError, ListOptions, LocalFs, OverwritePolicy,
    ProgressUpdate, QueueConfig, RemoteFs, TransferId, TransferQueue, TransferRequest,
    TransferState, WriteStart, read_all,
};
#[cfg(feature = "bench")]
use rmx_ui::egui;
use rmx_ui::widgets::{
    DiffView, FileColumnView, FileKind, FileRow, FilesBody, FilesView, Highlight, TextView,
    TransferRow, TransferRowState,
};
use rmx_ui::{FileSide, FilesAction};
use tokio::runtime::Handle;
use tokio::task::AbortHandle;
use winit::event_loop::EventLoopProxy;

use crate::app::AppEvent;
use crate::compare::{Comparison, compare};
use crate::highlight::highlight;

/// The longest file that can be edited: the field lays all of it out at every keystroke.
const EDIT_LIMIT: usize = 512 * 1024;
/// The longest file that is shown at all, line by line.
const OPEN_LIMIT: u64 = 64 * 1024 * 1024;
/// How much is handed to a writer at a time.
const WRITE_CHUNK: usize = 64 * 1024;
/// How much of the start of a file is looked at for telling text from anything else.
const SNIFF: usize = 8192;
const NOT_TEXT: &str = "This is not a UTF-8 text file";

/// Tells every opening of a file and comparison from the others, in every pane.
static NEXT_BODY: AtomicU64 = AtomicU64::new(1);

fn next_body() -> u64 {
    NEXT_BODY.fetch_add(1, Ordering::Relaxed)
}

/// How often a growing listing is shown again.
const PUBLISH_EVERY: Duration = Duration::from_millis(100);
/// The policies of [`rmx_ui::widgets::CONFLICT_POLICIES`], in its order.
const POLICIES: [OverwritePolicy; 5] = [
    OverwritePolicy::Skip,
    OverwritePolicy::Overwrite,
    OverwritePolicy::IfNewer,
    OverwritePolicy::Resume,
    OverwritePolicy::Fail,
];

/// What a session gives the pane once it is up.
pub struct FilesLink {
    pub fs: Arc<dyn RemoteFs>,
    /// The directory the remote column opens at.
    pub start: String,
    pub title: String,
}

/// News from the tasks behind a file pane.
pub enum FilesEvent {
    Listing {
        side: FileSide,
        generation: u64,
        rows: Arc<Vec<FileRow>>,
        done: bool,
    },
    ListingFailed {
        side: FileSide,
        generation: u64,
        reason: String,
    },
    /// A link turned out to lead to a directory, or not.
    Link {
        side: FileSide,
        path: String,
        directory: bool,
    },
    Progress(Box<ProgressUpdate>),
    /// Making, renaming or removing something in the column is over.
    Changed {
        side: FileSide,
        directory: String,
        error: Option<String>,
    },
    /// A file asked for has been read, or could not be.
    Opened {
        token: u64,
        side: FileSide,
        result: Result<Box<TextFile>, String>,
    },
    /// Two texts asked for have been compared, or could not be.
    Compared {
        token: u64,
        side: FileSide,
        result: Result<Box<Compared>, String>,
    },
    Saved {
        document: u64,
        result: Result<Saved, String>,
    },
    /// The colors of the open file, as of the latest request.
    Highlighted {
        document: u64,
        token: u64,
        highlight: Option<Highlight>,
    },
    /// The file has changed since it was read: this is what was to be written over it.
    SaveConflict {
        document: u64,
        text: String,
    },
}

/// What tells whether a file has changed since it was last looked at.
type Stamp = (Option<u64>, Option<SystemTime>);

fn stamp_of(entry: &Entry) -> Stamp {
    (entry.size, entry.modified)
}

/// A file that has just been written.
pub struct Saved {
    text: Arc<str>,
    stamp: Stamp,
}

/// A text file, as what is shown and how to write it back as it was.
pub struct TextFile {
    id: u64,
    revision: u64,
    side: FileSide,
    title: String,
    path: String,
    /// With `\n` for line endings, if the whole file was consistent about them.
    text: Arc<str>,
    /// Where lines start, in a file too long to edit.
    lines: Option<Arc<Vec<u32>>>,
    crlf: bool,
    bom: bool,
    stamp: Stamp,
    saving: bool,
    error: Option<String>,
    /// What is waiting to be written once the user has said to overwrite.
    held: Option<String>,
    highlight: Option<Highlight>,
    /// Goes up with every new `highlight`.
    highlight_id: u64,
}

/// Two texts lined up.
pub struct Compared {
    id: u64,
    titles: [String; 2],
    texts: [Arc<str>; 2],
    found: Comparison,
    note: Option<&'static str>,
    highlights: [Option<Highlight>; 2],
}

/// The bytes of a file as text. `None` if they are not.
fn decode(data: Vec<u8>) -> Option<(String, bool, bool)> {
    if data[..data.len().min(SNIFF)].contains(&0) {
        return None;
    }
    const BOM: &[u8] = b"\xEF\xBB\xBF";
    let bom = data.starts_with(BOM);
    let mut text = String::from_utf8(data).ok()?;
    if bom {
        text.drain(..BOM.len());
    }
    let breaks = text.bytes().filter(|byte| *byte == b'\n').count();
    let crlf = breaks > 0 && text.matches("\r\n").count() == breaks;
    if crlf {
        text = text.replace("\r\n", "\n");
    }
    Some((text, crlf, bom))
}

/// What goes in the file for `text`.
fn encode(text: &str, crlf: bool, bom: bool) -> Vec<u8> {
    let body = if crlf { text.replace('\n', "\r\n") } else { text.to_owned() };
    let mut data = Vec::with_capacity(body.len() + 3);
    if bom {
        data.extend_from_slice(b"\xEF\xBB\xBF");
    }
    data.extend_from_slice(body.as_bytes());
    data
}

/// Where each line of `text` starts.
fn line_starts(text: &str) -> Vec<u32> {
    let mut starts = vec![0];
    starts.extend(
        text.bytes().enumerate().filter(|(_, byte)| *byte == b'\n').map(|(at, _)| at as u32 + 1),
    );
    if text.ends_with('\n') {
        starts.pop();
    }
    starts
}

/// Read the text file at `path`, off the main thread.
async fn read_text(
    fs: Arc<dyn RemoteFs>,
    side: FileSide,
    path: String,
    title: String,
) -> Result<Box<TextFile>, String> {
    let entry = fs.stat(&path).await.map_err(|err| err.to_string())?;
    if entry.kind != EntryKind::File {
        return Err(NOT_TEXT.to_owned());
    }
    if entry.size.is_some_and(|size| size > OPEN_LIMIT) {
        return Err("This file is too large to open here".to_owned());
    }
    let data = read_all(fs.as_ref(), &path, 0, Some(OPEN_LIMIT + 1))
        .await
        .map_err(|err| err.to_string())?;
    if data.len() as u64 > OPEN_LIMIT {
        return Err("This file is too large to open here".to_owned());
    }
    let stamp = stamp_of(&entry);
    let (text, crlf, bom) = tokio::task::spawn_blocking(move || decode(data))
        .await
        .map_err(|err| err.to_string())?
        .ok_or_else(|| NOT_TEXT.to_owned())?;
    let lines = (text.len() > EDIT_LIMIT).then(|| Arc::new(line_starts(&text)));
    Ok(Box::new(TextFile {
        id: next_body(),
        revision: 0,
        side,
        title,
        path,
        text: text.into(),
        lines,
        crlf,
        bom,
        stamp,
        saving: false,
        error: None,
        held: None,
        highlight: None,
        highlight_id: 0,
    }))
}

/// Line two texts up, off the main thread.
async fn compared(
    names: [String; 2],
    titles: [String; 2],
    texts: [Arc<str>; 2],
    same_bytes: bool,
) -> Result<Box<Compared>, String> {
    tokio::task::spawn_blocking(move || {
        let highlights = [highlight(&names[0], &texts[0]), highlight(&names[1], &texts[1])];
        let found = compare(&texts[0], &texts[1]);
        let note = found.changes.is_empty().then_some(if same_bytes {
            "Identical"
        } else {
            "Only line endings differ"
        });
        Box::new(Compared { id: next_body(), titles, texts, found, note, highlights })
    })
    .await
    .map_err(|err| err.to_string())
}

/// Write `data` over the file at `path`.
async fn write_file(fs: &dyn RemoteFs, path: &str, data: Vec<u8>) -> Result<(), String> {
    let mut writer =
        fs.write_range(path, WriteStart::Truncate).await.map_err(|err| err.to_string())?;
    let data = Bytes::from(data);
    let mut at = 0;
    while at < data.len() {
        let end = (at + WRITE_CHUNK).min(data.len());
        writer.write(data.slice(at..end)).await.map_err(|err| err.to_string())?;
        at = end;
    }
    writer.finish().await.map_err(|err| err.to_string())
}

struct Column {
    title: String,
    path: String,
    fs: Arc<dyn RemoteFs>,
    rows: Arc<Vec<FileRow>>,
    busy: bool,
    error: Option<String>,
    /// Which listing is current: what earlier ones send is stale.
    generation: u64,
    listing: Option<AbortHandle>,
}

struct Transfer {
    id: TransferId,
    /// How the interface refers to it.
    number: u64,
    /// The column it goes to, and the directory there.
    to: FileSide,
    directory: String,
    row: TransferRow,
    finished: bool,
}

/// A pane browsing the local disk and a remote file system.
pub struct FilesPane {
    columns: [Column; 2],
    queue: TransferQueue,
    transfers: Vec<Transfer>,
    next_number: u64,
    policy: usize,
    /// Rows of the transfer queue, kept in the shape the interface draws.
    rows: Vec<TransferRow>,
    /// What is being fetched to show instead of the columns: which request, and what to call it.
    opening: Option<(u64, String)>,
    document: Option<Box<TextFile>>,
    /// Shown over `document`, if there is one, which it goes back to.
    diff: Option<Box<Compared>>,
    /// Which request for colors is the latest.
    highlight_token: u64,
    pane: PaneId,
    generation: u64,
    runtime: Handle,
    proxy: EventLoopProxy<AppEvent>,
}

fn row_of(entry: Entry) -> FileRow {
    let kind = match entry.kind {
        EntryKind::Dir => FileKind::Dir,
        EntryKind::File => FileKind::File,
        EntryKind::Symlink => FileKind::Link,
        EntryKind::Other => FileKind::Other,
    };
    FileRow::new(&entry.name, kind, entry.size, entry.modified)
}

/// Directories first, then by name without regard to case.
fn sort_rows(rows: &mut [FileRow]) {
    // Stable: what is already in order is a single run for it to merge the new batch into.
    rows.sort_by(|a, b| {
        (a.kind != FileKind::Dir, &a.sort, &a.name).cmp(&(
            b.kind != FileKind::Dir,
            &b.sort,
            &b.name,
        ))
    });
}

/// The parent of `path`, or itself at the root.
fn parent(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) | None => "/".to_owned(),
        Some(at) => trimmed[..at].to_owned(),
    }
}

/// `directory`, then `name`, with one slash between them.
fn join(directory: &str, name: &str) -> String {
    if directory.ends_with('/') {
        format!("{directory}{name}")
    } else {
        format!("{directory}/{name}")
    }
}

/// What to change in a directory of a column.
enum Change {
    NewFolder(String),
    Rename(String, String),
    Remove(Vec<(String, FileKind)>),
}

/// Remove `path`, a folder with all that is in it.
async fn remove_folder(fs: &dyn RemoteFs, path: String) -> Result<(), String> {
    let entries = fs.list(&path, ListOptions::default()).await.map_err(|err| err.to_string())?;
    let entries = entries.collect_all().await.map_err(|err| err.to_string())?;
    for entry in entries {
        let inside = join(&path, &entry.name);
        if entry.kind == EntryKind::Dir {
            Box::pin(remove_folder(fs, inside)).await?;
        } else {
            fs.remove_file(&inside).await.map_err(|err| err.to_string())?;
        }
    }
    fs.remove_dir(&path).await.map_err(|err| err.to_string())
}

/// Make the `change` in `directory`, off the main thread.
async fn apply_change(
    fs: Arc<dyn RemoteFs>,
    directory: &str,
    change: Change,
) -> Result<(), String> {
    match change {
        Change::NewFolder(name) => {
            fs.mkdir(&join(directory, &name)).await.map_err(|err| err.to_string())
        },
        Change::Rename(from, to) => fs
            .rename(&join(directory, &from), &join(directory, &to))
            .await
            .map_err(|err| err.to_string()),
        Change::Remove(names) => {
            for (name, kind) in names {
                let path = join(directory, &name);
                if kind == FileKind::Dir {
                    remove_folder(fs.as_ref(), path).await?;
                } else {
                    fs.remove_file(&path).await.map_err(|err| err.to_string())?;
                }
            }
            Ok(())
        },
    }
}

/// Read the rest of a listing, showing it as it grows. Everything here is off the main thread,
/// and sorting a large one off the runtime too.
async fn pump_listing(
    mut stream: DirStream,
    side: FileSide,
    generation: u64,
    pane: PaneId,
    tab: u64,
    proxy: EventLoopProxy<AppEvent>,
) {
    let send = |event| {
        let _ = proxy.send_event(AppEvent::Files(pane, tab, event));
    };
    let mut rows: Vec<FileRow> = Vec::new();
    let mut last = Instant::now();
    let mut shown = 0;
    loop {
        match stream.next_batch().await {
            Ok(Some(batch)) => {
                rows.extend(batch.into_iter().map(row_of));
                if last.elapsed() >= PUBLISH_EVERY && shown != rows.len() {
                    let (kept, snapshot) = sorted(rows).await;
                    rows = kept;
                    shown = snapshot.len();
                    last = Instant::now();
                    send(FilesEvent::Listing { side, generation, rows: snapshot, done: false });
                }
            },
            Ok(None) => {
                let (_, snapshot) = sorted(rows).await;
                send(FilesEvent::Listing { side, generation, rows: snapshot, done: true });
                return;
            },
            Err(err) => {
                let reason = err.to_string();
                send(FilesEvent::ListingFailed { side, generation, reason });
                return;
            },
        }
    }
}

/// Sort `rows`, and a copy of them to show.
async fn sorted(mut rows: Vec<FileRow>) -> (Vec<FileRow>, Arc<Vec<FileRow>>) {
    tokio::task::spawn_blocking(move || {
        sort_rows(&mut rows);
        let snapshot = Arc::new(rows.clone());
        (rows, snapshot)
    })
    .await
    .unwrap_or_else(|_| (Vec::new(), Arc::new(Vec::new())))
}

impl FilesPane {
    /// A pane with `link`'s file system on the right and the local disk on the left.
    pub fn new(
        pane: PaneId,
        tab: u64,
        link: FilesLink,
        local_start: String,
        runtime: Handle,
        proxy: EventLoopProxy<AppEvent>,
    ) -> Self {
        let FilesLink { fs, start, title } = link;
        let column = |title: String, path: String, fs: Arc<dyn RemoteFs>| Column {
            title,
            path,
            fs,
            rows: Arc::new(Vec::new()),
            busy: false,
            error: None,
            generation: 0,
            listing: None,
        };
        let local: Arc<dyn RemoteFs> = Arc::new(LocalFs::new());
        // The queue starts its workers on the runtime it finds itself in.
        let context = runtime.enter();
        let (queue, mut updates) = TransferQueue::new(QueueConfig::default());
        drop(context);
        let forward = proxy.clone();
        runtime.spawn(async move {
            while let Some(update) = updates.recv().await {
                let event = FilesEvent::Progress(Box::new(update));
                let _ = forward.send_event(AppEvent::Files(pane, tab, event));
            }
        });
        let mut files = Self {
            columns: [column("Local".to_owned(), local_start, local), column(title, start, fs)],
            queue,
            transfers: Vec::new(),
            next_number: 0,
            policy: 0,
            rows: Vec::new(),
            opening: None,
            document: None,
            diff: None,
            highlight_token: 0,
            pane,
            generation: tab,
            runtime,
            proxy,
        };
        files.list(FileSide::Local);
        files.list(FileSide::Remote);
        files
    }

    /// Which attempt at connecting this pane belongs to.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    fn column(&self, side: FileSide) -> &Column {
        &self.columns[side.index()]
    }

    fn column_mut(&mut self, side: FileSide) -> &mut Column {
        &mut self.columns[side.index()]
    }

    /// The directory a column shows.
    pub fn path(&self, side: FileSide) -> &str {
        &self.column(side).path
    }

    /// The rows of a column, in the order shown.
    pub fn rows(&self, side: FileSide) -> &[FileRow] {
        &self.column(side).rows
    }

    /// Start listing the directory a column should show.
    fn list(&mut self, side: FileSide) {
        let pane = self.pane;
        let tab = self.generation;
        let proxy = self.proxy.clone();
        let runtime = self.runtime.clone();
        let column = self.column_mut(side);
        if let Some(listing) = column.listing.take() {
            listing.abort();
        }
        column.generation += 1;
        column.busy = true;
        column.error = None;
        let generation = column.generation;
        let (fs, path) = (column.fs.clone(), column.path.clone());
        let task = runtime.spawn(async move {
            match fs.list(&path, ListOptions::default()).await {
                Ok(stream) => pump_listing(stream, side, generation, pane, tab, proxy).await,
                Err(err) => {
                    let reason = err.to_string();
                    let event = FilesEvent::ListingFailed { side, generation, reason };
                    let _ = proxy.send_event(AppEvent::Files(pane, tab, event));
                },
            }
        });
        self.column_mut(side).listing = Some(task.abort_handle());
    }

    /// Go to `path`, listing it.
    fn go(&mut self, side: FileSide, path: String) {
        let column = self.column_mut(side);
        column.path = path;
        column.rows = Arc::new(Vec::new());
        self.list(side);
    }

    /// Find out whether the link at `path` leads to a directory.
    fn follow_link(&self, side: FileSide, path: String) {
        let (pane, tab, proxy) = (self.pane, self.generation, self.proxy.clone());
        let fs = self.column(side).fs.clone();
        self.runtime.spawn(async move {
            let directory = fs.stat(&path).await.is_ok_and(|entry| entry.kind == EntryKind::Dir);
            let event = FilesEvent::Link { side, path, directory };
            let _ = proxy.send_event(AppEvent::Files(pane, tab, event));
        });
    }

    /// Do what the interface asked. Returns the side whose directory changed, if one did.
    pub fn action(&mut self, action: FilesAction) -> Option<FileSide> {
        match action {
            FilesAction::NewFolder { side, name } => self.change(side, Change::NewFolder(name)),
            FilesAction::Rename { side, from, to } => {
                self.change(side, Change::Rename(from.to_string(), to));
            },
            FilesAction::Delete { side, names } => {
                let rows = self.rows(side);
                let kinds = names.iter().filter_map(|name| {
                    rows.iter()
                        .find(|row| row.name == *name)
                        .map(|row| (name.to_string(), row.kind))
                });
                let names = kinds.collect();
                self.change(side, Change::Remove(names));
            },
            FilesAction::Open { side, name } => {
                let row = self.rows(side).iter().find(|row| row.name == name)?.clone();
                let path = join(self.path(side), &name);
                match row.kind {
                    FileKind::Dir => {
                        self.go(side, path);
                        return Some(side);
                    },
                    FileKind::Link => self.follow_link(side, path),
                    FileKind::File => self.open_text(side, path),
                    FileKind::Other => (),
                }
            },
            FilesAction::Compare { local, remote } => self.compare_files(&local, &remote),
            FilesAction::Save { text } => self.save(text, false),
            FilesAction::SaveAnyway => {
                if let Some(text) = self.document.as_mut().and_then(|file| file.held.take()) {
                    self.save(text, true);
                }
            },
            FilesAction::DiffEdits { text } => self.diff_edits(text),
            FilesAction::CloseBody => self.close_body(),
            FilesAction::Highlight { text } => self.request_highlight(text),
            FilesAction::Up { side } => {
                let path = parent(self.path(side));
                if path != self.path(side) {
                    self.go(side, path);
                    return Some(side);
                }
            },
            FilesAction::Goto { side, path } => {
                self.go(side, path);
                return Some(side);
            },
            FilesAction::Refresh { side } => self.list(side),
            FilesAction::Transfer { from, names } => self.transfer(from, &names),
            FilesAction::Cancel(number) => {
                if let Some(transfer) = self.find(number) {
                    self.queue.cancel(transfer.id);
                }
            },
            FilesAction::Retry(number) => {
                if let Some(index) = self.transfers.iter().position(|t| t.number == number) {
                    let id = self.transfers[index].id;
                    self.queue.retry(id);
                    let transfer = &mut self.transfers[index];
                    transfer.finished = false;
                    transfer.row.state = TransferRowState::Queued;
                    self.sync_rows();
                }
            },
            FilesAction::ClearFinished => {
                self.queue.clear_finished();
                self.transfers.retain(|transfer| !transfer.finished);
                self.sync_rows();
            },
            FilesAction::Policy(index) => {
                if index < POLICIES.len() {
                    self.policy = index;
                }
            },
        }
        None
    }

    /// Which policy is picked, as an index into `CONFLICT_POLICIES`.
    pub fn policy(&self) -> usize {
        self.policy
    }

    /// Start working out the colors of `text`, what the open file says by now.
    fn request_highlight(&mut self, text: String) {
        let Some(file) = &self.document else { return };
        self.highlight_token += 1;
        let (token, document, name) = (self.highlight_token, file.id, file.path.clone());
        let (pane, tab, proxy) = (self.pane, self.generation, self.proxy.clone());
        self.runtime.spawn_blocking(move || {
            let highlight = highlight(&name, &text);
            let _ = proxy.send_event(AppEvent::Files(
                pane,
                tab,
                FilesEvent::Highlighted { document, token, highlight },
            ));
        });
    }

    /// Whether a file is open, on screen or not.
    pub fn has_document(&self) -> bool {
        self.document.is_some()
    }

    /// Whether a file is being edited on screen, rather than looked at or compared.
    pub fn editing(&self) -> bool {
        self.opening.is_none() && self.diff.is_none() && self.document.is_some()
    }

    /// Go back from what is shown instead of the columns.
    pub fn close_body(&mut self) {
        if self.opening.take().is_none() && self.diff.take().is_none() {
            self.document = None;
        }
    }

    /// Start reading the file at `path` to show it.
    fn open_text(&mut self, side: FileSide, path: String) {
        let token = next_body();
        let title = self.column(side).title.clone();
        self.opening = Some((token, path.rsplit('/').next().unwrap_or(&path).to_owned()));
        let (pane, tab, proxy) = (self.pane, self.generation, self.proxy.clone());
        let fs = self.column(side).fs.clone();
        self.runtime.spawn(async move {
            let result = read_text(fs, side, path, title).await;
            let _ = proxy.send_event(AppEvent::Files(
                pane,
                tab,
                FilesEvent::Opened { token, side, result },
            ));
        });
    }

    /// Start comparing a file of each column.
    fn compare_files(&mut self, local: &str, remote: &str) {
        let token = next_body();
        self.opening = Some((
            token,
            if local == remote { local.to_owned() } else { format!("{local}, {remote}") },
        ));
        let (pane, tab, proxy) = (self.pane, self.generation, self.proxy.clone());
        let path =
            |side| join(self.path(side), if side == FileSide::Local { local } else { remote });
        let (left_path, right_path) = (path(FileSide::Local), path(FileSide::Remote));
        let titles = [
            format!("{}  {left_path}", self.column(FileSide::Local).title),
            format!("{}  {right_path}", self.column(FileSide::Remote).title),
        ];
        let left_fs = self.column(FileSide::Local).fs.clone();
        let right_fs = self.column(FileSide::Remote).fs.clone();
        self.runtime.spawn(async move {
            let (left, right) = tokio::join!(
                read_text(left_fs, FileSide::Local, left_path, String::new()),
                read_text(right_fs, FileSide::Remote, right_path, String::new()),
            );
            let (side, result) = match (left, right) {
                (Err(reason), _) => (FileSide::Local, Err(reason)),
                (_, Err(reason)) => (FileSide::Remote, Err(reason)),
                (Ok(left), Ok(right)) => {
                    let same =
                        left.text == right.text && left.crlf == right.crlf && left.bom == right.bom;
                    let names = [left.path.clone(), right.path.clone()];
                    (FileSide::Remote, compared(names, titles, [left.text, right.text], same).await)
                },
            };
            let _ = proxy.send_event(AppEvent::Files(
                pane,
                tab,
                FilesEvent::Compared { token, side, result },
            ));
        });
    }

    /// Start comparing `text` with the open file as it was read.
    fn diff_edits(&mut self, text: String) {
        let Some(file) = &self.document else { return };
        let token = next_body();
        let name = file.path.rsplit('/').next().unwrap_or(&file.path).to_owned();
        let names = [name.clone(), name.clone()];
        let titles = [format!("{name}  saved"), format!("{name}  edited")];
        let (side, saved, same) = (file.side, file.text.clone(), *file.text == *text);
        self.opening = Some((token, name));
        let (pane, tab, proxy) = (self.pane, self.generation, self.proxy.clone());
        self.runtime.spawn(async move {
            let result = compared(names, titles, [saved, text.into()], same).await;
            let _ = proxy.send_event(AppEvent::Files(
                pane,
                tab,
                FilesEvent::Compared { token, side, result },
            ));
        });
    }

    /// Write `text` to the open file. Unless `force`, not over a change someone else made.
    fn save(&mut self, text: String, force: bool) {
        let (pane, tab, proxy) = (self.pane, self.generation, self.proxy.clone());
        let Some(file) = self.document.as_mut().filter(|file| !file.saving && file.lines.is_none())
        else {
            return;
        };
        file.saving = true;
        file.error = None;
        let (document, path, stamp, crlf, bom) =
            (file.id, file.path.clone(), file.stamp, file.crlf, file.bom);
        let side = file.side;
        let fs = self.columns[side.index()].fs.clone();
        self.runtime.spawn(async move {
            let send = |event| {
                let _ = proxy.send_event(AppEvent::Files(pane, tab, event));
            };
            if !force {
                match fs.stat(&path).await {
                    Ok(entry) if stamp_of(&entry) != stamp => {
                        send(FilesEvent::SaveConflict { document, text });
                        return;
                    },
                    Ok(_) | Err(FsError::NotFound(_)) => (),
                    Err(err) => {
                        send(FilesEvent::Saved { document, result: Err(err.to_string()) });
                        return;
                    },
                }
            }
            let written = write_file(fs.as_ref(), &path, encode(&text, crlf, bom)).await;
            let result = match written {
                Ok(()) => match fs.stat(&path).await {
                    Ok(entry) => Ok(Saved { text: text.into(), stamp: stamp_of(&entry) }),
                    Err(err) => Err(err.to_string()),
                },
                Err(reason) => Err(reason),
            };
            send(FilesEvent::Saved { document, result });
        });
    }

    fn find(&self, number: u64) -> Option<&Transfer> {
        self.transfers.iter().find(|transfer| transfer.number == number)
    }

    /// Queue copies of `names` from the column to the one across from it.
    fn transfer(&mut self, from: FileSide, names: &[Arc<str>]) {
        let to = from.other();
        let overwrite = POLICIES[self.policy];
        let (source, source_dir) = (self.column(from).fs.clone(), self.path(from).to_owned());
        let (dest, dest_dir) = (self.column(to).fs.clone(), self.path(to).to_owned());
        for name in names {
            let source_path = join(&source_dir, name);
            let dest_path = join(&dest_dir, name);
            let label = format!(
                "{name} {} {}",
                if from == FileSide::Local { "to" } else { "from" },
                self.column(FileSide::Remote).title,
            );
            let id = self.queue.submit(TransferRequest {
                source: source.clone(),
                source_path,
                dest: dest.clone(),
                dest_path,
                overwrite,
            });
            let number = self.next_number;
            self.next_number += 1;
            let row = TransferRow {
                id: number,
                label,
                detail: String::new(),
                fraction: None,
                state: TransferRowState::Queued,
            };
            self.transfers.push(Transfer {
                id,
                number,
                to,
                directory: dest_dir.clone(),
                row,
                finished: false,
            });
        }
        self.sync_rows();
    }

    fn sync_rows(&mut self) {
        self.rows.clear();
        self.rows.extend(self.transfers.iter().map(|transfer| transfer.row.clone()));
    }

    /// Make `change` in what the column shows, off the main thread, and list it again after.
    fn change(&mut self, side: FileSide, change: Change) {
        let column = self.column_mut(side);
        let (fs, directory) = (column.fs.clone(), column.path.clone());
        let (proxy, pane, generation) = (self.proxy.clone(), self.pane, self.generation);
        self.runtime.spawn(async move {
            let error = apply_change(fs, &directory, change).await.err();
            let event = FilesEvent::Changed { side, directory, error };
            let _ = proxy.send_event(AppEvent::Files(pane, generation, event));
        });
    }

    /// News from the tasks behind the pane. Returns the old rows, which are too many to free here,
    /// and which columns to list again now that a transfer has finished landing in them.
    pub fn apply(&mut self, event: FilesEvent) -> Applied {
        let mut outcome = Applied::default();
        match event {
            FilesEvent::Listing { side, generation, rows, done } => {
                let column = self.column_mut(side);
                if generation != column.generation {
                    outcome.retired = Some(rows);
                    return outcome;
                }
                column.busy = !done;
                outcome.retired = Some(std::mem::replace(&mut column.rows, rows));
            },
            FilesEvent::ListingFailed { side, generation, reason } => {
                let column = self.column_mut(side);
                if generation == column.generation {
                    column.busy = false;
                    column.error = Some(reason);
                }
            },
            FilesEvent::Changed { side, directory, error } => match error {
                Some(reason) => self.column_mut(side).error = Some(reason),
                None => outcome.refresh.push((side, directory)),
            },
            FilesEvent::Link { side, path, directory } => {
                if directory {
                    self.go(side, path);
                    outcome.moved = Some(side);
                } else {
                    self.open_text(side, path);
                }
            },
            FilesEvent::Opened { token, side, result } => {
                if self.opening.as_ref().is_none_or(|(current, _)| *current != token) {
                    return outcome;
                }
                self.opening = None;
                match result {
                    Ok(file) => {
                        let text = file.text.to_string();
                        self.document = Some(file);
                        self.request_highlight(text);
                    },
                    Err(reason) => self.column_mut(side).error = Some(reason),
                }
            },
            FilesEvent::Compared { token, side, result } => {
                if self.opening.as_ref().is_none_or(|(current, _)| *current != token) {
                    return outcome;
                }
                self.opening = None;
                match result {
                    Ok(diff) => self.diff = Some(diff),
                    Err(reason) => match &mut self.document {
                        Some(document) => document.error = Some(reason),
                        None => self.column_mut(side).error = Some(reason),
                    },
                }
            },
            FilesEvent::Saved { document, result } => {
                let Some(file) = self.document.as_mut().filter(|file| file.id == document) else {
                    return outcome;
                };
                file.saving = false;
                match result {
                    Ok(saved) => {
                        file.text = saved.text;
                        file.stamp = saved.stamp;
                        file.revision += 1;
                        file.error = None;
                        outcome.refresh.push((file.side, parent(&file.path)));
                    },
                    Err(reason) => file.error = Some(reason),
                }
            },
            FilesEvent::Highlighted { document, token, highlight } => {
                if token == self.highlight_token
                    && let Some(file) = self.document.as_mut().filter(|file| file.id == document)
                {
                    file.highlight = highlight;
                    file.highlight_id += 1;
                }
            },
            FilesEvent::SaveConflict { document, text } => {
                if let Some(file) = self.document.as_mut().filter(|file| file.id == document) {
                    file.saving = false;
                    file.held = Some(text);
                    outcome.conflict = Some(file.path.clone());
                }
            },
            FilesEvent::Progress(update) => {
                for progress in &update.transfers {
                    let Some(transfer) = self.transfers.iter_mut().find(|t| t.id == progress.id)
                    else {
                        continue;
                    };
                    let done = progress.bytes_done;
                    let total = progress.bytes_total;
                    transfer.row.fraction = (total > 0)
                        .then(|| (done as f64 / total as f64) as f32)
                        .or((progress.state == TransferState::Done).then_some(1.));
                    let (state, word): (TransferRowState, String) = match &progress.state {
                        TransferState::Queued => (TransferRowState::Queued, "Queued".to_owned()),
                        TransferState::Running => (TransferRowState::Running, "Copying".to_owned()),
                        TransferState::Done => (TransferRowState::Done, "Done".to_owned()),
                        TransferState::Failed(reason) => {
                            (TransferRowState::Failed, format!("Failed: {reason}"))
                        },
                        TransferState::Cancelled => {
                            (TransferRowState::Cancelled, "Cancelled".to_owned())
                        },
                    };
                    transfer.row.state = state;
                    transfer.row.detail = if progress.files_total > 1 {
                        format!("{word}, {} of {} files", progress.files_done, progress.files_total)
                    } else {
                        word
                    };

                    let finished = progress.state.is_finished();
                    if finished && !transfer.finished {
                        outcome.refresh.push((transfer.to, transfer.directory.clone()));
                    }
                    transfer.finished = finished;
                }
                self.sync_rows();
            },
        }
        outcome
    }

    /// List `side` again if it still shows `directory`.
    pub fn refresh_if_showing(&mut self, side: FileSide, directory: &str) {
        if self.path(side) == directory {
            self.list(side);
        }
    }

    /// Stop what the pane has running.
    pub fn stop(&mut self) {
        self.queue.cancel_all();
        for column in &mut self.columns {
            if let Some(listing) = column.listing.take() {
                listing.abort();
            }
        }
    }

    /// What the interface shows of the pane.
    pub fn view(&self, pane: rmx_ui::PaneId) -> FilesView<'_> {
        let column = |side: FileSide| {
            let column = self.column(side);
            FileColumnView {
                side,
                title: &column.title,
                path: &column.path,
                rows: &column.rows,
                busy: column.busy,
                error: column.error.as_deref(),
            }
        };
        FilesView {
            pane,
            columns: [column(FileSide::Local), column(FileSide::Remote)],
            transfers: &self.rows,
            policy: self.policy,
            body: self.body(),
        }
    }

    fn body(&self) -> FilesBody<'_> {
        if let Some((_, name)) = &self.opening {
            return FilesBody::Loading(name);
        }
        if let Some(diff) = &self.diff {
            return FilesBody::Diff(DiffView {
                id: diff.id,
                titles: [&diff.titles[0], &diff.titles[1]],
                texts: [&diff.texts[0], &diff.texts[1]],
                rows: &diff.found.rows,
                changes: &diff.found.changes,
                longest: diff.found.longest,
                note: diff.note,
                highlights: [diff.highlights[0].as_ref(), diff.highlights[1].as_ref()],
            });
        }
        match &self.document {
            Some(file) => FilesBody::Text(TextView {
                id: file.id,
                revision: file.revision,
                title: &file.title,
                path: &file.path,
                text: &file.text,
                lines: file.lines.as_ref().map(|lines| lines.as_slice()),
                saving: file.saving,
                error: file.error.as_deref(),
                highlight: file.highlight.as_ref(),
                highlight_id: file.highlight_id,
            }),
            None => FilesBody::Columns,
        }
    }

    /// What the file open in the pane holds, for checking from a script.
    #[cfg(feature = "bench")]
    pub fn document_text(&self) -> Option<&str> {
        self.document.as_ref().map(|file| file.text.as_ref())
    }

    /// The rows of the comparison on screen.
    #[cfg(feature = "bench")]
    pub fn diff_rows(&self) -> Option<&[rmx_ui::widgets::DiffRow]> {
        self.diff.as_ref().map(|diff| diff.found.rows.as_slice())
    }

    /// The interface's rows of the queue.
    #[cfg(feature = "bench")]
    pub fn transfers(&self) -> &[TransferRow] {
        &self.rows
    }

    /// Where the pointer goes to reach `name` of a column, if it is on screen near the top.
    #[cfg(feature = "bench")]
    pub fn row_point(
        &self,
        area: egui::Rect,
        side: FileSide,
        name: &str,
        panels: &rmx_ui::widgets::Panels,
    ) -> Option<egui::Pos2> {
        let index = self.rows(side).iter().position(|row| row.name.as_ref() == name)?;
        Some(rmx_ui::widgets::file_row_point(area, side, index, panels))
    }
}

/// What the main thread has to do about a file pane's news.
#[derive(Default)]
pub struct Applied {
    /// Rows replaced, to be freed off the main thread.
    pub retired: Option<Arc<Vec<FileRow>>>,
    /// A column moved to another directory.
    pub moved: Option<FileSide>,
    /// Columns that a transfer finished landing in, and the directory it landed in.
    pub refresh: Vec<(FileSide, String)>,
    /// A save was held back: the file at this path changed since it was read.
    pub conflict: Option<String>,
}

/// The directory the local column opens at.
pub fn local_home() -> String {
    let home = std::env::home_dir().filter(|home| home.is_dir());
    home.and_then(|home| home.to_str().map(str::to_owned)).unwrap_or_else(|| "/".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, kind: FileKind) -> FileRow {
        FileRow::new(name, kind, None, None)
    }

    #[test]
    fn directories_come_first_and_case_does_not_matter() {
        let mut rows = vec![
            row("beta.txt", FileKind::File),
            row("Zeta", FileKind::Dir),
            row("alpha.txt", FileKind::File),
            row("Alpha", FileKind::Dir),
        ];
        sort_rows(&mut rows);
        let names: Vec<&str> = rows.iter().map(|row| row.name.as_ref()).collect();
        assert_eq!(names, ["Alpha", "Zeta", "alpha.txt", "beta.txt"]);
    }

    #[test]
    fn a_hundred_thousand_rows_sort_without_a_quadratic_cost() {
        let mut rows: Vec<FileRow> =
            (0..100_000).map(|n| row(&format!("{:06}", 100_000 - n), FileKind::File)).collect();
        let started = Instant::now();
        sort_rows(&mut rows);
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(rows[0].name.as_ref(), "000001");
    }

    #[test]
    fn line_endings_and_a_byte_order_mark_come_back_as_they_were() {
        for original in ["a\nb\n", "a\r\nb\r\n", "\u{FEFF}a\r\nb", "mixed\r\nends\n", "no end", ""]
        {
            let (text, crlf, bom) = decode(original.as_bytes().to_vec()).unwrap();
            assert_eq!(encode(&text, crlf, bom), original.as_bytes(), "{original:?}");
        }
    }

    #[test]
    fn crlf_is_shown_as_plain_lines_only_when_the_whole_file_uses_it() {
        assert_eq!(decode(b"a\r\nb\r\n".to_vec()).unwrap(), ("a\nb\n".to_owned(), true, false));
        assert_eq!(decode(b"a\r\nb\n".to_vec()).unwrap(), ("a\r\nb\n".to_owned(), false, false));
    }

    #[test]
    fn what_is_not_text_is_refused() {
        assert_eq!(decode(b"ELF\0binary".to_vec()), None);
        assert_eq!(
            decode(vec![0xC4, 0xE3, 0xBA, 0xC3]),
            None,
            "GBK, which would not survive a save"
        );
        assert!(decode("中文".as_bytes().to_vec()).is_some());
    }

    #[test]
    fn lines_start_after_each_break_and_the_last_break_opens_no_line() {
        assert_eq!(line_starts("a\nbc\nd"), [0, 2, 5]);
        assert_eq!(line_starts("a\nb\n"), [0, 2]);
        assert_eq!(line_starts(""), [0]);
    }

    #[test]
    fn paths_join_and_climb() {
        assert_eq!(join("/a", "b"), "/a/b");
        assert_eq!(join("/", "b"), "/b");
        assert_eq!(parent("/a/b"), "/a");
        assert_eq!(parent("/a"), "/");
        assert_eq!(parent("/"), "/");
    }
}
