//! Logins that folders hand down to the connections inside them.
//!
//! The rules, which every device applies to whatever rows it has, so that merged replicas agree:
//!
//! - A connection that sets any login of its own ([`Connection::has_own_login`]) uses only that.
//!   Otherwise it takes the [`FolderLogin`] of the nearest folder above it that has a non-empty
//!   one, whole: username, credential and key come from the same folder, never mixed.
//! - Inheritance is worked out when it is needed, not copied into the connection. Moving a
//!   connection or a folder therefore changes what it inherits to whatever its new place hands
//!   down, and changing a folder's login reaches every connection below it at once.
//! - A folder that is gone ends the walk as the root does: connections in a deleted folder, or in
//!   a folder whose parent was deleted on another device, inherit nothing from above it. A folder's
//!   login row outliving its folder hands down nothing.
//! - A cycle of parents, which concurrent moves on two devices can make, ends the walk too.

use std::collections::HashMap;

use crate::records::{Connection, Folder, FolderLogin, Id};

/// The login a connection in `folder` inherits: that of the nearest folder, itself included, with a
/// non-empty [`FolderLogin`].
pub fn nearest_login<'a>(
    folder: Option<Id>,
    folders: &[Folder],
    logins: &'a [FolderLogin],
) -> Option<&'a FolderLogin> {
    let parents: HashMap<Id, Option<Id>> = folders.iter().map(|f| (f.id, f.parent_id)).collect();
    let logins: HashMap<Id, &FolderLogin> =
        logins.iter().filter(|l| !l.is_empty()).map(|l| (l.id, l)).collect();
    let mut at = folder;
    for _ in 0..=folders.len() {
        let id = at?;
        let parent = parents.get(&id)?;
        if let Some(login) = logins.get(&id) {
            return Some(login);
        }
        at = *parent;
    }
    None
}

impl Connection {
    /// This connection with `login` filled in, unless it has a login of its own.
    pub fn inheriting(mut self, login: Option<&FolderLogin>) -> Self {
        if let Some(login) = login
            && !self.has_own_login()
        {
            self.username.clone_from(&login.username);
            self.credential_id = login.credential_id;
            self.ssh_key_id = login.ssh_key_id;
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(parent: Option<&Folder>) -> Folder {
        Folder { parent_id: parent.map(|p| p.id), ..Default::default() }
    }

    fn login(folder: &Folder, user: &str) -> FolderLogin {
        FolderLogin { username: Some(user.to_owned()), ..FolderLogin::new(folder.id) }
    }

    fn user(found: Option<&FolderLogin>) -> Option<&str> {
        found.and_then(|l| l.username.as_deref())
    }

    #[test]
    fn the_nearest_folder_with_a_login_wins() {
        let a = folder(None);
        let b = folder(Some(&a));
        let c = folder(Some(&b));
        let folders = [a.clone(), b.clone(), c.clone()];
        let logins = [login(&a, "a"), FolderLogin::new(b.id)];
        assert_eq!(user(nearest_login(Some(c.id), &folders, &logins)), Some("a"));
        let logins = [login(&a, "a"), login(&b, "b")];
        assert_eq!(user(nearest_login(Some(c.id), &folders, &logins)), Some("b"));
        assert_eq!(user(nearest_login(Some(b.id), &folders, &logins)), Some("b"));
        assert_eq!(user(nearest_login(None, &folders, &logins)), None);
    }

    #[test]
    fn a_missing_folder_is_the_root() {
        let a = folder(None);
        let b = folder(Some(&a));
        let logins = [login(&a, "a"), login(&b, "b")];
        // `a` deleted: `b` is at the root, and keeps its own login.
        let folders = [b.clone()];
        assert_eq!(user(nearest_login(Some(b.id), &folders, &logins)), Some("b"));
        // `b` deleted: what was in it is at the root, under no login.
        let folders = [a.clone()];
        assert_eq!(user(nearest_login(Some(b.id), &folders, &logins)), None);
    }

    #[test]
    fn a_cycle_ends_the_walk() {
        let mut x = folder(None);
        let y = folder(Some(&x));
        x.parent_id = Some(y.id);
        let folders = [x.clone(), y.clone()];
        assert_eq!(nearest_login(Some(x.id), &folders, &[]), None);
        let logins = [login(&y, "y")];
        assert_eq!(user(nearest_login(Some(x.id), &folders, &logins)), Some("y"));
    }

    #[test]
    fn a_login_of_its_own_is_kept_whole() {
        let a = folder(None);
        let full = FolderLogin {
            username: Some("admin".to_owned()),
            credential_id: Some(Id::new()),
            ssh_key_id: Some(Id::new()),
            ..FolderLogin::new(a.id)
        };
        let bare = Connection::default();
        let got = bare.clone().inheriting(Some(&full));
        assert_eq!(got.username, full.username);
        assert_eq!(got.credential_id, full.credential_id);
        assert_eq!(got.ssh_key_id, full.ssh_key_id);
        let own = Connection { credential_id: Some(Id::new()), ..bare };
        assert_eq!(own.clone().inheriting(Some(&full)), own);
    }
}
