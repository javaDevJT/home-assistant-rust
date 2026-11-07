# Third-party source and behavioral provenance

The macOS IOKit traversal in `src/darwin.rs` is adapted from `serialx 1.15.1`,
archive member `src/darwin.rs`. The audit archive is
`/tmp/usb-engine-source-audit-2026-11-07/serialx-1.15.1.tar.gz`, SHA-256
`689dfbaf55f5f87ea0b6bcdcab135577ce1e70f98d490650b02a8b86eb0b0281`.
The adapted code removes the PyO3 module and maps native results into the
`hass-usb` provider record. The source is Apache-2.0; its license text is in
the repository-root `LICENSE.md`.

The Linux enumeration fields, alias behavior, and `udev_serial_by_id_stem`
algorithm were ported from the audited `serialx 1.15.1` source. The inotify
event set and recursive-watch lifecycle follow the audited behavior of
`aiousbwatcher 1.1.2`; no Python source or runtime bridge is included. Its
archive is
`/tmp/usb-engine-source-audit-2026-11-07/aiousbwatcher-1.1.2.tar.gz`, SHA-256
`908919fed3b5d6b5b0a26f7aea82a5f45c6a6205789dcb39348abc34a4f06ebe`.
That package is MIT-licensed; this crate implements the documented behavior
without copying its source code.
