//! Importing `.rdp` files: asking for them or taking them dropped on a window, reading them off
//! the main thread, and the preview of what they make before anything is saved.

use std::path::PathBuf;

use rmx_ui::widgets::{ImportPreviewState, ImportProblem, ImportRow};

use super::*;
use crate::rdp_file::{self, ImportedFile};

/// The folder imported files go in unless the preview says otherwise.
const IMPORT_GROUP: &str = "RDP files";

impl At<'_> {
    /// Ask which files to import: one, a folder of them, or several separated by `;`.
    pub(super) fn ask_rdp_files(&mut self) {
        let home = std::env::home_dir().map_or(String::new(), |home| home.display().to_string());
        let field = PromptField { label: String::new(), secret: false, value: format!("{home}/") };
        let mut state = PromptState::new(
            "Import .rdp files",
            "A file, or a folder of them. Separate several with ;",
            vec![field],
        );
        state.submit = "Read".to_owned();
        self.ask_here(state, Reply::ImportPaths);
    }

    /// Read these files and folders off the main thread; the preview follows.
    pub(super) fn read_rdp_files(&mut self, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            return;
        }
        let proxy = self.core.proxy.clone();
        self.core.runtime.spawn_blocking(move || {
            let files = rdp_file::read_paths(&paths);
            let _ = proxy.send_event(AppEvent::RdpFiles(files));
        });
    }

    /// Show what the files read make, adding to the preview if one is up already, as each file
    /// dropped at once arrives on its own.
    pub(super) fn preview_rdp_files(&mut self, files: Vec<ImportedFile>) {
        // Locked in the meantime: there is nowhere to save them.
        if self.core.ui.locked() {
            return;
        }
        let mut rows = Vec::new();
        let mut drafts = Vec::new();
        let mut problems = Vec::new();
        for file in files {
            match file.result {
                Ok(import) => {
                    rows.push(import_row(&import.draft, import.notes));
                    drafts.push(import.draft);
                },
                Err(reason) => problems.push(ImportProblem { source: file.source, reason }),
            }
        }
        if let (Some((Reply::Import(pending), _)), Some(Asking::Import(state))) =
            (&mut self.core.answer, &mut self.core.ui.asking)
        {
            pending.extend(drafts);
            state.rows.extend(rows);
            state.problems.extend(problems);
            self.core.broadcast = true;
            return;
        }
        if self.core.answer.is_some() {
            let note = "Answer the open question first, then import the files again";
            self.core.ui.status_note = Some((note.to_owned(), true));
            self.core.broadcast = true;
            return;
        }
        let mut state = ImportPreviewState::new("Import .rdp files", IMPORT_GROUP);
        state.rows = rows;
        state.problems = problems;
        self.core.ui.asking = Some(Asking::Import(state));
        self.core.ui.asking_window = self.win.key;
        self.core.answer = Some((Reply::Import(drafts), None));
        self.core.broadcast = true;
    }

    /// Save what the preview has ticked, in `group`.
    pub(super) fn confirm_import(&mut self, group: &str, include: &[bool]) {
        if !matches!(self.core.answer, Some((Reply::Import(_), _))) {
            return;
        }
        let Some((Reply::Import(drafts), _)) = self.core.answer.take() else { return };
        self.core.ui.asking = None;
        let drafts: Vec<ConnectionDraft> = drafts
            .into_iter()
            .zip(include)
            .filter(|&(_, &include)| include)
            .map(|(mut draft, _)| {
                draft.group = group.to_owned();
                draft
            })
            .collect();
        if !drafts.is_empty() {
            self.core.vault.send(Request::ImportConnections(drafts));
        }
        self.core.broadcast = true;
        self.next_prompt();
    }

    /// Ask where to write the RDP connection `node` as an `.rdp` file.
    pub(super) fn ask_export_rdp(&mut self, node: u64) {
        let Some(info) = self.core.ui.nodes.get(&node) else { return };
        let home = std::env::home_dir().map_or(String::new(), |home| home.display().to_string());
        let name = info.name.replace(['/', '\\'], "-");
        let value = format!("{home}/{name}.rdp");
        let field = PromptField { label: String::new(), secret: false, value };
        let mut state = PromptState::new(
            "Export as .rdp",
            "Everything but the password, which stays in the vault.",
            vec![field],
        );
        state.submit = "Export".to_owned();
        self.ask_here(state, Reply::ExportRdp(node));
    }
}

/// The paths typed into the question, `;` between them, a leading `~` the home directory.
pub(super) fn typed_paths(text: &str) -> Vec<PathBuf> {
    text.split(';')
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(|path| match path.strip_prefix('~') {
            Some(rest) if rest.is_empty() || rest.starts_with(['/', '\\']) => {
                let home = std::env::home_dir().unwrap_or_default();
                home.join(rest.trim_start_matches(['/', '\\']))
            },
            _ => PathBuf::from(path),
        })
        .collect()
}

/// How a connection about to be made shows in the preview.
fn import_row(draft: &ConnectionDraft, notes: Vec<String>) -> ImportRow {
    let user = match (draft.domain.as_str(), draft.username.as_str()) {
        (_, "") => String::new(),
        ("", user) => format!(" as {user}"),
        (domain, user) => format!(" as {domain}\\{user}"),
    };
    let host =
        if draft.host.contains(':') { format!("[{}]", draft.host) } else { draft.host.clone() };
    ImportRow {
        name: draft.name.clone(),
        protocol: draft.protocol,
        detail: format!("{host}:{}{user}", draft.port),
        notes,
        include: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn several_paths_split_at_semicolons() {
        let home = std::env::home_dir().unwrap_or_default();
        assert_eq!(
            typed_paths(" /a/b.rdp ; ~/c.rdp;;~"),
            [PathBuf::from("/a/b.rdp"), home.join("c.rdp"), home.join("")]
        );
        assert!(typed_paths("  ").is_empty());
    }

    #[test]
    fn a_row_says_who_connects_where() {
        let mut draft = ConnectionDraft::new(Protocol::Rdp);
        draft.name = "web".into();
        draft.host = "fd00::1".into();
        draft.port = "3390".into();
        draft.username = "alice".into();
        draft.domain = "CORP".into();
        let row = import_row(&draft, vec!["note".into()]);
        assert_eq!(row.detail, "[fd00::1]:3390 as CORP\\alice");
        draft.domain.clear();
        draft.username = "bob@corp.example".into();
        assert_eq!(import_row(&draft, Vec::new()).detail, "[fd00::1]:3390 as bob@corp.example");
        draft.username.clear();
        assert_eq!(import_row(&draft, Vec::new()).detail, "[fd00::1]:3390");
        assert_eq!(row.notes, ["note"]);
        assert!(row.include);
    }
}
