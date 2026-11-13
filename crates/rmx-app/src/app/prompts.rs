//! Dialogs: the questions a session or the vault asks, and the answers that come back.

use super::*;
use crate::closing;

/// Where the answer to the question on screen goes.
pub(super) enum Reply {
    HostKey(oneshot::Sender<HostKeyAnswer>),
    Fields(oneshot::Sender<Option<Vec<SecretString>>>),
    NewFolder(Option<u64>),
    RenameFolder(u64),
    /// The login a folder hands down: username, password and key file.
    FolderLogin(u64),
    DeleteConnection(u64),
    DeleteFolder(u64),
    /// The values of a snippet's placeholders: send it with them.
    Snippet(String),
    FilesNewFolder(PaneId, rmx_ui::FileSide),
    FilesRename(PaneId, rmx_ui::FileSide, Arc<str>),
    FilesDelete(PaneId, rmx_ui::FileSide, Vec<Arc<str>>),
    /// Write over a file that changed since it was opened.
    FilesOverwrite(PaneId),
    /// Go on with `then`, losing the edits in these panes.
    LoseEdits(Vec<PaneId>, UiAction),
    /// Go on with closing `then`, ending the live sessions in it.
    EndSessions(UiAction),
    /// Go to the vault at the path typed.
    VaultPath,
    /// Move the vault's file to the path typed.
    VaultMove,
    /// Change the master password: the current one, the new one twice, and a key file.
    VaultPassword,
    /// The synced copy's new master password and key file.
    VaultRemotePassword,
    /// Nothing to do: the dialog only told something.
    Acknowledge,
}

