//! Laws of the row-level merge, and what tombstones, history and HLC skew do through it.

mod common;

use common::{creds, fast_kdf, replicas, snapshot_of};
use rmx_vault::{
    Connection, Credential, Folder, FolderLogin, HISTORY_LIMIT, Id, KnownHost, Secret,
    SharedSetting, Vault,
};

struct Rng(u64);

impl Rng {
    fn next(&mut self, bound: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % bound
    }
}

fn random_edits(vault: &mut Vault, ids: &[Id], rng: &mut Rng) {
    for round in 0..40 {
        let id = ids[rng.next(ids.len() as u64) as usize];
        match rng.next(8) {
            0 | 1 => vault
                .put(&Folder {
                    id,
                    name: format!("f{round}-{}", rng.next(5)),
                    ..Default::default()
                })
                .unwrap(),
            2 | 3 => vault
                .put(&Connection {
                    id,
                    name: format!("c{}", rng.next(9)),
                    protocol: "ssh".to_owned(),
                    host: format!("h{round}"),
                    port: Some(22),
                    ..Default::default()
                })
                .unwrap(),
            4 => {
                vault.delete::<Folder>(id).unwrap();
            },
            5 => vault
                .put(&FolderLogin {
                    username: Some(format!("u{}", rng.next(5))),
                    credential_id: Some(ids[rng.next(ids.len() as u64) as usize]),
                    ..FolderLogin::new(id)
                })
                .unwrap(),
            6 => {
                vault.delete::<FolderLogin>(id).unwrap();
            },
            _ => {
                vault.delete::<Connection>(id).unwrap();
            },
        }
    }
}

/// Three replicas that diverged from a shared origin.
fn diverged(dir: &std::path::Path, seed: u64) -> Vec<Vault> {
    let mut vaults = replicas(dir, 3);
    let ids: Vec<Id> = (0..8).map(|_| Id::new()).collect();
    let mut rng = Rng(seed | 1);
    for vault in &mut vaults {
        random_edits(vault, &ids, &mut rng);
    }
    vaults
}

fn copy_replica(source: &mut Vault, dir: &std::path::Path, name: &str) -> Vault {
    let path = dir.join(name);
    source.snapshot(&path).unwrap();
    Vault::open(&path, &creds()).unwrap()
}

#[test]
fn merge_is_commutative() {
    for seed in 1..=8 {
        let dir = tempfile::tempdir().unwrap();
        let mut v = diverged(dir.path(), seed * 7919);
        let (a, b) =
            (snapshot_of(&mut v[0], dir.path(), "a"), snapshot_of(&mut v[1], dir.path(), "b"));
        let mut left = copy_replica(&mut v[0], dir.path(), "left");
        let mut right = copy_replica(&mut v[1], dir.path(), "right");
        left.merge_snapshot(&b).unwrap();
        right.merge_snapshot(&a).unwrap();
        assert_eq!(left.digest().unwrap(), right.digest().unwrap(), "seed {seed}");
        assert_eq!(left.list::<Folder>().unwrap(), right.list::<Folder>().unwrap());
        assert_eq!(left.list::<Connection>().unwrap(), right.list::<Connection>().unwrap());
        assert_eq!(left.list::<FolderLogin>().unwrap(), right.list::<FolderLogin>().unwrap());
    }
}

#[test]
fn merge_is_associative() {
    for seed in 1..=4 {
        let dir = tempfile::tempdir().unwrap();
        let mut v = diverged(dir.path(), seed * 104_729);
        let b = snapshot_of(&mut v[1], dir.path(), "b");
        let c = snapshot_of(&mut v[2], dir.path(), "c");
        // (A + B) + C
        let mut left = copy_replica(&mut v[0], dir.path(), "left");
        left.merge_snapshot(&b).unwrap();
        left.merge_snapshot(&c).unwrap();
        // A + (B + C)
        let mut bc = copy_replica(&mut v[1], dir.path(), "bc");
        bc.merge_snapshot(&c).unwrap();
        let bc_snapshot = snapshot_of(&mut bc, dir.path(), "bc.snap");
        let mut right = copy_replica(&mut v[0], dir.path(), "right");
        right.merge_snapshot(&bc_snapshot).unwrap();
        assert_eq!(left.digest().unwrap(), right.digest().unwrap(), "seed {seed}");
    }
}

