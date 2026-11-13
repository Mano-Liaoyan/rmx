//! Typed rows of the synced tables.

use std::fmt;
use std::str::FromStr;

use ring::digest::{self, SHA256};
use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use uuid::Uuid;

use crate::secret::Secret;

/// A row's UUID, stored as hyphenated text.
///
/// `Default` is a fresh random id, so `..Default::default()` builds a new row.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Id(Uuid);

impl Id {
    /// A fresh random id.
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// The same id on every device for the same `parts`, so independently created rows that
    /// mean the same thing merge instead of duplicating.
    pub fn derived(parts: &[&str]) -> Self {
        let mut ctx = digest::Context::new(&SHA256);
        for part in parts {
            ctx.update(&(part.len() as u64).to_le_bytes());
            ctx.update(part.as_bytes());
        }
        let mut bytes = [0u8; 16];
        bytes.copy_from_slice(&ctx.finish().as_ref()[..16]);
        Self(Uuid::new_v8(bytes))
    }
}

impl Default for Id {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl fmt::Debug for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl FromStr for Id {
    type Err = uuid::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Uuid::parse_str(s).map(Self)
    }
}

impl ToSql for Id {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.0.hyphenated().to_string()))
    }
}

impl FromSql for Id {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        value.as_str()?.parse().map_err(|e: uuid::Error| FromSqlError::Other(Box::new(e)))
    }
}

/// Column list of one synced table.
#[derive(Debug)]
pub(crate) struct TableDef {
    pub name: &'static str,
    /// Payload columns as `(name, declaration)`, excluding `id` and `hlc`.
    pub columns: &'static [(&'static str, &'static str)],
}

impl TableDef {
    /// `alias.col1, alias.col2, ...`; `alias` empty means unqualified.
    pub fn column_list(&self, alias: &str) -> String {
        let prefix = if alias.is_empty() { String::new() } else { format!("{alias}.") };
        self.columns.iter().map(|(c, _)| format!("{prefix}{c}")).collect::<Vec<_>>().join(", ")
    }

    /// SQL for the canonical JSON payload of a row.
    pub fn payload_expr(&self, alias: &str) -> String {
        format!("json_array({})", self.column_list(alias))
    }
}

pub(crate) trait SqlColumn {
    const DECL: &'static str;
}

macro_rules! sql_column {
    ($($ty:ty => $not_null:literal, $nullable:literal;)*) => {$(
        impl SqlColumn for $ty {
            const DECL: &'static str = $not_null;
        }
        impl SqlColumn for Option<$ty> {
            const DECL: &'static str = $nullable;
        }
    )*};
}

sql_column! {
    String => "TEXT NOT NULL", "TEXT";
    Secret => "TEXT NOT NULL", "TEXT";
    Id => "TEXT NOT NULL", "TEXT";
    i64 => "INTEGER NOT NULL", "INTEGER";
    u16 => "INTEGER NOT NULL", "INTEGER";
    bool => "INTEGER NOT NULL", "INTEGER";
}

mod sealed {
    pub trait Sealed {}
}

/// A row type of a synced table. Sealed; implemented for each table below.
pub trait Record: sealed::Sealed + Sized {
    #[doc(hidden)]
    #[allow(private_interfaces)]
    const TABLE: &'static TableDef;

    /// The row's UUID.
    fn id(&self) -> Id;

    #[doc(hidden)]
    fn values(&self) -> Vec<&dyn ToSql>;