impl At<'_> {
    /// Queue a question for the user, and show it if nothing else is showing.
    pub(super) fn ask(&mut self, prompt: Prompt) {
        if let Some(id) = prompt.pane
            && self
                .pane(id)
                .and_then(|pane| pane.remote.as_ref())
                .is_some_and(|remote| remote.quiet)
        {
            self.leave_awaiting(id, &prompt);
            return;
        }
        self.core.prompts.push_back(prompt);
        self.next_prompt();
    }

    /// A session reopened at startup needs the user: stop it there, saying what for. Dropping the
    /// prompt tells the attempt there will be no answer.
    pub(super) fn leave_awaiting(&mut self, id: PaneId, prompt: &Prompt) {
        let why = match &prompt.kind {
            PromptKind::HostKey(..) => "The server's key needs your word".to_owned(),
            PromptKind::Fields { title, .. } => title.clone(),
        };
        let Some(pane) = self.pane_mut(id) else { return };
        pane.hang_up();
        if let Some(remote) = pane.remote.as_mut() {
            // Whatever it reports from here on is about the attempt given up on.
            remote.generation += 1;
        }
        pane.link = Link::Awaiting { why };
        self.link_changed(id);
    }

    /// Ask something of the app's own, ahead of anything waiting.
    pub(super) fn ask_here(&mut self, state: PromptState, reply: Reply) {
        if self.core.answer.is_none() {
            self.core.ui.asking = Some(Asking::Fields(state));
            self.core.ui.asking_window = self.win.key;
            self.core.answer = Some((reply, None));
            self.core.broadcast = true;
        }
    }

    /// Tell the user how something went, in a dialog; on the sidebar's foot if one is up already.
    pub(super) fn tell(&mut self, title: &str, detail: &str, bad: bool) {
        if self.core.answer.is_some() {
            self.core.ui.status_note = Some((format!("{title}: {detail}"), bad));
            self.core.broadcast = true;
            return;
        }
        self.ask_here(PromptState::notice(title, detail, bad), Reply::Acknowledge);
    }

    /// The synced copy's master password was changed on another device: ask for the new one.
    pub(super) fn ask_remote_password(&mut self) {
        let fields = vec![
            PromptField {
                label: "New master password".to_owned(),
                secret: true,
                value: String::new(),
            },
            PromptField {
                label: "Key file (optional)".to_owned(),
                secret: false,
                value: self.core.ui.vault_key_file.clone(),
            },
        ];
        let detail = "The synced copy has a different master password, such as one set on another device. Enter it to keep syncing; this vault will use it too.";
        let mut state = PromptState::new("Synced copy has another password", detail, fields);
        state.submit = "Use it".to_owned();
        self.ask_here(state, Reply::VaultRemotePassword);
    }

    /// Put the next question that still matters in the dialog. While the vault is locked only a
    /// question from syncing can be asked: sessions are out of sight.
    pub(super) fn next_prompt(&mut self) {
        if self.core.answer.is_some() {
            return;
        }
        let locked = self.core.ui.locked();
        self.core.prompts.retain(|prompt| !prompt.abandoned());
        let Some(index) = self.core.prompts.iter().position(|p| !locked || p.pane.is_none()) else {
            return;
        };
        let Some(Prompt { pane, kind }) = self.core.prompts.remove(index) else { return };
        let (asking, reply) = match kind {
            PromptKind::HostKey(prompt, reply) => (Asking::HostKey(prompt), Reply::HostKey(reply)),
            PromptKind::Fields { title, detail, fields, reply } => {
                (Asking::Fields(PromptState::new(title, detail, fields)), Reply::Fields(reply))
            },
        };
        // Over the pane it is about; a question about nothing goes where the user is.
        let window = pane.and_then(|id| self.pane(id)).map_or(self.core.active, |pane| pane.window);
        self.core.ui.asking = Some(asking);
        self.core.ui.asking_window = window;
        self.core.answer = Some((reply, pane));
        self.core.broadcast = true;
    }

    /// The questions about `pane` no longer matter.
    pub(super) fn dismiss_prompts(&mut self, pane: PaneId) {
        self.core.prompts.retain(|prompt| prompt.pane != Some(pane));
        if matches!(&self.core.answer, Some((_, Some(id))) if *id == pane) {
            self.core.answer = None;
            self.core.ui.asking = None;
            self.core.broadcast = true;
            self.next_prompt();
        }
    }

    /// Finish the question on screen.
    pub(super) fn answered(
        &mut self,
        host_key: Option<HostKeyAnswer>,
        fields: Option<Vec<Sensitive>>,
    ) {
        let Some((reply, _)) = self.core.answer.take() else { return };
        let checked = matches!(&self.core.ui.asking, Some(Asking::Fields(state)) if state.checked);
        self.core.ui.asking = None;
        let first =
            fields.as_ref().and_then(|fields| fields.first()).map(|f| f.expose().trim().to_owned());
        match reply {
            Reply::HostKey(reply) => {
                let _ = reply.send(host_key.unwrap_or(HostKeyAnswer::Reject));
            },
            Reply::Fields(reply) => {
                let answers = fields.map(|fields| {
                    fields
                        .iter()
                        .map(|field| SecretString::new(field.expose().to_owned()))
                        .collect()
                });
                let _ = reply.send(answers);
            },
            Reply::NewFolder(parent) => {
                if let Some(name) = first.filter(|name| !name.is_empty()) {
                    self.core.vault.send(Request::NewFolder { parent, name });
                }
            },
            Reply::RenameFolder(node) => {
                if let Some(name) = first.filter(|name| !name.is_empty()) {
                    self.core.vault.send(Request::RenameFolder { node, name });
                }
            },
            Reply::FolderLogin(node) => {
                if let Some([username, password, key_path]) = fields.as_deref() {
                    self.core.vault.send(Request::SaveFolderLogin {
                        node,
                        username: username.expose().trim().to_owned(),
                        password: Secret::new(password.expose()),
                        key_path: key_path.expose().trim().to_owned(),
                    });
                }
            },
            Reply::Snippet(body) => {
                if let Some(fields) = &fields {
                    let names = snippet::variables(&body);
                    let values: Vec<(&str, &str)> = names
                        .iter()
                        .zip(fields)
                        .map(|(variable, field)| (variable.name, field.expose()))
                        .collect();
                    let text = snippet::expand(&body, &values);
                    self.send_text(&text);
                }
            },
            Reply::FilesNewFolder(pane, side) => {
                if let Some(name) = first.filter(|name| !name.is_empty()) {
                    self.files_action(pane, FilesAction::NewFolder { side, name });
                }
            },
            Reply::FilesRename(pane, side, from) => {
                if let Some(to) = first.filter(|name| !name.is_empty() && *name != *from) {
                    self.files_action(pane, FilesAction::Rename { side, from, to });
                }
            },
            Reply::FilesDelete(pane, side, names) => {
                if fields.is_some() {
                    self.files_action(pane, FilesAction::Delete { side, names });
                }
            },
            Reply::FilesOverwrite(pane) => {
                if fields.is_some() {
                    self.files_action(pane, FilesAction::SaveAnyway);
                }
            },
            Reply::LoseEdits(panes, then) => {
                if fields.is_some() {
                    for pane in panes {
                        self.core.ui.files_forget_edits(pane);
                    }
                    // A close goes on to ask about the sessions it ends.
                    self.ui_action(then);
                }
            },
            Reply::EndSessions(then) => {
                if fields.is_some() {
                    if checked {
                        self.set_confirm_close(false);
                    }
                    self.close_now(&then);
                }
            },
            Reply::VaultPath => {
                if let Some(path) = first.filter(|path| !path.is_empty()) {
                    self.core.ui.vault_error = None;
                    self.core.vault.send(Request::Switch(path));
                }
            },
            Reply::VaultMove => {
                if let Some(path) = first.filter(|path| !path.is_empty()) {
                    self.core.vault.send(Request::Move(path));
                }
            },
            Reply::VaultPassword => {
                if let Some([current, new, repeat, key_file]) = fields.as_deref() {
                    if new.expose() != repeat.expose() {
                        self.tell(
                            "Master password not changed",
                            "The new passwords do not match",
                            true,
                        );
                    } else {
                        self.core.ui.status_note =
                            Some(("Changing the master password\u{2026}".to_owned(), false));
                        self.core.vault.send(Request::ChangePassword {
                            current: Secret::new(current.expose()),
                            new: Secret::new(new.expose()),
                            key_file: key_path(key_file.expose()),
                        });
                    }
                }
            },
            Reply::VaultRemotePassword => {
                if let Some([password, key_file]) = fields.as_deref() {
                    self.core.vault.send(Request::AdoptPassword {
                        password: Secret::new(password.expose()),
                        key_file: key_path(key_file.expose()),
                    });
                }
            },
            Reply::Acknowledge => {},
            Reply::DeleteConnection(node) => {
                if fields.is_some() {
                    self.core.vault.send(Request::DeleteConnection(node));
                }
            },
            Reply::DeleteFolder(node) => {
                if fields.is_some() {
                    self.core.vault.send(Request::DeleteFolder(node));
                }
            },
        }
        self.core.broadcast = true;
        self.next_prompt();
    }

    /// Hand a typed password to the vault's thread, and show that it is working.
    pub(super) fn start_vault_work(&mut self) {
        self.core.ui.vault_busy = true;
        self.core.ui.vault_error = None;
        self.core.broadcast = true;
    }

    /// Ask before `then` loses edits made in any of `panes`. Returns whether it was asked.
    pub(super) fn ask_before_losing_edits(&mut self, panes: Vec<PaneId>, then: UiAction) -> bool {
        let dirty: Vec<PaneId> = panes
            .into_iter()
            .filter(|&pane| {
                self.core.ui.files_dirty(pane)
                    && self
                        .pane(pane)
                        .and_then(|found| found.content.files())
                        .is_some_and(FilesPane::has_document)
            })
            .collect();
        if dirty.is_empty() {
            return false;
        }
        let mut state = PromptState::new("Discard your changes?", "", Vec::new());
        state.submit = "Discard".to_owned();
        self.ask_here(state, Reply::LoseEdits(dirty, then));
        true
    }

    /// Close what `then` says, a window, tab or pane, first asking if that loses edits or ends
    /// live sessions. See [`crate::closing`] for what is asked about.
    pub(super) fn close_asking(&mut self, then: UiAction) {
        let panes = closing::closed_by(&self.win.workspace, &then);
        if self.ask_before_losing_edits(panes.clone(), then.clone()) {
            return;
        }
        if self.core.confirm_close {
            let hidden = self.core.ui.locked();
            let names = closing::live_sessions(&self.core.panes, &panes);
            if let Some(state) = closing::question(&names, hidden) {
                self.ask_here(state, Reply::EndSessions(then));
                return;
            }
        }
        self.close_now(&then);
    }

    /// Close what `then` says, without asking.
    pub(super) fn close_now(&mut self, then: &UiAction) {
        match *then {
            UiAction::WindowClose => self.win.exit = true,
            UiAction::TabClose(TabId(id)) => {
                if let Some(index) = self.win.workspace.tab_index(id) {
                    self.close_tab(index);
                }
            },
            UiAction::PaneClose(rmx_ui::PaneId(pane)) => self.close(pane),
            _ => (),
        }
    }

    /// Whether closing asks before it ends live sessions, on this device from now on.
    pub(super) fn set_confirm_close(&mut self, confirm: bool) {
        if self.core.confirm_close != confirm {
            self.core.confirm_close = confirm;
            self.core.vault.send(Request::SaveLayout(self.layout()));
        }
    }

    pub(super) fn ask_delete(&mut self, node: u64) {
        let Some(info) = self.core.ui.nodes.get(&node) else { return };
        let folder = info.kind == NodeKind::Folder;
        let name = info.name.clone();
        let (title, detail, reply) = if folder {
            (
                format!("Delete the folder {name}?"),
                if info.login {
                    "What is in it moves up to the top of the tree, and no longer inherits its login."
                } else {
                    "What is in it moves up to the top of the tree."
                },
                Reply::DeleteFolder(node),
            )
        } else {
            (
                format!("Delete {name}?"),
                "Its saved password and port forwards go with it.",
                Reply::DeleteConnection(node),
            )
        };
        let mut state = PromptState::new(title, detail, Vec::new());
        state.submit = "Delete".to_owned();
        self.ask_here(state, reply);
    }

    /// Ask for the login folder `node` hands down, filled in with the one it has.
    pub(super) fn ask_folder_login(
        &mut self,
        node: u64,
        folder: &str,
        username: String,
        password: bool,
        key: String,
    ) {
        let field =
            |label: &str, secret, value| PromptField { label: label.to_owned(), secret, value };
        let fields = vec![
            field("Username", false, username),
            field("Password", true, String::new()),
            field("Key file", false, key),
        ];
        let keep = if password { " Leave the password empty to keep the saved one." } else { "" };
        let detail = format!(
            "Connections in {folder} and the folders inside it use this login, unless they have \
             their own or a nearer folder has one.{keep}"
        );
        let mut state = PromptState::new(format!("Login for {folder}"), detail, fields);
        state.submit = "Save".to_owned();
        self.ask_here(state, Reply::FolderLogin(node));
    }

    pub(super) fn ask_folder_name(&mut self, title: &str, value: String, reply: Reply) {
        let field = PromptField { label: String::new(), secret: false, value };
        self.ask_here(PromptState::new(title, "", vec![field]), reply);
    }
}
