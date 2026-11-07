use std::{
    collections::HashMap,
    ffi::CString,
    fs::{self, File},
    io::{self, Write},
    os::fd::{AsRawFd, FromRawFd},
    os::unix::ffi::OsStrExt,
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
};

use crate::{WatcherError, WatcherStartError};

/// Filesystem tree watched by the pinned aiousbwatcher implementation.
pub const USB_DEVICE_TREE: &str = "/dev/bus/usb";

use crate::watcher_events::watcher_event_requires_notification;

const WATCH_MASK: u32 = libc::IN_CREATE
    | libc::IN_MOVED_FROM
    | libc::IN_MOVED_TO
    | libc::IN_DELETE_SELF
    | libc::IN_DELETE
    | libc::IN_IGNORED;

type ChangeCallback = Arc<dyn Fn() + Send + Sync + 'static>;

/// Strong owner of the Linux inotify worker, descriptors, and cancellation.
pub struct NativeSerialPortWatcher {
    root: PathBuf,
    state: Mutex<Option<RunningWorker>>,
}

struct RunningWorker {
    cancel: Option<File>,
    worker: Option<JoinHandle<Result<(), WatcherError>>>,
    stop: Arc<AtomicBool>,
}

impl NativeSerialPortWatcher {
    /// Construct a not-yet-started watcher for `/dev/bus/usb`.
    pub fn new() -> Self {
        Self::with_root(PathBuf::from(USB_DEVICE_TREE))
    }

    /// Construct a watcher for a caller-owned tree, primarily for deterministic
    /// tests and nonstandard mounted sysfs/devfs layouts.
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            state: Mutex::new(None),
        }
    }

    /// Start watching the configured tree. The callback runs on the owned
    /// worker thread and should only enqueue/schedule SDK work.
    pub fn start_watching(
        &self,
        callback: crate::UsbPortChangeCallback,
    ) -> Result<(), WatcherStartError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.is_some() {
            return Err(WatcherStartError::AlreadyStarted);
        }
        *state = Some(start_worker(&self.root, callback)?);
        Ok(())
    }

    /// Signal cancellation and join the worker. Repeated calls are harmless.
    pub fn stop_and_join(&self) -> Result<(), WatcherError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Serialize start and every stop through the completed join, rather
        // than publishing an empty slot while the old worker still runs.
        let result = state.take().map_or(Ok(()), RunningWorker::stop_and_join);
        drop(state);
        result
    }
}

impl Default for NativeSerialPortWatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for NativeSerialPortWatcher {
    fn drop(&mut self) {
        let _ = self.stop_and_join();
    }
}

impl RunningWorker {
    fn stop_and_join(mut self) -> Result<(), WatcherError> {
        self.stop.store(true, Ordering::Release);
        if let Some(mut cancel) = self.cancel.take() {
            let _ = cancel.write_all(&1_u64.to_ne_bytes());
        }
        let Some(worker) = self.worker.take() else {
            return Ok(());
        };
        worker.join().unwrap_or(Err(WatcherError::WorkerPanicked))
    }
}

fn start_worker(root: &Path, callback: ChangeCallback) -> Result<RunningWorker, WatcherStartError> {
    let inotify_fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC | libc::IN_NONBLOCK) };
    if inotify_fd < 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ENOSYS) {
            return Err(WatcherStartError::InotifyNotAvailable);
        }
        return Err(WatcherStartError::Io(error));
    }
    let inotify = unsafe { File::from_raw_fd(inotify_fd) };

    let mut watched = HashMap::new();
    add_directories_recursive(inotify.as_raw_fd(), root, &mut watched)
        .map_err(WatcherStartError::Io)?;

    let cancel_fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
    if cancel_fd < 0 {
        return Err(WatcherStartError::Io(io::Error::last_os_error()));
    }
    let cancel_reader = unsafe { File::from_raw_fd(cancel_fd) };
    let cancel_writer = cancel_reader.try_clone().map_err(WatcherStartError::Io)?;

    let worker_root = root.to_path_buf();
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = Arc::clone(&stop);
    let worker = thread::Builder::new()
        .name("hass-usb-inotify".to_owned())
        .spawn(move || {
            run_worker(
                inotify,
                cancel_reader,
                worker_root,
                watched,
                callback,
                worker_stop,
            )
        })
        .map_err(WatcherStartError::Io)?;

    Ok(RunningWorker {
        cancel: Some(cancel_writer),
        worker: Some(worker),
        stop,
    })
}

