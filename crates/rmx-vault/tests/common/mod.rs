#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rmx_vault::{
    BackendError, Credentials, KdfParams, RemoteObject, RemoteVersion, SyncBackend, Vault,
};

pub fn fast_kdf() -> KdfParams {
    KdfParams::new(5, 1, 1).unwrap()
}

pub fn creds() -> Credentials {
    Credentials::new("correct horse battery staple")
}

/// `count` replicas of one new vault: same identity and key, separate node ids.
pub fn replicas(dir: &Path, count: usize) -> Vec<Vault> {
    let first_path = dir.join("r0.rmxdb");
    let mut first = Vault::create(&first_path, &creds(), fast_kdf()).unwrap();
    let mut out = Vec::new();
    for i in 1..count {
        let path = dir.join(format!("r{i}.rmxdb"));
        first.snapshot(&path).unwrap();
        out.push(Vault::open(&path, &creds()).unwrap());
    }
    out.insert(0, first);
    out
}

/// A single-file snapshot of `vault` at `dir/name`.
pub fn snapshot_of(vault: &mut Vault, dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    vault.snapshot(&path).unwrap();
    path
}

type Hook = Box<dyn FnOnce(&MemoryBackend) + Send>;

/// An in-memory store with counter ETags and real compare-and-swap.
#[derive(Default)]
pub struct MemoryBackend {
    state: Mutex<State>,
    hook: Mutex<Option<Hook>>,
}

#[derive(Default)]
struct State {
    data: Option<Vec<u8>>,
    generation: u64,
    gets: u32,
    puts: u32,
    always_conflict: bool,
}

impl MemoryBackend {
    /// Runs `hook` once, just before the next `put_if_match` checks its ETag.
    pub fn before_next_put(&self, hook: impl FnOnce(&MemoryBackend) + Send + 'static) {
        *self.hook.lock().unwrap() = Some(Box::new(hook));
    }

    pub fn always_conflict(&self) {
        self.state.lock().unwrap().always_conflict = true;
    }

    pub fn counts(&self) -> (u32, u32) {
        let state = self.state.lock().unwrap();
        (state.gets, state.puts)
    }
}

impl SyncBackend for MemoryBackend {
    fn id(&self) -> String {
        "memory".to_owned()
    }

    fn head(&self) -> Result<Option<RemoteVersion>, BackendError> {
        let state = self.state.lock().unwrap();
        Ok(state.data.as_ref().map(|_| RemoteVersion(format!("\"{}\"", state.generation))))
    }

    fn get(&self) -> Result<Option<RemoteObject>, BackendError> {
        let mut state = self.state.lock().unwrap();
        state.gets += 1;
        let version = RemoteVersion(format!("\"{}\"", state.generation));
        Ok(state.data.clone().map(|data| RemoteObject { data, version }))
    }

    fn put_if_match(
        &self,
        data: &[u8],
        expected: Option<&RemoteVersion>,
    ) -> Result<Option<RemoteVersion>, BackendError> {
        let hook = self.hook.lock().unwrap().take();
        if let Some(hook) = hook {
            hook(self);
        }
        let mut state = self.state.lock().unwrap();
        let current =
            state.data.as_ref().map(|_| RemoteVersion(format!("\"{}\"", state.generation)));
        if state.always_conflict || current.as_ref() != expected {
            return Err(BackendError::Conflict);
        }
        state.generation += 1;
        state.puts += 1;
        state.data = Some(data.to_vec());
        Ok(Some(RemoteVersion(format!("\"{}\"", state.generation))))
    }
}
