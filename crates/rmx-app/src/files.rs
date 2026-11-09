//! A file pane: two listings and a queue of copies, with everything that waits off the main thread.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rmx_layout::PaneId;
use rmx_transfer::{
    DirStream, Entry, EntryKind, ListOptions, LocalFs, OverwritePolicy, ProgressUpdate, QueueConfig,
    RemoteFs, TransferId, TransferQueue, TransferRequest, TransferState,
};
#[cfg(feature = "bench")]
use rmx_ui::egui;
use rmx_ui::widgets::{
    FileColumnView, FileKind, FileRow, FilesView, TransferRow, TransferRowState,
};
use rmx_ui::{FileSide, FilesAction};
use tokio::runtime::Handle;
use tokio::task::AbortHandle;
use winit::event_loop::EventLoopProxy;

use crate::app::AppEvent;

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
    Listing { side: FileSide, generation: u64, rows: Arc<Vec<FileRow>>, done: bool },
    ListingFailed { side: FileSide, generation: u64, reason: String },
    /// A link turned out to lead to a directory, or not.
    Link { side: FileSide, path: String, directory: bool },
    Progress(Box<ProgressUpdate>),
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
        (a.kind != FileKind::Dir, &a.sort, &a.name).cmp(&(b.kind != FileKind::Dir, &b.sort, &b.name))
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
    if directory.ends_with('/') { format!("{directory}{name}") } else { format!("{directory}/{name}") }
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
            columns: [
                column("Local".to_owned(), local_start, local),
                column(title, start, fs),
            ],
            queue,
            transfers: Vec::new(),
            next_number: 0,
            policy: 0,
            rows: Vec::new(),
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
            FilesAction::Open { side, name } => {
                let row = self.rows(side).iter().find(|row| row.name == name)?.clone();
                let path = join(self.path(side), &name);
                match row.kind {
                    FileKind::Dir => {
                        self.go(side, path);
                        return Some(side);
                    },
                    FileKind::Link => self.follow_link(side, path),
                    _ => (),
                }
            },
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
            FilesEvent::Link { side, path, directory } => {
                if directory {
                    self.go(side, path);
                    outcome.moved = Some(side);
                }
            },
            FilesEvent::Progress(update) => {
                for progress in &update.transfers {
                    let Some(transfer) = self.transfers.iter_mut().find(|t| t.id == progress.id) else {
                        continue;
                    };
                    let done = progress.bytes_done;
                    let total = progress.bytes_total;
                    transfer.row.fraction =
                        (total > 0).then(|| (done as f64 / total as f64) as f32).or(
                            (progress.state == TransferState::Done).then_some(1.),
                        );
                    let (state, word): (TransferRowState, String) = match &progress.state {
                        TransferState::Queued => (TransferRowState::Queued, "Queued".to_owned()),
                        TransferState::Running => (TransferRowState::Running, "Copying".to_owned()),
                        TransferState::Done => (TransferRowState::Done, "Done".to_owned()),
                        TransferState::Failed(reason) => {
                            (TransferRowState::Failed, format!("Failed: {reason}"))
                        },
                        TransferState::Cancelled => (TransferRowState::Cancelled, "Cancelled".to_owned()),
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
        }
    }

    /// The interface's rows of the queue.
    #[cfg(feature = "bench")]
    pub fn transfers(&self) -> &[TransferRow] {
        &self.rows
    }

    /// Where the pointer goes to reach `name` of a column, if it is on screen near the top.
    #[cfg(feature = "bench")]
    pub fn row_point(&self, area: egui::Rect, side: FileSide, name: &str) -> Option<egui::Pos2> {
        let index = self.rows(side).iter().position(|row| row.name.as_ref() == name)?;
        Some(rmx_ui::widgets::file_row_point(area, side, index))
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
}

/// The directory the local column opens at.
pub fn local_home() -> String {
    std::env::var("HOME").ok().filter(|home| Path::new(home).is_dir()).unwrap_or_else(|| "/".to_owned())
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
    fn paths_join_and_climb() {
        assert_eq!(join("/a", "b"), "/a/b");
        assert_eq!(join("/", "b"), "/b");
        assert_eq!(parent("/a/b"), "/a");
        assert_eq!(parent("/a"), "/");
        assert_eq!(parent("/"), "/");
    }
}