#[test]
fn merge_is_idempotent() {
    for seed in 1..=8 {
        let dir = tempfile::tempdir().unwrap();
        let mut v = diverged(dir.path(), seed * 15_485_863);
        let b = snapshot_of(&mut v[1], dir.path(), "b");
        let first = v[0].merge_snapshot(&b).unwrap();
        let digest = v[0].digest().unwrap();
        assert!(first.changed);
        let again = v[0].merge_snapshot(&b).unwrap();
        assert!(!again.changed, "seed {seed}");
        assert_eq!(v[0].digest().unwrap(), digest, "seed {seed}");
        // Merging a replica with itself is also a no-op.
        let own = snapshot_of(&mut v[0], dir.path(), "own");
        assert!(!v[0].merge_snapshot(&own).unwrap().changed, "seed {seed}");
    }
}

#[test]
fn identical_hlc_with_different_content_still_commutes() {
    let dir = tempfile::tempdir().unwrap();
    let mut v = replicas(dir.path(), 2);
    let id = Id::new();
    // Same node id and a frozen clock force the two writes to carry the same HLC.
    let (a_path, b_path) = (v[0].path().to_owned(), v[1].path().to_owned());
    v.clear();
    std::fs::copy(
        rmx_vault::LocalState::path_for(&a_path),
        rmx_vault::LocalState::path_for(&b_path),
    )
    .unwrap();
    let mut a = Vault::open(&a_path, &creds()).unwrap();
    let mut b = Vault::open(&b_path, &creds()).unwrap();
    a.set_time_source(|| 42_000).unwrap();
    b.set_time_source(|| 42_000).unwrap();
    a.put(&Folder { id, name: "left".to_owned(), ..Default::default() }).unwrap();
    b.put(&Folder { id, name: "right".to_owned(), ..Default::default() }).unwrap();
    assert_eq!(
        a.revision::<Folder>(id).unwrap().unwrap().hlc,
        b.revision::<Folder>(id).unwrap().unwrap().hlc
    );

    let (sa, sb) = (snapshot_of(&mut a, dir.path(), "sa"), snapshot_of(&mut b, dir.path(), "sb"));
    a.merge_snapshot(&sb).unwrap();
    b.merge_snapshot(&sa).unwrap();
    assert_eq!(a.digest().unwrap(), b.digest().unwrap());
    assert_eq!(a.get::<Folder>(id).unwrap().unwrap().name, "right");
    assert_eq!(a.history::<Folder>(id).unwrap()[0].record.name, "left");
}

/// Two replicas holding one row written at wall time 1000, with separate write clocks.
fn shared_row(dir: &std::path::Path) -> (Vec<Vault>, Id) {
    let mut v = replicas(dir, 2);
    let id = Id::new();
    v[0].set_time_source(|| 1_000).unwrap();
    v[0].put(&Folder { id, name: "base".to_owned(), ..Default::default() }).unwrap();
    let base = snapshot_of(&mut v[0], dir, "base");
    v[1].merge_snapshot(&base).unwrap();
    (v, id)
}

#[test]
fn concurrent_delete_beats_an_older_edit() {
    let dir = tempfile::tempdir().unwrap();
    let (mut v, id) = shared_row(dir.path());
    v[0].set_time_source(|| 3_000).unwrap();
    v[1].set_time_source(|| 2_000).unwrap();
    v[1].put(&Folder { id, name: "older edit".to_owned(), ..Default::default() }).unwrap();
    assert!(v[0].delete::<Folder>(id).unwrap());

    let (a, b) = (snapshot_of(&mut v[0], dir.path(), "a"), snapshot_of(&mut v[1], dir.path(), "b"));
    v[0].merge_snapshot(&b).unwrap();
    v[1].merge_snapshot(&a).unwrap();
    assert_eq!(v[0].digest().unwrap(), v[1].digest().unwrap());
    for vault in &mut v {
        assert_eq!(vault.get::<Folder>(id).unwrap(), None);
        let names: Vec<String> =
            vault.history::<Folder>(id).unwrap().into_iter().map(|r| r.record.name).collect();
        assert_eq!(names, ["older edit", "base"], "deleted content stays recoverable");
    }
}

