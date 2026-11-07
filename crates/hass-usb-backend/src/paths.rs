use std::{
    collections::HashSet,
    path::{Component, Path, PathBuf},
};

use hass_usb::PathResolver;

/// A native non-strict realpath helper, matching `Path.resolve()` for existing
/// aliases while retaining an absolute normalized value for missing paths.
pub fn resolve_serial_path(path: impl AsRef<Path>) -> PathBuf {
    let path = path.as_ref();
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(path)
    };

    resolve_components(&absolute, &mut HashSet::new(), 0)
}

// `canonicalize` requires the final device to exist. Read each symlink itself
// instead, so a configured by-id alias still resolves after unplugging its
// target. Keep the unresolved tail, as Python's non-strict realpath does.
fn resolve_components(path: &Path, active: &mut HashSet<PathBuf>, depth: usize) -> PathBuf {
    let mut resolved = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            Component::Normal(name) => {
                resolved.push(name);
                if depth < 128
                    && !active.contains(&resolved)
                    && let Ok(target) = std::fs::read_link(&resolved)
                {
                    let link = resolved.clone();
                    active.insert(link.clone());
                    let target = if target.is_absolute() {
                        target
                    } else {
                        resolved
                            .parent()
                            .unwrap_or_else(|| Path::new("/"))
                            .join(target)
                    };
                    resolved = resolve_components(&target, active, depth + 1);
                    active.remove(&link);
                }
            }
            other => resolved.push(other.as_os_str()),
        }
    }
    resolved
}

/// Adapter used by `hass_usb::usb_device_from_path` for filesystem realpaths.
#[derive(Clone, Copy, Debug, Default)]
pub struct NativePathResolver;

impl PathResolver for NativePathResolver {
    fn realpath(&self, path: &str) -> String {
        resolve_serial_path(path).to_string_lossy().into_owned()
    }
}
