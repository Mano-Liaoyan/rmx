//! The file operations SFTP sync needs, on `rmx-ssh` and `rmx-transfer`.

use std::sync::Arc;

use bytes::Bytes;
use rmx_ssh::{
    AuthMethod, ConnectConfig, ConnectOptions, HostConfig, HostKeyVerifier, NoPrompter, SecretString,
    SshClient,
};
use rmx_transfer::{Error as FsError, RemoteFs, SftpFs, SftpOptions, WriteStart, read_all};
use rmx_vault::{BackendError, RemoteFiles};
use tokio::runtime::Handle;

/// How to reach the server.
pub struct SftpTarget {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: SecretString,
}

/// A connected SFTP session, used from a thread that may block: never one of the runtime's.
pub struct SftpFiles {
    runtime: Handle,
    client: SshClient,
    fs: SftpFs,
}

fn transport(err: impl std::fmt::Display) -> BackendError {
    BackendError::Transport(err.to_string())
}

impl SftpFiles {
    /// Connect and log in. Blocks.
    pub fn connect(
        runtime: Handle,
        target: SftpTarget,
        verifier: Arc<dyn HostKeyVerifier>,
    ) -> Result<Self, BackendError> {
        let handle = runtime.clone();
        handle.block_on(async move {
            let auth = vec![AuthMethod::Password(Some(target.password))];
            let host = HostConfig::new(target.host, target.user, auth).with_port(target.port);
            let config = ConnectConfig {
                target: host,
                jumps: Vec::new(),
                verifier,
                prompter: Arc::new(NoPrompter),
                options: ConnectOptions::default(),
            };
            let client = rmx_ssh::connect(config).await.map_err(transport)?;
            let stream = client.open_sftp().await.map_err(transport)?;
            let fs = SftpFs::connect(stream, SftpOptions::default()).await.map_err(transport)?;
            Ok(Self { runtime, client, fs })
        })
    }
}

impl RemoteFiles for SftpFiles {
    fn read(&self, path: &str) -> Result<Option<Vec<u8>>, BackendError> {
        self.runtime.block_on(async {
            match read_all(&self.fs, path, 0, None).await {
                Ok(data) => Ok(Some(data)),
                Err(FsError::NotFound(_)) => Ok(None),
                Err(err) => Err(transport(err)),
            }
        })
    }

    fn write(&self, path: &str, data: &[u8]) -> Result<(), BackendError> {
        self.runtime.block_on(async {
            let mut writer =
                self.fs.write_range(path, WriteStart::Truncate).await.map_err(transport)?;
            writer.write(Bytes::copy_from_slice(data)).await.map_err(transport)?;
            writer.finish().await.map_err(transport)
        })
    }

    fn create_exclusive(&self, path: &str, data: &[u8]) -> Result<bool, BackendError> {
        self.runtime.block_on(self.fs.create_exclusive(path, data)).map_err(transport)
    }

    fn rename_replace(&self, from: &str, to: &str) -> Result<(), BackendError> {
        self.runtime.block_on(self.fs.rename(from, to)).map_err(transport)
    }

    fn remove(&self, path: &str) -> Result<(), BackendError> {
        match self.runtime.block_on(self.fs.remove_file(path)) {
            Ok(()) | Err(FsError::NotFound(_)) => Ok(()),
            Err(err) => Err(transport(err)),
        }
    }
}

impl Drop for SftpFiles {
    fn drop(&mut self) {
        let _ = self.fs.close();
        self.runtime.block_on(self.client.disconnect());
    }
}
