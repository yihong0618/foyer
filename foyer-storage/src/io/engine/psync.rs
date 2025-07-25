// Copyright 2025 foyer Project Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::{
    fmt::Debug,
    fs::File,
    mem::ManuallyDrop,
    ops::{Deref, DerefMut},
    sync::Arc,
};

use futures_util::FutureExt;

use crate::{
    io::{
        bytes::{IoB, IoBuf, IoBufMut},
        device::{Device, Partition},
        engine::{IoEngine, IoEngineBuilder, IoHandle},
        error::{IoError, IoResult},
    },
    RawFile, Runtime,
};

#[derive(Debug)]
struct FileHandle(ManuallyDrop<File>);

#[cfg(target_family = "windows")]
impl From<RawFile> for FileHandle {
    fn from(raw: RawFile) -> Self {
        use std::os::windows::io::FromRawHandle;
        let file = unsafe { File::from_raw_handle(raw.0) };
        let file = ManuallyDrop::new(file);
        Self(file)
    }
}

#[cfg(target_family = "unix")]
impl From<RawFile> for FileHandle {
    fn from(raw: RawFile) -> Self {
        use std::os::unix::io::FromRawFd;
        let file = unsafe { File::from_raw_fd(raw.0) };
        let file = ManuallyDrop::new(file);
        Self(file)
    }
}

impl Deref for FileHandle {
    type Target = File;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for FileHandle {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

/// Builder for synchronous I/O engine with pread(2)/pwrite(2).
#[derive(Debug)]
pub struct PsyncIoEngineBuilder;

impl Default for PsyncIoEngineBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl PsyncIoEngineBuilder {
    /// Create a new synchronous I/O engine builder with default configurations.
    pub fn new() -> Self {
        Self
    }
}

impl IoEngineBuilder for PsyncIoEngineBuilder {
    fn build(self: Box<Self>, device: Arc<dyn Device>, runtime: Runtime) -> IoResult<Arc<dyn IoEngine>> {
        let engine = PsyncIoEngine { device, runtime };
        let engine = Arc::new(engine);
        Ok(engine)
    }
}

pub struct PsyncIoEngine {
    device: Arc<dyn Device>,
    runtime: Runtime,
}

impl Debug for PsyncIoEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PsyncIoEngine").finish()
    }
}

impl IoEngine for PsyncIoEngine {
    fn device(&self) -> &Arc<dyn Device> {
        &self.device
    }

    fn read(&self, buf: Box<dyn IoBufMut>, partition: &dyn Partition, offset: u64) -> IoHandle {
        let (raw, offset) = partition.translate(offset);
        let (ptr, len) = buf.as_raw_parts();
        let slice = unsafe { std::slice::from_raw_parts_mut(ptr, len) };
        let file = FileHandle::from(raw);
        let runtime = self.runtime.clone();
        async move {
            let res = match runtime
                .read()
                .spawn_blocking(move || {
                    #[cfg(target_family = "windows")]
                    {
                        use std::os::windows::fs::FileExt;
                        file.seek_read(slice, offset).map_err(IoError::from)?;
                    };
                    #[cfg(target_family = "unix")]
                    {
                        use std::os::unix::fs::FileExt;
                        file.read_exact_at(slice, offset).map_err(IoError::from)?;
                    };
                    Ok(())
                })
                .await
            {
                Ok(res) => res,
                Err(e) => Err(IoError::other(e)),
            };
            let buf: Box<dyn IoB> = buf.into_iob();
            (buf, res)
        }
        .boxed()
        .into()
    }

    fn write(&self, buf: Box<dyn IoBuf>, partition: &dyn Partition, offset: u64) -> IoHandle {
        let (raw, offset) = partition.translate(offset);
        let (ptr, len) = buf.as_raw_parts();
        let slice = unsafe { std::slice::from_raw_parts(ptr, len) };
        let file = FileHandle::from(raw);
        let runtime = self.runtime.clone();
        async move {
            let res = match runtime
                .write()
                .spawn_blocking(move || {
                    #[cfg(target_family = "windows")]
                    {
                        use std::os::windows::fs::FileExt;
                        file.seek_write(slice, offset).map_err(IoError::from)?;
                    };
                    #[cfg(target_family = "unix")]
                    {
                        use std::os::unix::fs::FileExt;
                        file.write_all_at(slice, offset).map_err(IoError::from)?;
                    };
                    Ok(())
                })
                .await
            {
                Ok(res) => res,
                Err(e) => Err(IoError::other(e)),
            };
            let buf: Box<dyn IoB> = buf.into_iob();
            (buf, res)
        }
        .boxed()
        .into()
    }
}