#[test]
fn concurrent_newer_edit_beats_a_delete() {
    let dir = tempfile::tempdir().unwrap();
    let (mut v, id) = shared_row(dir.path());
    v[0].set_time_source(|| 2_000).unwrap();
    v[1].set_time_source(|| 3_000).unwrap();
    assert!(v[0].delete::<Folder>(id).unwrap());
    v[1].put(&Folder { id, name: "newer edit".to_owned(), ..Default::default() }).unwrap();

    let (a, b) = (snapshot_of(&mut v[0], dir.path(), "a"), snapshot_of(&mut v[1], dir.path(), "b"));
    v[0].merge_snapshot(&b).unwrap();
    v[1].merge_snapshot(&a).unwrap();
    assert_eq!(v[0].digest().unwrap(), v[1].digest().unwrap());
    assert_eq!(v[0].get::<Folder>(id).unwrap().unwrap().name, "newer edit");
    assert_eq!(v[1].get::<Folder>(id).unwrap().unwrap().name, "newer edit");
}

#[test]
fn delete_after_seeing_the_edit_removes_the_row_everywhere() {
    let dir = tempfile::tempdir().unwrap();
    let mut v = replicas(dir.path(), 2);
    let id = Id::new();
    v[0].put(&Connection { id, name: "doomed".to_owned(), ..Default::default() }).unwrap();
    let s = snapshot_of(&mut v[0], dir.path(), "s1");
    v[1].merge_snapshot(&s).unwrap();
    assert!(v[1].delete::<Connection>(id).unwrap());
    let s = snapshot_of(&mut v[1], dir.path(), "s2");
    v[0].merge_snapshot(&s).unwrap();
    assert_eq!(v[0].get::<Connection>(id).unwrap(), None);
    assert_eq!(v[0].digest().unwrap(), v[1].digest().unwrap());
    let last = v[0].history::<Connection>(id).unwrap();
    assert_eq!(last[0].record.name, "doomed", "the deleted version stays recoverable");
}

#[test]
fn newer_edit_resurrects_a_row_and_clears_its_tombstone() {
    let dir = tempfile::tempdir().unwrap();
    let mut v = replicas(dir.path(), 2);
    let id = Id::new();
    v[0].put(&Folder { id, name: "base".to_owned(), ..Default::default() }).unwrap();
    let s = snapshot_of(&mut v[0], dir.path(), "s1");
    v[1].merge_snapshot(&s).unwrap();
    v[0].delete::<Folder>(id).unwrap();
    let s = snapshot_of(&mut v[0], dir.path(), "s2");
    v[1].merge_snapshot(&s).unwrap();
    assert_eq!(v[1].get::<Folder>(id).unwrap(), None);

    // B writes the id again after observing the delete, so its edit is causally newer.
    v[1].put(&Folder { id, name: "reborn".to_owned(), ..Default::default() }).unwrap();
    let s = snapshot_of(&mut v[1], dir.path(), "s3");
    v[0].merge_snapshot(&s).unwrap();
    assert_eq!(v[0].get::<Folder>(id).unwrap().unwrap().name, "reborn");
    assert_eq!(v[0].digest().unwrap(), v[1].digest().unwrap());
}

#[test]
fn deleting_a_missing_row_leaves_no_tombstone() {
    let dir = tempfile::tempdir().unwrap();
    let mut v = replicas(dir.path(), 1);
    let before = v[0].digest().unwrap();
    assert!(!v[0].delete::<Folder>(Id::new()).unwrap());
    assert_eq!(v[0].digest().unwrap(), before);
}

