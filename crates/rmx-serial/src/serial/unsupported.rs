//! Where a platform has no serial backend yet.

use std::io;
use std::path::Path;

use super::{PortInfo, SerialConfig, SerialError};

pub(super) fn list_ports() -> Vec<PortInfo> {
    Vec::new()
}

pub(super) enum Device {}

impl Device {
    pub(super) fn open(_path: &Path, _config: SerialConfig) -> Result<Self, SerialError> {
        Err(SerialError::Unsupported)
    }

    pub(super) fn register(self) -> io::Result<Registered> {
        match self {}
    }
}

pub(super) enum Registered {}

impl Registered {
    pub(super) async fn read(&self, _buffer: &mut [u8]) -> io::Result<usize> {
        match *self {}
    }

    pub(super) async fn write(&self, _bytes: &[u8]) -> io::Result<usize> {
        match *self {}
    }
}
