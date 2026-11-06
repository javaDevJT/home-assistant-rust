# Native image uploads

This crate stores actual JPEG, PNG and GIF uploads under a caller-selected
`image/` directory and persists the image collection in Runtime metadata.
Original bytes are preserved. Metadata contains `id`, `name`, `content_type`,
`filesize` and `uploaded_at`; IDs are random lowercase 32-character hexadecimal
values. Collection order follows upload order through edits and restarts.

`ImageIntegration::start(runtime, data_dir)` owns the store, and `manager()`
returns weak handles for create/list/get/update/delete, exact membership,
atomic snapshot/subscription and file access. `shutdown()` invalidates handles
without deleting uploaded data. The private `image_collection_updated` signal
contains `changes` with `change_type`, `image_id` and `item`.

Uploads are decoded before writes, with a 10 MiB file limit, 16,384-pixel
dimension limit and 256 MiB decoded-allocation limit. The manager rejects
traversal IDs and symlinked storage paths. Metadata/file failures attempt
cleanup or compensation; this is not a cross-filesystem database transaction.

Thumbnail filenames support `256x256` and `512x512` with optional cache-busting
suffixes. Cache files use canonical names. EXIF orientation is applied before
aspect-preserving resizing, with no cropping or upscaling. Output uses the
stored MIME format, takes the first GIF frame, and converts alpha to RGB for
JPEG. Catmull-Rom interpolation can differ slightly from Pillow's resampling.

The app supplies authenticated multipart `POST /api/image/upload`, public
`GET/HEAD /api/image/serve/{image_id}/{filename}`, and the `image/*` WebSocket
collection. Admin users can rename/delete; ordinary authenticated users can
upload and subscribe. File responses retain range/modified-date handling and
add source-style ETags and `Cache-Control: public, max-age=2678400`. Own-profile
picture updates require a matching relative URL and an existing image ID.

The implementation uses pinned Home Assistant Core commit
`cbf2c02c9bff331537c19bff69da8dc11c3fbfbf`, particularly
`components/image_upload/__init__.py`, `helpers/collection.py`, and
`tests/components/image_upload`. Native codec behavior uses `image 0.25.10`
with explicit EXIF orientation. The media-source platform, upstream storage
import, complete static-response/error parity and exhaustive upstream fixture
acceptance remain open.