#[test]
fn history_keeps_superseded_versions_across_devices() {
    let dir = tempfile::tempdir().unwrap();
    let mut v = replicas(dir.path(), 2);
    let id = Id::new();
    for name in ["one", "two", "three"] {
        v[0].put(&Credential {
            id,
            name: name.to_owned(),
            secret: Secret::new(format!("pw-{name}")),
            ..Default::default()
        })
        .unwrap();
    }
    let s = snapshot_of(&mut v[0], dir.path(), "s");
    v[1].merge_snapshot(&s).unwrap();
    let names = |vault: &mut Vault| -> Vec<String> {
        vault.history::<Credential>(id).unwrap().into_iter().map(|r| r.record.name).collect()
    };
    assert_eq!(names(&mut v[0]), ["two", "one"]);
    assert_eq!(names(&mut v[1]), ["two", "one"]);
    let oldest = v[1].history::<Credential>(id).unwrap().pop().unwrap();
    assert_eq!(oldest.record.secret.expose(), "pw-one");

    // A remote winner pushes the local version into history.
    v[1].put(&Credential { id, name: "four".to_owned(), ..Default::default() }).unwrap();
    let s = snapshot_of(&mut v[1], dir.path(), "s2");
    v[0].merge_snapshot(&s).unwrap();
    assert_eq!(names(&mut v[0]), ["three", "two", "one"]);
    assert_eq!(v[0].digest().unwrap(), v[1].digest().unwrap());

    // Restoring writes the old content as a new version.
    let target = v[0].history::<Credential>(id).unwrap()[2].hlc;
    v[0].restore::<Credential>(id, target).unwrap();
    assert_eq!(v[0].get::<Credential>(id).unwrap().unwrap().name, "one");
    assert_eq!(names(&mut v[0])[0], "four");
}

#[test]
fn identical_write_is_a_no_op() {
    let dir = tempfile::tempdir().unwrap();
    let mut v = replicas(dir.path(), 1);
    let row = Folder { name: "same".to_owned(), ..Default::default() };
    v[0].put(&row).unwrap();
    let digest = v[0].digest().unwrap();
    v[0].put(&row).unwrap();
    assert_eq!(v[0].digest().unwrap(), digest);
    assert!(v[0].history::<Folder>(row.id).unwrap().is_empty());
}

#[test]
fn history_is_capped_and_the_cap_commutes() {
    let dir = tempfile::tempdir().unwrap();
    let mut v = replicas(dir.path(), 2);
    let id = Id::new();
    for i in 0..(HISTORY_LIMIT + 5) {
        v[0].put(&Folder { id, name: format!("a{i}"), ..Default::default() }).unwrap();
        v[1].put(&Folder { id, name: format!("b{i}"), ..Default::default() }).unwrap();
    }
    assert_eq!(v[0].history::<Folder>(id).unwrap().len(), HISTORY_LIMIT as usize);
    let (a, b) = (snapshot_of(&mut v[0], dir.path(), "a"), snapshot_of(&mut v[1], dir.path(), "b"));
    v[0].merge_snapshot(&b).unwrap();
    v[1].merge_snapshot(&a).unwrap();
    assert_eq!(v[0].digest().unwrap(), v[1].digest().unwrap());
    assert_eq!(v[0].history::<Folder>(id).unwrap().len(), HISTORY_LIMIT as usize);
}

#[test]
fn causally_later_write_wins_despite_clock_skew() {
    let dir = tempfile::tempdir().unwrap();
    let mut v = replicas(dir.path(), 2);
    let id = Id::new();
    // A's clock runs a year fast; B's sits at the Unix epoch plus a little.
    v[0].set_time_source(|| 1_800_000_000_000).unwrap();
    v[1].set_time_source(|| 5_000).unwrap();
    v[0].put(&Folder { id, name: "from the future".to_owned(), ..Default::default() }).unwrap();
    let s = snapshot_of(&mut v[0], dir.path(), "s1");
    v[1].merge_snapshot(&s).unwrap();

    v[1].put(&Folder { id, name: "reply".to_owned(), ..Default::default() }).unwrap();
    let s = snapshot_of(&mut v[1], dir.path(), "s2");
    v[0].merge_snapshot(&s).unwrap();
    assert_eq!(v[0].get::<Folder>(id).unwrap().unwrap().name, "reply");
    assert_eq!(v[0].digest().unwrap(), v[1].digest().unwrap());
}

#[test]
fn clock_stays_ahead_of_stored_data_after_reopening() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v.rmxdb");
    let id = Id::new();
    let hlc = {
        let mut v = Vault::create(&path, &creds(), fast_kdf()).unwrap();
        v.set_time_source(|| u64::MAX / 2).unwrap();
        v.put(&Folder { id, name: "future".to_owned(), ..Default::default() }).unwrap();
        v.revision::<Folder>(id).unwrap().unwrap().hlc
    };
    let mut reopened = Vault::open(&path, &creds()).unwrap();
    reopened.put(&Folder { id, name: "now".to_owned(), ..Default::default() }).unwrap();
    assert!(reopened.revision::<Folder>(id).unwrap().unwrap().hlc > hlc);
}

