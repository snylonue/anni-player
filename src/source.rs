use std::{
    fs::{create_dir_all, File},
    io::{ErrorKind, Read, Seek, Write},
    path::Path,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    thread,
};

use anni_playback::types::MediaSource;
use anni_provider::providers::TypedPriorityProvider;
use anyhow::anyhow;

use reqwest::{blocking::Client, Url};

use crate::{identifier::TrackIdentifier, provider::ProviderProxy};

const BUF_SIZE: usize = 1024 * 64; // 64k

pub struct CachedHttpSource {
    identifier: TrackIdentifier,
    cache: File,
    buf_len: Arc<AtomicUsize>,
    pos: Arc<AtomicUsize>,
    is_buffering: Arc<AtomicBool>,
    #[allow(unused)]
    buffer_signal: Arc<AtomicBool>,
}

impl CachedHttpSource {
    /// `cache_path` is the path to cache file.
    pub fn new(
        identifier: TrackIdentifier,
        url: impl FnOnce() -> Option<Url>,
        cache_path: &Path,
        client: Client,
        buffer_signal: Arc<AtomicBool>,
    ) -> anyhow::Result<Self> {
        if cache_path.exists() {
            // todo: varify cache
            let cache = File::open(cache_path)?;
            let buf_len = cache.metadata()?.len() as usize;
            Ok(Self {
                identifier,
                cache,
                buf_len: Arc::new(AtomicUsize::new(buf_len)),
                pos: Arc::new(AtomicUsize::new(0)),
                is_buffering: Arc::new(AtomicBool::new(false)),
                buffer_signal,
            })
        } else {
            create_parent(cache_path)?;

            let cache = File::options()
                .read(true)
                .append(true)
                .create(true)
                .open(cache_path)?;

            let buf_len = Arc::new(AtomicUsize::new(0));
            let is_buffering = Arc::new(AtomicBool::new(true));
            let pos = Arc::new(AtomicUsize::new(0));

            thread::spawn({
                let mut response = client.get(url().ok_or(anyhow!("no audio"))?).send()?;

                let mut cache = cache.try_clone()?;
                let buf_len = Arc::clone(&buf_len);
                let pos = Arc::clone(&pos);
                let mut buf = [0; BUF_SIZE];
                let is_buffering = Arc::clone(&is_buffering);

                move || loop {
                    match response.read(&mut buf) {
                        Ok(0) => {
                            is_buffering.store(false, Ordering::Release);
                            break;
                        }
                        Ok(n) => {
                            let pos = pos.load(Ordering::Acquire);
                            if let Err(e) = cache.write_all(&buf[..n]) {
                                log::error!("{e}")
                            }

                            log::trace!("wrote {n} bytes to {identifier}");

                            let _ = cache.seek(std::io::SeekFrom::Start(pos as u64));
                            let _ = cache.flush();

                            buf_len.fetch_add(n, Ordering::AcqRel);
                        }
                        Err(e) if e.kind() == ErrorKind::Interrupted => {}
                        Err(e) => {
                            log::error!("{e}");
                            is_buffering.store(false, Ordering::Release);
                        }
                    }
                }
            });

            Ok(Self {
                identifier,
                cache,
                buf_len,
                pos,
                is_buffering,
                buffer_signal,
            })
        }
    }
}

impl Read for CachedHttpSource {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        // let n = self.cache.read(buf)?;
        // self.pos.fetch_add(n, Ordering::AcqRel);
        // log::trace!("read {n} bytes");
        // Ok(n)

        loop {
            let has_buf = self.buf_len.load(Ordering::Acquire) > self.pos.load(Ordering::Acquire);
            let is_buffering = self.is_buffering.load(Ordering::Acquire);

            if has_buf {
                let n = self.cache.read(buf)?;
                log::trace!("read {n} bytes from {}", self.identifier);
                if n == 0 {
                    continue;
                }
                self.pos.fetch_add(n, Ordering::AcqRel);
                break Ok(n);
            } else if !is_buffering {
                break Ok(0);
            }
        }
    }
}

impl Seek for CachedHttpSource {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        let p = self.cache.seek(pos)?;
        self.pos.store(p as usize, Ordering::Release);
        Ok(p)
    }
}

impl MediaSource for CachedHttpSource {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        let len = self.buf_len.load(Ordering::Acquire) as u64;
        log::trace!("returning buf_len {len}");
        Some(len)
    }
}

pub struct CachedAnnilSource(CachedHttpSource);

impl CachedAnnilSource {
    pub fn new(
        track: TrackIdentifier,
        cache_path: &Path,
        client: Client,
        provider: &TypedPriorityProvider<ProviderProxy>,
        buffer_signal: Arc<AtomicBool>,
    ) -> anyhow::Result<Self> {
        let mut source = provider
            .providers()
            .filter_map(|p| p.head(track).inspect_err(|e| log::warn!("{e}")).ok())
            .map(|r| r.url().clone());

        CachedHttpSource::new(track, || source.next(), cache_path, client, buffer_signal).map(Self)
    }
}

impl Read for CachedAnnilSource {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf)
    }
}

impl Seek for CachedAnnilSource {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        self.0.seek(pos)
    }
}

impl MediaSource for CachedAnnilSource {
    fn is_seekable(&self) -> bool {
        self.0.is_seekable()
    }

    fn byte_len(&self) -> Option<u64> {
        self.0.byte_len()
    }
}

fn create_parent(p: &Path) -> std::io::Result<()> {
    let mut ancestor = p.ancestors();
    let parent = ancestor.nth(1).unwrap();

    match create_dir_all(parent) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e),
    }
}