fn run_worker(
    inotify: File,
    cancel: File,
    root: PathBuf,
    mut watched: HashMap<i32, PathBuf>,
    callback: ChangeCallback,
    stop: Arc<AtomicBool>,
) -> Result<(), WatcherError> {
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        if stop.load(Ordering::Acquire) {
            return Ok(());
        }
        let mut poll_fds = [
            libc::pollfd {
                fd: inotify.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: cancel.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let ready = unsafe { libc::poll(poll_fds.as_mut_ptr(), poll_fds.len() as _, 250) };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(WatcherError::Io(error));
        }
        if ready == 0 {
            continue;
        }
        if poll_fds[1].revents != 0 {
            return Ok(());
        }
        if poll_fds[0].revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            return Err(WatcherError::Io(io::Error::other(
                "inotify descriptor reported a poll error",
            )));
        }
        if poll_fds[0].revents & libc::POLLIN == 0 {
            continue;
        }

        let count = unsafe {
            libc::read(
                inotify.as_raw_fd(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
            )
        };
        if count < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::WouldBlock
                || error.kind() == io::ErrorKind::Interrupted
            {
                continue;
            }
            return Err(WatcherError::Io(error));
        }
        let count = count as usize;
        let mut offset = 0;
        while offset + std::mem::size_of::<libc::inotify_event>() <= count {
            let event = unsafe {
                buffer
                    .as_ptr()
                    .add(offset)
                    .cast::<libc::inotify_event>()
                    .read_unaligned()
            };
            let name_start = offset + std::mem::size_of::<libc::inotify_event>();
            let name_end = name_start.saturating_add(event.len as usize).min(count);
            let name = buffer[name_start..name_end]
                .split(|byte| *byte == 0)
                .next()
                .filter(|bytes| !bytes.is_empty());
            let base_path = watched
                .get(&event.wd)
                .cloned()
                .unwrap_or_else(|| root.clone());
            let event_path = name.map(|name| base_path.join(std::ffi::OsStr::from_bytes(name)));

            // aiousbwatcher adds watches below a newly created directory before
            // invoking the changed callback for that same event.
            if event.mask & libc::IN_CREATE != 0
                && let Some(path) = event_path.as_deref()
            {
                add_directories_recursive(inotify.as_raw_fd(), path, &mut watched)
                    .map_err(WatcherError::Io)?;
            }
            dispatch_watch_event(event.mask, || {
                let _ = catch_unwind(AssertUnwindSafe(|| callback()));
            });
            if event.mask & libc::IN_IGNORED != 0 {
                watched.remove(&event.wd);
            }
            offset = name_end;
        }
    }
}

fn dispatch_watch_event(mask: u32, callback: impl FnOnce()) {
    if watcher_event_requires_notification(mask) {
        callback();
    }
}

fn add_directories_recursive(
    inotify_fd: i32,
    root: &Path,
    watched: &mut HashMap<i32, PathBuf>,
) -> io::Result<()> {
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        if !directory.is_dir() {
            continue;
        }
        let path = CString::new(directory.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
        let wd = unsafe { libc::inotify_add_watch(inotify_fd, path.as_ptr(), WATCH_MASK) };
        if wd < 0 {
            return Err(io::Error::last_os_error());
        }
        watched.insert(wd, directory.clone());

        // Path.walk in the source does not follow directory symlinks and
        // tolerates directories that disappear while being traversed.
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                pending.push(entry.path());
            }
        }
    }
    Ok(())
}