    #[doc(hidden)]
    fn from_row(id: Id, row: &rusqlite::Row<'_>, offset: usize) -> rusqlite::Result<Self>;
}

macro_rules! records {
    ($(
        $(#[$meta:meta])*
        $name:ident => $table:literal {
            $($(#[$fmeta:meta])* $field:ident : $ty:ty,)*
        }
    )*) => {
        $(
            $(#[$meta])*
            #[derive(Clone, Debug, Default, PartialEq, Eq)]
            pub struct $name {
                /// The row's UUID.
                pub id: Id,
                $($(#[$fmeta])* pub $field: $ty,)*
            }

            impl sealed::Sealed for $name {}

            impl Record for $name {
                #[allow(private_interfaces)]
                const TABLE: &'static TableDef = &TableDef {
                    name: $table,
                    columns: &[$((stringify!($field), <$ty as SqlColumn>::DECL),)*],
                };

                fn id(&self) -> Id {
                    self.id
                }

                fn values(&self) -> Vec<&dyn ToSql> {
                    vec![$(&self.$field as &dyn ToSql,)*]
                }

                #[allow(unused_assignments, unused_mut, unused_variables)]
                fn from_row(id: Id, row: &rusqlite::Row<'_>, offset: usize) -> rusqlite::Result<Self> {
                    let mut index = offset;
                    $(
                        let $field = row.get(index)?;
                        index += 1;
                    )*
                    Ok(Self { id, $($field,)* })
                }
            }
        )*

        /// Every synced table, in merge order.
        pub(crate) const TABLES: &[&TableDef] = &[$(<$name as Record>::TABLE,)*];
    };
}

records! {
    /// A node of the connection tree.
    ///
    /// Rows merge independently, so a parent may be gone on another device: treat a missing
    /// parent as the root.
    Folder => "folders" {
        /// Containing folder; `None` is the root.
        parent_id: Option<Id>,
        /// Display name.
        name: String,
        /// Position among siblings.
        sort_order: i64,
    }

    /// A saved remote connection.
    Connection => "connections" {
        /// Containing folder; `None` or a missing folder is the root.
        folder_id: Option<Id>,
        /// Display name.
        name: String,
        /// `ssh`, `sftp`, `ftp`, `vnc`, `rdp`, `telnet` or `serial`.
        protocol: String,
        /// Host name, address or device path.
        host: String,
        /// Port; `None` is the protocol default.
        port: Option<u16>,
        /// Login name, when not taken from the credential.
        username: Option<String>,
        /// Stored credential to log in with.
        credential_id: Option<Id>,
        /// Stored SSH key to log in with.
        ssh_key_id: Option<Id>,
        /// Connection to jump through.
        jump_connection_id: Option<Id>,
        /// Marker color.
        color: Option<String>,
        /// Free-form notes.
        notes: Option<String>,
        /// Protocol-specific options as JSON, owned by the caller.
        options: Option<String>,
        /// Position among siblings.
        sort_order: i64,
    }

    /// A reusable login.
    Credential => "credentials" {
        /// Display name.
        name: String,
        /// Login name.
        username: Option<String>,
        /// The password or token.
        secret: Secret,
        /// Free-form notes.
        notes: Option<String>,
    }

    /// A stored SSH key pair.
    SshKey => "ssh_keys" {
        /// Display name.
        name: String,
        /// Key algorithm, such as `ssh-ed25519`.
        algorithm: String,
        /// Public key in OpenSSH format.
        public_key: String,
        /// Private key in OpenSSH or PEM format.
        private_key: Secret,
        /// Passphrase protecting the private key.
        passphrase: Option<Secret>,
    }

    /// A trusted server host key. Build with [`KnownHost::new`] so devices agree on the id.
    KnownHost => "known_hosts" {
        /// Host name or address.
        host: String,
        /// Port.
        port: u16,
        /// Key algorithm.
        key_type: String,
        /// Base64 public key.
        public_key: String,
    }

    /// A reusable command snippet.
    Snippet => "snippets" {
        /// Containing folder.
        folder_id: Option<Id>,
        /// Display name.
        name: String,
        /// The text to send.
        body: String,
    }

    /// A saved port forward.
    Tunnel => "tunnels" {
        /// The connection it runs over.
        connection_id: Id,
        /// Display name.
        name: String,
        /// `local`, `remote` or `dynamic`.
        kind: String,
        /// Listen address.
        bind_host: String,
        /// Listen port.
        bind_port: u16,
        /// Destination host; `None` for dynamic forwards.
        target_host: Option<String>,
        /// Destination port; `None` for dynamic forwards.
        target_port: Option<u16>,
        /// Start with the connection.
        autostart: bool,
    }

    /// A setting that follows the user across devices. Build with [`SharedSetting::new`].
    SharedSetting => "shared_settings" {
        /// Setting name.
        key: String,
        /// Setting value.
        value: String,
    }

    /// The login a folder hands down to the connections inside it, at any depth. Its id is the
    /// folder's: build it with [`FolderLogin::new`]. See [`crate::inherit`] for the rules.
    ///
    /// Added in schema version 2.
    FolderLogin => "folder_logins" {
        /// Login name, when not taken from the credential.
        username: Option<String>,
        /// Stored credential to log in with.
        credential_id: Option<Id>,
        /// Stored SSH key to log in with.
        ssh_key_id: Option<Id>,
    }
}

impl FolderLogin {
    /// An empty login for the folder `folder`.
    pub fn new(folder: Id) -> Self {
        Self { id: folder, username: None, credential_id: None, ssh_key_id: None }
    }

    /// Whether it sets nothing, and so hands nothing down.
    pub fn is_empty(&self) -> bool {
        self.username.is_none() && self.credential_id.is_none() && self.ssh_key_id.is_none()
    }
}

impl Connection {
    /// Whether it sets a login of its own (a username, a credential or a key), which keeps it from
    /// inheriting its folders'.
    pub fn has_own_login(&self) -> bool {
        self.username.is_some() || self.credential_id.is_some() || self.ssh_key_id.is_some()
    }
}

impl KnownHost {
    /// A host key whose id is the same on every device for this host, port and key type.
    pub fn new(host: &str, port: u16, key_type: &str, public_key: &str) -> Self {
        Self {
            id: Id::derived(&["known_host", host, &port.to_string(), key_type]),
            host: host.to_owned(),
            port,
            key_type: key_type.to_owned(),
            public_key: public_key.to_owned(),
        }
    }
}

impl SharedSetting {
    /// A setting whose id is the same on every device for this key.
    pub fn new(key: &str, value: &str) -> Self {
        Self {
            id: Id::derived(&["shared_setting", key]),
            key: key.to_owned(),
            value: value.to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_ids_are_stable_and_unambiguous() {
        assert_eq!(Id::derived(&["a", "b"]), Id::derived(&["a", "b"]));
        assert_ne!(Id::derived(&["ab", ""]), Id::derived(&["a", "b"]));
        assert_eq!(SharedSetting::new("k", "1").id, SharedSetting::new("k", "2").id);
        assert_ne!(KnownHost::new("h", 22, "t", "x").id, KnownHost::new("h", 23, "t", "x").id);
    }

    #[test]
    fn default_is_a_fresh_row() {
        assert_ne!(Folder::default().id, Folder::default().id);
    }

    #[test]
    fn secret_fields_do_not_appear_in_debug() {
        let row = Credential { secret: Secret::new("s3cr3t"), ..Default::default() };
        assert!(!format!("{row:?}").contains("s3cr3t"));
    }
}
