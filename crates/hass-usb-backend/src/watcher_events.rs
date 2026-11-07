/// Linux inotify bits watched by aiousbwatcher 1.1.2:
/// CREATE, MOVED_FROM, MOVED_TO, DELETE_SELF, DELETE, and IGNORED.
pub const SOURCE_WATCH_EVENT_MASK: u32 =
    0x0000_0100 | 0x0000_0040 | 0x0000_0080 | 0x0000_0400 | 0x0000_0200 | 0x0000_8000;

pub const fn watcher_event_requires_notification(mask: u32) -> bool {
    mask & SOURCE_WATCH_EVENT_MASK != 0
}