#[test]
fn deterministic_ids_coalesce_independent_rows() {
    let dir = tempfile::tempdir().unwrap();
    let mut v = replicas(dir.path(), 2);
    v[0].put(&KnownHost::new("example.org", 22, "ssh-ed25519", "AAAA")).unwrap();
    v[1].put(&KnownHost::new("example.org", 22, "ssh-ed25519", "AAAA")).unwrap();
    v[0].put(&SharedSetting::new("theme", "dark")).unwrap();
    v[1].put(&SharedSetting::new("theme", "light")).unwrap();
    let s = snapshot_of(&mut v[1], dir.path(), "s");
    v[0].merge_snapshot(&s).unwrap();
    assert_eq!(v[0].list::<KnownHost>().unwrap().len(), 1);
    assert_eq!(v[0].list::<SharedSetting>().unwrap().len(), 1);
}

#[test]
fn merging_a_different_vault_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let mut v = replicas(dir.path(), 1);
    let other_path = dir.path().join("other.rmxdb");
    let mut other = Vault::create(&other_path, &creds(), fast_kdf()).unwrap();
    let snap = snapshot_of(&mut other, dir.path(), "other.snap");
    assert!(matches!(v[0].merge_snapshot(&snap), Err(rmx_vault::Error::RemoteKeyChanged)));
}

#[test]
fn every_table_round_trips() {
    use rmx_vault::{Snippet, SshKey, Tunnel};
    let dir = tempfile::tempdir().unwrap();
    let mut v = replicas(dir.path(), 2);
    let folder = Folder { name: "a".to_owned(), sort_order: 3, ..Default::default() };
    let conn = Connection {
        folder_id: Some(folder.id),
        name: "c".to_owned(),
        protocol: "ssh".to_owned(),
        host: "h".to_owned(),
        port: Some(2222),
        username: Some("u".to_owned()),
        credential_id: Some(Id::new()),
        ssh_key_id: Some(Id::new()),
        jump_connection_id: None,
        color: Some("#fff".to_owned()),
        notes: Some("n\"o'te\n".to_owned()),
        options: Some("{\"x\":1}".to_owned()),
        sort_order: -4,
        ..Default::default()
    };
    let credential = Credential {
        name: "cr".to_owned(),
        username: Some("u".to_owned()),
        secret: Secret::new("pässwörd"),
        notes: None,
        ..Default::default()
    };
    let key = SshKey {
        name: "k".to_owned(),
        algorithm: "ssh-ed25519".to_owned(),
        public_key: "pub".to_owned(),
        private_key: Secret::new("-----BEGIN"),
        passphrase: Some(Secret::new("pp")),
        ..Default::default()
    };
    let snippet = Snippet {
        folder_id: None,
        name: "s".to_owned(),
        body: "echo hi".to_owned(),
        ..Default::default()
    };
    let tunnel = Tunnel {
        connection_id: conn.id,
        name: "t".to_owned(),
        kind: "local".to_owned(),
        bind_host: "127.0.0.1".to_owned(),
        bind_port: 8080,
        target_host: Some("x".to_owned()),
        target_port: Some(80),
        autostart: true,
        ..Default::default()
    };
    v[0].put(&folder).unwrap();
    v[0].put(&conn).unwrap();
    v[0].put(&credential).unwrap();
    v[0].put(&key).unwrap();
    v[0].put(&snippet).unwrap();
    v[0].put(&tunnel).unwrap();
    let login = FolderLogin {
        username: Some("admin".to_owned()),
        credential_id: Some(credential.id),
        ssh_key_id: Some(key.id),
        ..FolderLogin::new(folder.id)
    };
    v[0].put(&login).unwrap();
    let s = snapshot_of(&mut v[0], dir.path(), "s");
    v[1].merge_snapshot(&s).unwrap();
    assert_eq!(v[1].get::<Folder>(folder.id).unwrap().unwrap(), folder);
    assert_eq!(v[1].get::<Connection>(conn.id).unwrap().unwrap(), conn);
    assert_eq!(v[1].get::<Credential>(credential.id).unwrap().unwrap(), credential);
    assert_eq!(v[1].get::<SshKey>(key.id).unwrap().unwrap(), key);
    assert_eq!(v[1].get::<Snippet>(snippet.id).unwrap().unwrap(), snippet);
    assert_eq!(v[1].get::<Tunnel>(tunnel.id).unwrap().unwrap(), tunnel);
    assert_eq!(v[1].get::<FolderLogin>(folder.id).unwrap().unwrap(), login);

    // History decodes payloads back through JSON, including nulls, quotes and booleans.
    v[0].put(&Connection { name: "c2".to_owned(), ..conn.clone() }).unwrap();
    v[0].put(&Tunnel { name: "t2".to_owned(), ..tunnel.clone() }).unwrap();
    assert_eq!(v[0].history::<Connection>(conn.id).unwrap()[0].record, conn);
    assert_eq!(v[0].history::<Tunnel>(tunnel.id).unwrap()[0].record, tunnel);
}

/// Production > Web > a connection with no login of its own, and one beside it that has one.
struct LoginTree {
    production: Folder,
    web: Folder,
    inheriting: Connection,
    own: Connection,
    login: FolderLogin,
}

fn login_tree(vault: &mut Vault) -> LoginTree {
    let production = Folder { name: "Production".to_owned(), ..Default::default() };
    let web =
        Folder { parent_id: Some(production.id), name: "Web".to_owned(), ..Default::default() };
    let secret =
        Credential { name: "prod".to_owned(), secret: Secret::new("s3"), ..Default::default() };
    let login = FolderLogin {
        username: Some("admin".to_owned()),
        credential_id: Some(secret.id),
        ..FolderLogin::new(production.id)
    };
    let inheriting =
        Connection { folder_id: Some(web.id), name: "web1".to_owned(), ..Default::default() };
    let own = Connection {
        folder_id: Some(web.id),
        name: "web2".to_owned(),
        username: Some("me".to_owned()),
        ..Default::default()
    };
    vault.put(&production).unwrap();
    vault.put(&web).unwrap();
    vault.put(&secret).unwrap();
    vault.put(&login).unwrap();
    vault.put(&inheriting).unwrap();
    vault.put(&own).unwrap();
    LoginTree { production, web, inheriting, own, login }
}

#[test]
fn inherited_logins_survive_sync_between_devices() {
    let dir = tempfile::tempdir().unwrap();
    let mut v = replicas(dir.path(), 2);
    let tree = login_tree(&mut v[0]);
    let s = snapshot_of(&mut v[0], dir.path(), "s1");
    v[1].merge_snapshot(&s).unwrap();

    let got = v[1].with_inherited_login(tree.inheriting.clone()).unwrap();
    assert_eq!(got.username.as_deref(), Some("admin"));
    assert_eq!(got.credential_id, tree.login.credential_id);
    assert_eq!(v[1].inherited_login(tree.inheriting.folder_id).unwrap(), Some(tree.login.clone()));
    // A login of its own is never mixed with the folder's.
    assert_eq!(v[1].with_inherited_login(tree.own.clone()).unwrap(), tree.own);

    // The other device gives the nearer folder a login: it wins on both, once synced back.
    let web_login =
        FolderLogin { username: Some("deploy".to_owned()), ..FolderLogin::new(tree.web.id) };
    v[1].put(&web_login).unwrap();
    let s = snapshot_of(&mut v[1], dir.path(), "s2");
    v[0].merge_snapshot(&s).unwrap();
    assert_eq!(v[0].digest().unwrap(), v[1].digest().unwrap());
    for vault in &mut v {
        let got = vault.with_inherited_login(tree.inheriting.clone()).unwrap();
        assert_eq!(got.username.as_deref(), Some("deploy"));
        assert_eq!(got.credential_id, None, "the nearest login is taken whole");
    }

    // Clearing it on one device hands Production's back down everywhere.
    v[0].delete::<FolderLogin>(tree.web.id).unwrap();
    let s = snapshot_of(&mut v[0], dir.path(), "s3");
    v[1].merge_snapshot(&s).unwrap();
    let got = v[1].with_inherited_login(tree.inheriting.clone()).unwrap();
    assert_eq!(got.username.as_deref(), Some("admin"));
}

#[test]
fn a_deleted_folder_or_a_moved_connection_changes_what_is_inherited() {
    let dir = tempfile::tempdir().unwrap();
    let mut v = replicas(dir.path(), 2);
    let tree = login_tree(&mut v[0]);
    let s = snapshot_of(&mut v[0], dir.path(), "s1");
    v[1].merge_snapshot(&s).unwrap();

    // Moved to the root on one device: nothing to inherit there.
    let moved = Connection { folder_id: None, ..tree.inheriting.clone() };
    v[1].put(&moved).unwrap();
    // Production deleted on the other: Web is orphaned and Production's login no longer applies.
    v[0].delete::<Folder>(tree.production.id).unwrap();
    let (a, b) = (snapshot_of(&mut v[0], dir.path(), "a"), snapshot_of(&mut v[1], dir.path(), "b"));
    v[0].merge_snapshot(&b).unwrap();
    v[1].merge_snapshot(&a).unwrap();
    assert_eq!(v[0].digest().unwrap(), v[1].digest().unwrap());
    for vault in &mut v {
        assert_eq!(vault.with_inherited_login(moved.clone()).unwrap(), moved);
        assert_eq!(vault.inherited_login(Some(tree.web.id)).unwrap(), None);
    }
}

/// Rewrites the plain vault file at `path` into what rmx wrote before folder logins existed.
fn make_schema_v1(path: &std::path::Path) {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute_batch(
        "DROP TABLE folder_logins;
         UPDATE meta SET value = '1' WHERE key = 'schema_version';",
    )
    .unwrap();
}

fn schema_version(path: &std::path::Path) -> String {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.query_row("SELECT value FROM meta WHERE key = 'schema_version'", [], |r| r.get(0)).unwrap()
}

#[test]
fn an_old_schema_database_opens_and_merges() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("new.rmxdb");
    let mut new = Vault::create_plain(&path).unwrap();
    let folder = Folder { name: "kept".to_owned(), ..Default::default() };
    new.put(&folder).unwrap();
    let old_path = snapshot_of(&mut new, dir.path(), "old.rmxdb");
    make_schema_v1(&old_path);
    assert_eq!(schema_version(&old_path), "1");

    // A copy from a device not yet upgraded merges, adding nothing, and folder logins stay.
    new.put(&FolderLogin { username: Some("u".to_owned()), ..FolderLogin::new(folder.id) })
        .unwrap();
    assert!(!new.merge_snapshot(&old_path).unwrap().changed);
    assert!(new.get::<FolderLogin>(folder.id).unwrap().is_some());

    // An old working copy opens, is upgraded in place, and takes folder logins.
    let mut old = Vault::open_plain(&old_path).unwrap();
    assert_eq!(old.get::<Folder>(folder.id).unwrap().unwrap(), folder);
    assert!(old.list::<FolderLogin>().unwrap().is_empty());
    old.put(&FolderLogin { username: Some("v".to_owned()), ..FolderLogin::new(folder.id) })
        .unwrap();
    drop(old);
    assert_eq!(schema_version(&old_path), rmx_vault::SCHEMA_VERSION.to_string());
}

#[test]
fn an_old_and_a_new_replica_with_the_same_rows_agree() {
    let dir = tempfile::tempdir().unwrap();
    let mut new = Vault::create_plain(&dir.path().join("new.rmxdb")).unwrap();
    new.put(&Folder { name: "a".to_owned(), ..Default::default() }).unwrap();
    let before = new.digest().unwrap();
    let old_path = snapshot_of(&mut new, dir.path(), "old.rmxdb");
    make_schema_v1(&old_path);
    // Nothing to send: a sync would not upload just to add an empty table.
    assert!(!new.merge_snapshot(&old_path).unwrap().changed);
    assert_eq!(new.digest().unwrap(), before);
    let mut old = Vault::open_plain(&old_path).unwrap();
    assert_eq!(old.digest().unwrap(), before);
}

#[test]
fn a_newer_schema_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let mut vault = Vault::create_plain(&dir.path().join("v.rmxdb")).unwrap();
    let future = snapshot_of(&mut vault, dir.path(), "future.rmxdb");
    let conn = rusqlite::Connection::open(&future).unwrap();
    conn.execute("UPDATE meta SET value = '99' WHERE key = 'schema_version'", []).unwrap();
    drop(conn);
    assert!(matches!(
        vault.merge_snapshot(&future),
        Err(rmx_vault::Error::UnsupportedFormat { found: 99, .. })
    ));
    drop(vault);
    assert!(matches!(
        Vault::open_plain(&future),
        Err(rmx_vault::Error::UnsupportedFormat { found: 99, .. })
    ));
}
