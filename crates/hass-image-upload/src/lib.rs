//! Native storage and image processing for Home Assistant profile images.
//!
//! The crate owns the image collection and its image files. HTTP, WebSocket,
//! authentication, and profile URL handling belong to the application that
//! embeds it.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use chrono::{SecondsFormat, Utc};
use hass_core::{Context, Error, Event, Result, Runtime};
use image::imageops::FilterType;
use image::{DynamicImage, GenericImageView, ImageDecoder, ImageFormat, ImageReader, Limits};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::sync::broadcast;
use uuid::Uuid;

/// Home Assistant image upload integration domain.
pub const DOMAIN: &str = "image_upload";
/// Home Assistant uses this storage key for image metadata.
pub const STORAGE_KEY: &str = "image";
/// The maximum accepted multipart image body size.
pub const MAX_SIZE: usize = 10 * 1024 * 1024;
/// Home Assistant's list of thumbnail sizes.
pub const VALID_SIZES: [u32; 2] = [256, 512];
/// Image collection update signal consumed by the native API adapters.
pub const IMAGE_COLLECTION_UPDATED_SIGNAL: &str = "image_collection_updated";
/// Cache-Control value used by Home Assistant's image static responses.
pub const CACHE_CONTROL_HEADER_VALUE: &str = "public, max-age=2678400";
/// Cache-Control header name used by Home Assistant's image static responses.
pub const CACHE_CONTROL_HEADER_NAME: &str = "Cache-Control";

const IMAGE_DIRECTORY: &str = "image";
const MAX_IMAGE_DIMENSION: u32 = 16_384;
const MAX_DECODED_BYTES: u64 = 256 * 1024 * 1024;

/// One persisted image collection entry, matching Home Assistant's image DTO.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ImageMetadata {
    /// Unguessable 32-character lowercase hexadecimal ID.
    pub id: String,
    /// Upload filename; updateable through the collection API.
    pub name: String,
    /// Stored upload MIME type.
    pub content_type: String,
    /// Byte length of the exact uploaded original.
    pub filesize: u64,
    /// UTC ISO-8601 upload timestamp.
    pub uploaded_at: String,
}

/// Partial update supported by Home Assistant's image collection.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImageUpdate {
    /// New display name. When omitted, the existing name is kept.
    pub name: Option<String>,
}

/// A valid square thumbnail target accepted by the serve endpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThumbnailSize {
    /// 256 by 256 maximum bounds.
    Size256,
    /// 512 by 512 maximum bounds.
    Size512,
}

impl ThumbnailSize {
    /// Return the square bound in pixels.
    pub const fn pixels(self) -> u32 {
        match self {
            Self::Size256 => 256,
            Self::Size512 => 512,
        }
    }

    fn cache_filename(self) -> String {
        format!("{}x{}", self.pixels(), self.pixels())
    }
}

/// Atomic snapshot of the editable image collection.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImageCollectionSnapshot {
    /// Stored images ordered by their stable ID.
    pub storage: Vec<ImageMetadata>,
}

/// Validated managed file for an HTTP file service. Paths remain internal to
/// the application and are never part of the public image metadata.
pub struct PreparedImage {
    pub path: PathBuf,
    pub metadata: ImageMetadata,
    pub etag: String,
}

/// Owning lifecycle for the native image upload integration.
pub struct ImageIntegration {
    manager: ImageManager,
    shared: Arc<Shared>,
}

/// Weak handle for synchronous collection and image-file operations.
#[derive(Clone)]
pub struct ImageManager {
    shared: Weak<Shared>,
}

struct Shared {
    runtime: Runtime,
    image_dir: PathBuf,
    data: Mutex<ImageData>,
    active: AtomicBool,
}

#[derive(Clone, Default)]
struct ImageData {
    items: Vec<ImageMetadata>,
}

impl ImageIntegration {
    /// Load image metadata from `Runtime` and own files below `data_dir/image`.
    ///
    /// `data_dir` must be an explicit directory supplied by the caller. The
    /// integration does not choose a process-global or current-directory path.
    pub fn start(runtime: Runtime, data_dir: impl AsRef<Path>) -> Result<Self> {
        let image_dir = prepare_image_dir(data_dir.as_ref())?;
        let data = ImageData {
            items: load_items(&runtime)?,
        };
        let shared = Arc::new(Shared {
            runtime,
            image_dir,
            data: Mutex::new(data),
            active: AtomicBool::new(true),
        });
        let manager = ImageManager {
            shared: Arc::downgrade(&shared),
        };
        Ok(Self { manager, shared })
    }

    /// Return a weak manager handle for typed collection operations.
    pub fn manager(&self) -> ImageManager {
        self.manager.clone()
    }

    /// Stop accepting operations and release the Runtime and file handles.
    pub fn shutdown(self) -> Result<()> {
        self.shared.active.store(false, Ordering::Release);
        Ok(())
    }
}

impl Drop for ImageIntegration {
    fn drop(&mut self) {
        self.shared.active.store(false, Ordering::Release);
    }
}

impl ImageManager {
    /// List stored images ordered by stable ID.
    pub fn list(&self) -> Result<Vec<ImageMetadata>> {
        let shared = self.live()?;
        let data = lock(&shared.data);
        Self::ensure_active(&shared)?;
        Ok(data.items.clone())
    }

    /// Return one stored image, if the exact ID exists.
    pub fn get(&self, id: &str) -> Result<Option<ImageMetadata>> {
        let shared = self.live()?;
        let data = lock(&shared.data);
        Self::ensure_active(&shared)?;
        Ok(data.items.iter().find(|item| item.id == id).cloned())
    }

    /// Check exact collection membership. URL validators should use this
    /// instead of accepting IDs by prefix alone.
    pub fn contains(&self, id: &str) -> Result<bool> {
        let shared = self.live()?;
        let data = lock(&shared.data);
        Self::ensure_active(&shared)?;
        Ok(data.items.iter().any(|item| item.id == id))
    }

    /// Atomically snapshot the collection and subscribe to later Runtime
    /// signals. Consumers should retain events whose type is
    /// [`IMAGE_COLLECTION_UPDATED_SIGNAL`].
    pub fn snapshot_and_subscribe(
        &self,
    ) -> Result<(ImageCollectionSnapshot, broadcast::Receiver<Event>)> {
        let shared = self.live()?;
        let data = lock(&shared.data);
        Self::ensure_active(&shared)?;
        let receiver = shared.runtime.subscribe_signals();
        Ok((
            ImageCollectionSnapshot {
                storage: data.items.clone(),
            },
            receiver,
        ))
    }

    /// Validate and store an uploaded image while preserving its original
    /// bytes exactly. Only the three MIME types accepted by Home Assistant are
    /// accepted. The bytes are decoded before anything is written to disk.
    pub fn create(
        &self,
        name: impl Into<String>,
        content_type: impl AsRef<str>,
        bytes: &[u8],
    ) -> Result<ImageMetadata> {
        let content_type = content_type.as_ref();
        validate_content_type(content_type)?;
        if bytes.len() > MAX_SIZE {
            return Err(Error::InvalidInput(format!(
                "image exceeds the {MAX_SIZE}-byte upload limit"
            )));
        }
        let _decoded_image = decode_image(bytes)?;

        let shared = self.live()?;
        let mut data = lock(&shared.data);
        Self::ensure_active(&shared)?;
        ensure_storage_root(&shared.image_dir)?;

        let (id, image_path) = create_unique_image_dir(&shared.image_dir, &data.items)?;
        let original_path = image_path.join("original");
        if let Err(error) = write_atomic(&original_path, bytes) {
            let _ = remove_image_dir(&image_path);
            return Err(error);
        }

        let metadata = ImageMetadata {
            id: id.clone(),
            name: name.into(),
            content_type: content_type.to_owned(),
            filesize: bytes.len() as u64,
            uploaded_at: Utc::now().to_rfc3339_opts(SecondsFormat::AutoSi, false),
        };
        let old_items = data.items.clone();
        let mut next_items = old_items.clone();
        next_items.push(metadata.clone());
        if let Err(error) = persist_items(&shared.runtime, &next_items) {
            let _ = remove_image_dir(&image_path);
            return Err(error);
        }
        data.items = next_items;
        if let Err(error) = dispatch_change(&shared.runtime, "added", &metadata) {
            data.items = old_items.clone();
            let rollback = persist_items(&shared.runtime, &old_items);
            let _ = remove_image_dir(&image_path);
            if let Err(rollback_error) = rollback {
                return Err(Error::Storage(format!(
                    "image collection signal failed ({error}); metadata rollback failed ({rollback_error})"
                )));
            }
            return Err(error);
        }
        Ok(metadata)
    }

    /// Update only the display name. An empty supplied name is rejected, as in
    /// Home Assistant's `UPDATE_FIELDS` schema. An empty update still emits an
    /// `updated` collection change.
    pub fn update(&self, id: &str, update: ImageUpdate) -> Result<ImageMetadata> {
        if update.name.as_deref().is_some_and(str::is_empty) {
            return Err(Error::InvalidInput(
                "image name must not be empty".to_owned(),
            ));
        }
        let shared = self.live()?;
        let mut data = lock(&shared.data);
        Self::ensure_active(&shared)?;
        let index = data
            .items
            .iter()
            .position(|item| item.id == id)
            .ok_or_else(|| Error::NotFound(format!("image {id}")))?;
        let mut metadata = data.items[index].clone();
        if let Some(name) = update.name {
            metadata.name = name;
        }

        let old_items = data.items.clone();
        let mut next_items = old_items.clone();
        next_items[index] = metadata.clone();
        persist_items(&shared.runtime, &next_items)?;
        data.items = next_items;
        if let Err(error) = dispatch_change(&shared.runtime, "updated", &metadata) {
            data.items = old_items.clone();
            if let Err(rollback_error) = persist_items(&shared.runtime, &old_items) {
                return Err(Error::Storage(format!(
                    "image collection signal failed ({error}); metadata rollback failed ({rollback_error})"
                )));
            }
            return Err(error);
        }
        Ok(metadata)
    }

    /// Remove an image from metadata and delete its owned file directory.
    pub fn delete(&self, id: &str) -> Result<()> {
        let shared = self.live()?;
        let mut data = lock(&shared.data);
        Self::ensure_active(&shared)?;
        let index = data
            .items
            .iter()
            .position(|item| item.id == id)
            .ok_or_else(|| Error::NotFound(format!("image {id}")))?;
        let metadata = data.items[index].clone();
        ensure_storage_root(&shared.image_dir)?;
        let image_path = image_path_for(&shared.image_dir, id)?;
        validate_directory_if_present(&image_path)?;

        let old_items = data.items.clone();
        let mut next_items = old_items.clone();
        next_items.remove(index);
        persist_items(&shared.runtime, &next_items)?;
        data.items = next_items;
        if let Err(error) = dispatch_change(&shared.runtime, "removed", &metadata) {
            data.items = old_items.clone();
            if let Err(rollback_error) = persist_items(&shared.runtime, &old_items) {
                return Err(Error::Storage(format!(
                    "image collection signal failed ({error}); metadata rollback failed ({rollback_error})"
                )));
            }
            return Err(error);
        }
        remove_image_dir(&image_path)
    }

    /// Read exact uploaded bytes from the `original` file.
    pub fn read_original(&self, id: &str) -> Result<Vec<u8>> {
        let shared = self.live()?;
        let data = lock(&shared.data);
        Self::ensure_active(&shared)?;
        if !data.items.iter().any(|item| item.id == id) {
            return Err(Error::NotFound(format!("image {id}")));
        }
        ensure_storage_root(&shared.image_dir)?;
        let image_path = image_path_for(&shared.image_dir, id)?;
        validate_directory_if_present(&image_path)?;
        read_regular_file(&image_path.join("original"), "image original")
    }

    /// Read a cached thumbnail, creating it on the first request. The filename
    /// parser mirrors Home Assistant's accepted `256x256[-suffix]` and
    /// `512x512[-suffix]` forms; the suffix never enters a filesystem path.
    pub fn read_thumbnail(&self, id: &str, filename: &str) -> Result<Vec<u8>> {
        let shared = self.live()?;
        let data = lock(&shared.data);
        Self::ensure_active(&shared)?;
        let metadata = data
            .items
            .iter()
            .find(|item| item.id == id)
            .cloned()
            .ok_or_else(|| Error::NotFound(format!("image {id}")))?;
        let size = parse_thumbnail_filename(filename)?;
        ensure_storage_root(&shared.image_dir)?;
        let image_path = image_path_for(&shared.image_dir, id)?;
        validate_directory_if_present(&image_path)?;
        let cache_path = image_path.join(size.cache_filename());

        match fs::symlink_metadata(&cache_path) {
            Ok(file_metadata) if file_metadata.file_type().is_symlink() => {
                return Err(Error::Storage(
                    "thumbnail cache path must not be a symlink".to_owned(),
                ));
            }
            Ok(file_metadata) if file_metadata.is_file() => {
                return read_regular_file(&cache_path, "image thumbnail");
            }
            Ok(_) => {
                return Err(Error::Storage(
                    "thumbnail cache path is not a regular file".to_owned(),
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(storage_error("inspect thumbnail cache", error)),
        }

        let original = read_regular_file(&image_path.join("original"), "image original")?;
        let thumbnail = render_thumbnail(&original, &metadata.content_type, size)?;
        write_atomic(&cache_path, &thumbnail)?;
        read_regular_file(&cache_path, "image thumbnail")
    }

    /// Prepare a managed original or canonical cached thumbnail for streaming.
    /// The same metadata, directory and symlink checks as byte reads apply.
    pub fn prepare_file(&self, id: &str, filename: &str) -> Result<PreparedImage> {
        let filename = if filename == "original" {
            "original".to_owned()
        } else {
            self.read_thumbnail(id, filename)?;
            parse_thumbnail_filename(filename)?.cache_filename()
        };
        let shared = self.live()?;
        let data = lock(&shared.data);
        Self::ensure_active(&shared)?;
        let metadata = data
            .items
            .iter()
            .find(|item| item.id == id)
            .cloned()
            .ok_or_else(|| Error::NotFound(format!("image {id}")))?;
        ensure_storage_root(&shared.image_dir)?;
        let directory = image_path_for(&shared.image_dir, id)?;
        validate_directory_if_present(&directory)?;
        let path = directory.join(filename);
        let file = validate_regular_file(&path, "image file")?;
        let modified = file
            .modified()
            .map_err(|error| storage_error("inspect image modification time", error))?;
        let nanos = modified
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let etag = format!("\"{:x}-{:x}\"", nanos, file.len());
        Ok(PreparedImage {
            path,
            metadata,
            etag,
        })
    }

    fn live(&self) -> Result<Arc<Shared>> {
        self.shared
            .upgrade()
            .ok_or_else(|| Error::Service("image integration is shut down".to_owned()))
    }

    fn ensure_active(shared: &Shared) -> Result<()> {
        if shared.active.load(Ordering::Acquire) {
            Ok(())
        } else {
            Err(Error::Service("image integration is shut down".to_owned()))
        }
    }
}

/// Parse the filename accepted by Home Assistant's image serve endpoint.
pub fn parse_thumbnail_filename(filename: &str) -> Result<ThumbnailSize> {
    let size = filename.split_once('-').map_or(filename, |(size, _)| size);
    let (width, height) = size
        .split_once('x')
        .ok_or_else(|| Error::InvalidInput("invalid image thumbnail filename".to_owned()))?;
    let width = width
        .parse::<u32>()
        .map_err(|_| Error::InvalidInput("invalid image thumbnail filename".to_owned()))?;
    let height = height
        .parse::<u32>()
        .map_err(|_| Error::InvalidInput("invalid image thumbnail filename".to_owned()))?;
    if width != height {
        return Err(Error::InvalidInput(
            "image thumbnails must be square".to_owned(),
        ));
    }
    match width {
        256 if VALID_SIZES.contains(&height) => Ok(ThumbnailSize::Size256),
        512 if VALID_SIZES.contains(&height) => Ok(ThumbnailSize::Size512),
        _ => Err(Error::InvalidInput(
            "unsupported image thumbnail size".to_owned(),
        )),
    }
}

fn validate_content_type(content_type: &str) -> Result<()> {
    match content_type {
        "image/gif" | "image/jpeg" | "image/png" => Ok(()),
        _ => Err(Error::InvalidInput(
            "only jpeg, png, and gif images are allowed".to_owned(),
        )),
    }
}

fn decode_image(bytes: &[u8]) -> Result<DynamicImage> {
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|error| Error::InvalidInput(format!("unable to identify image file: {error}")))?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_DIMENSION);
    limits.max_image_height = Some(MAX_IMAGE_DIMENSION);
    limits.max_alloc = Some(MAX_DECODED_BYTES);
    reader.limits(limits);

    let mut decoder = reader
        .into_decoder()
        .map_err(|error| Error::InvalidInput(format!("unable to identify image file: {error}")))?;
    let (width, height) = decoder.dimensions();
    if width == 0
        || height == 0
        || width > MAX_IMAGE_DIMENSION
        || height > MAX_IMAGE_DIMENSION
        || decoder.total_bytes() > MAX_DECODED_BYTES
    {
        return Err(Error::InvalidInput(
            "image dimensions exceed the decode limit".to_owned(),
        ));
    }
    let orientation = decoder.orientation().map_err(|error| {
        Error::InvalidInput(format!("unable to read image orientation: {error}"))
    })?;
    let mut image = DynamicImage::from_decoder(decoder)
        .map_err(|error| Error::InvalidInput(format!("unable to decode image: {error}")))?;
    image.apply_orientation(orientation);
    Ok(image)
}

fn render_thumbnail(bytes: &[u8], content_type: &str, size: ThumbnailSize) -> Result<Vec<u8>> {
    let image = decode_image(bytes)
        .map_err(|error| Error::Storage(format!("unable to decode stored image: {error}")))?;
    let mut thumbnail = resize_contain(image, size.pixels());
    let format = match content_type {
        "image/gif" => ImageFormat::Gif,
        "image/jpeg" => {
            if !matches!(
                thumbnail.color(),
                image::ColorType::L8 | image::ColorType::Rgb8
            ) {
                thumbnail = DynamicImage::ImageRgb8(thumbnail.into_rgb8());
            }
            ImageFormat::Jpeg
        }
        "image/png" => ImageFormat::Png,
        _ => {
            return Err(Error::Storage(
                "stored image has an unsupported content type".to_owned(),
            ));
        }
    };
    let mut output = Cursor::new(Vec::new());
    thumbnail
        .write_to(&mut output, format)
        .map_err(|error| Error::Storage(format!("unable to encode image thumbnail: {error}")))?;
    Ok(output.into_inner())
}

fn resize_contain(image: DynamicImage, max_size: u32) -> DynamicImage {
    let (width, height) = image.dimensions();
    let scale = (max_size as f64 / width as f64)
        .min(max_size as f64 / height as f64)
        .min(1.0);
    if scale >= 1.0 {
        return image;
    }
    let target_width = ((width as f64 * scale).round() as u32).clamp(1, max_size);
    let target_height = ((height as f64 * scale).round() as u32).clamp(1, max_size);
    image.resize_exact(target_width, target_height, FilterType::CatmullRom)
}

fn prepare_image_dir(data_dir: &Path) -> Result<PathBuf> {
    if data_dir.as_os_str().is_empty() {
        return Err(Error::InvalidInput(
            "image upload data directory must not be empty".to_owned(),
        ));
    }
    match fs::symlink_metadata(data_dir) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(Error::InvalidInput(
                "image upload data directory must not be a symlink".to_owned(),
            ));
        }
        Ok(metadata) if !metadata.is_dir() => {
            return Err(Error::InvalidInput(
                "image upload data directory is not a directory".to_owned(),
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(data_dir)
                .map_err(|error| storage_error("create image upload data directory", error))?;
        }
        Err(error) => return Err(storage_error("inspect image upload data directory", error)),
    }
    let metadata = fs::symlink_metadata(data_dir)
        .map_err(|error| storage_error("inspect image upload data directory", error))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(Error::InvalidInput(
            "image upload data directory must be a real directory".to_owned(),
        ));
    }
    let root = fs::canonicalize(data_dir)
        .map_err(|error| storage_error("resolve image upload data directory", error))?;
    let image_dir = root.join(IMAGE_DIRECTORY);
    ensure_directory(&image_dir, "create image upload directory")?;
    Ok(image_dir)
}

fn ensure_directory(path: &Path, action: &str) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(Error::Storage(
            "image storage path must not be a symlink".to_owned(),
        )),
        Ok(metadata) if !metadata.is_dir() => Err(Error::Storage(
            "image storage path is not a directory".to_owned(),
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            create_private_directory(path).map_err(|error| storage_error(action, error))?;
            let metadata = fs::symlink_metadata(path)
                .map_err(|error| storage_error("inspect image storage directory", error))?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(Error::Storage(
                    "image storage path changed while it was created".to_owned(),
                ));
            }
            Ok(())
        }
        Err(error) => Err(storage_error("inspect image storage directory", error)),
    }
}

fn create_private_directory(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700).create(path)
    }
    #[cfg(not(unix))]
    {
        fs::create_dir(path)
    }
}

fn create_unique_image_dir(
    root: &Path,
    existing_items: &[ImageMetadata],
) -> Result<(String, PathBuf)> {
    ensure_storage_root(root)?;
    for _ in 0..8 {
        let id = Uuid::new_v4().simple().to_string();
        if existing_items.iter().any(|item| item.id == id) {
            continue;
        }
        let path = image_path_for(root, &id)?;
        match create_private_directory(&path) {
            Ok(()) => {
                if let Err(error) = sync_directory(root) {
                    let _ = remove_image_dir(&path);
                    return Err(storage_error("sync image upload directory", error));
                }
                return Ok((id, path));
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(storage_error("create image directory", error)),
        }
    }
    Err(Error::Storage(
        "could not allocate a unique image ID".to_owned(),
    ))
}

fn image_path_for(root: &Path, id: &str) -> Result<PathBuf> {
    if !is_valid_id(id) {
        return Err(Error::InvalidInput("invalid image ID".to_owned()));
    }
    Ok(root.join(id))
}

fn is_valid_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn validate_directory_if_present(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(Error::Storage(
            "image directory must not be a symlink".to_owned(),
        )),
        Ok(metadata) if !metadata.is_dir() => {
            Err(Error::Storage("image path is not a directory".to_owned()))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(storage_error("inspect image directory", error)),
    }
}

fn ensure_storage_root(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(Error::Storage(
            "image storage directory must not be a symlink".to_owned(),
        )),
        Ok(metadata) if !metadata.is_dir() => Err(Error::Storage(
            "image storage path is not a directory".to_owned(),
        )),
        Ok(_) => Ok(()),
        Err(error) => Err(storage_error("inspect image storage directory", error)),
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        validate_directory_if_present(parent)?;
    }
    match fs::symlink_metadata(path) {
        Ok(_) => return Err(Error::Storage("image file already exists".to_owned())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(storage_error("inspect image file", error)),
    }

    let parent = path
        .parent()
        .ok_or_else(|| Error::Storage("image file has no parent directory".to_owned()))?;
    let temp_path = parent.join(format!(".pending-{}.tmp", Uuid::new_v4().simple()));
    let mut created_temp = false;
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temp_path)
            .map_err(|error| storage_error("create temporary image file", error))?;
        created_temp = true;
        file.write_all(bytes)
            .map_err(|error| storage_error("write image file", error))?;
        file.sync_all()
            .map_err(|error| storage_error("sync image file", error))?;
        drop(file);
        fs::rename(&temp_path, path).map_err(|error| storage_error("commit image file", error))?;
        sync_directory(parent).map_err(|error| storage_error("sync image directory", error))?;
        Ok(())
    })();
    if result.is_err() && created_temp {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

fn read_regular_file(path: &Path, label: &str) -> Result<Vec<u8>> {
    validate_regular_file(path, label)?;
    fs::read(path).map_err(|error| storage_error("read image file", error))
}

fn validate_regular_file(path: &Path, label: &str) -> Result<fs::Metadata> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            Error::NotFound(label.to_owned())
        } else {
            storage_error("inspect image file", error)
        }
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(Error::Storage(format!("{label} is not a regular file")));
    }
    Ok(metadata)
}

fn remove_image_dir(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(storage_error("inspect image directory for removal", error)),
        Ok(metadata) if metadata.file_type().is_symlink() => Err(Error::Storage(
            "refusing to remove a symlinked image directory".to_owned(),
        )),
        Ok(metadata) if !metadata.is_dir() => Err(Error::Storage(
            "refusing to remove a non-directory image path".to_owned(),
        )),
        Ok(_) => {
            fs::remove_dir_all(path)
                .map_err(|error| storage_error("remove image directory", error))?;
            if let Some(parent) = path.parent() {
                sync_directory(parent)
                    .map_err(|error| storage_error("sync image directory removal", error))?;
            }
            Ok(())
        }
    }
}

fn load_items(runtime: &Runtime) -> Result<Vec<ImageMetadata>> {
    let Some(value) = runtime.get_data(STORAGE_KEY)? else {
        return Ok(Vec::new());
    };
    let items = serde_json::from_value::<Vec<ImageMetadata>>(value)
        .map_err(|error| Error::Storage(format!("invalid image metadata: {error}")))?;
    let mut seen_ids = HashSet::with_capacity(items.len());
    for item in &items {
        if !is_valid_id(&item.id) || !seen_ids.insert(&item.id) {
            return Err(Error::Storage("invalid image metadata ID".to_owned()));
        }
        validate_content_type(&item.content_type)
            .map_err(|error| Error::Storage(format!("invalid image metadata: {error}")))?;
        if item.filesize == 0 || item.filesize > MAX_SIZE as u64 {
            return Err(Error::Storage("invalid image metadata size".to_owned()));
        }
    }
    Ok(items)
}

fn persist_items(runtime: &Runtime, items: &[ImageMetadata]) -> Result<()> {
    let value = serde_json::to_value(items)
        .map_err(|error| Error::Storage(format!("serialize image metadata: {error}")))?;
    runtime.set_data(STORAGE_KEY, &value)
}

fn dispatch_change(runtime: &Runtime, change_type: &str, item: &ImageMetadata) -> Result<()> {
    let item = serde_json::to_value(item)
        .map_err(|error| Error::Storage(format!("serialize image collection item: {error}")))?;
    let change = json!({
        "change_type": change_type,
        "image_id": item["id"],
        "item": item,
    });
    let mut data = Map::new();
    data.insert("changes".to_owned(), Value::Array(vec![change]));
    runtime.dispatch_signal(IMAGE_COLLECTION_UPDATED_SIGNAL, data, Context::default())?;
    Ok(())
}

fn sync_directory(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        File::open(path)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

fn storage_error(action: &str, error: std::io::Error) -> Error {
    Error::Storage(format!("{action}: {error}"))
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;

    use hass_core::{
        EventHistoryQuery, HistoryQuery, HistorySeries, MemoryStore, PurgeResult, State, Store,
    };
    use hass_storage::SqliteStore;
    use image::{ImageBuffer, Luma, Rgb, Rgba};

    use super::*;

    fn runtime() -> Runtime {
        Runtime::new(Arc::new(MemoryStore::default())).unwrap()
    }

    fn encode_image(image: DynamicImage, format: ImageFormat) -> Vec<u8> {
        let mut output = Cursor::new(Vec::new());
        image.write_to(&mut output, format).unwrap();
        output.into_inner()
    }

    fn png_fixture(width: u32, height: u32) -> Vec<u8> {
        let image = ImageBuffer::from_fn(width, height, |x, y| {
            Rgba([
                (x as u8).wrapping_mul(29),
                (y as u8).wrapping_mul(47),
                ((x + y) as u8).wrapping_mul(13),
                if (x + y) % 2 == 0 { 96 } else { 255 },
            ])
        });
        encode_image(DynamicImage::ImageRgba8(image), ImageFormat::Png)
    }

    fn jpeg_fixture(width: u32, height: u32) -> Vec<u8> {
        let image = ImageBuffer::from_fn(width, height, |x, y| {
            Luma([((x * 31 + y * 67) % 240) as u8])
        });
        encode_image(DynamicImage::ImageLuma8(image), ImageFormat::Jpeg)
    }

    fn gif_fixture(width: u32, height: u32) -> Vec<u8> {
        let image = ImageBuffer::from_fn(width, height, |x, y| {
            Rgb([(x as u8).wrapping_mul(43), (y as u8).wrapping_mul(59), 111])
        });
        encode_image(DynamicImage::ImageRgb8(image), ImageFormat::Gif)
    }

    fn jpeg_with_exif_orientation(bytes: &[u8], orientation: u16) -> Vec<u8> {
        assert_eq!(&bytes[..2], &[0xff, 0xd8]);
        let mut tiff = Vec::new();
        tiff.extend_from_slice(b"II");
        tiff.extend_from_slice(&42_u16.to_le_bytes());
        tiff.extend_from_slice(&8_u32.to_le_bytes());
        tiff.extend_from_slice(&1_u16.to_le_bytes());
        tiff.extend_from_slice(&0x0112_u16.to_le_bytes());
        tiff.extend_from_slice(&3_u16.to_le_bytes());
        tiff.extend_from_slice(&1_u32.to_le_bytes());
        tiff.extend_from_slice(&orientation.to_le_bytes());
        tiff.extend_from_slice(&[0, 0]);
        tiff.extend_from_slice(&0_u32.to_le_bytes());
        let mut exif = b"Exif\0\0".to_vec();
        exif.extend_from_slice(&tiff);
        let segment_len = u16::try_from(exif.len() + 2).unwrap();
        let mut output = bytes[..2].to_vec();
        output.extend_from_slice(&[0xff, 0xe1]);
        output.extend_from_slice(&segment_len.to_be_bytes());
        output.extend_from_slice(&exif);
        output.extend_from_slice(&bytes[2..]);
        output
    }

    fn start(runtime: Runtime, root: &Path) -> ImageIntegration {
        ImageIntegration::start(runtime, root).unwrap()
    }

    #[test]
    fn accepts_jpeg_png_and_gif_and_preserves_original_bytes_and_metadata() {
        let runtime = runtime();
        let root = tempfile::tempdir().unwrap();
        let integration = start(runtime, root.path());
        let manager = integration.manager();
        for (name, content_type, bytes) in [
            ("portrait.jpg", "image/jpeg", jpeg_fixture(5, 3)),
            ("transparent.png", "image/png", png_fixture(5, 3)),
            ("moving.gif", "image/gif", gif_fixture(5, 3)),
        ] {
            let item = manager.create(name, content_type, &bytes).unwrap();
            assert_eq!(item.name, name);
            assert_eq!(item.content_type, content_type);
            assert_eq!(item.filesize, bytes.len() as u64);
            assert!(chrono::DateTime::parse_from_rfc3339(&item.uploaded_at).is_ok());
            assert!(is_valid_id(&item.id));
            assert_eq!(manager.read_original(&item.id).unwrap(), bytes);
        }
        assert_eq!(manager.list().unwrap().len(), 3);
    }

    #[test]
    fn rejects_unsupported_mime_before_writing_files() {
        let runtime = runtime();
        let root = tempfile::tempdir().unwrap();
        let integration = start(runtime, root.path());
        let manager = integration.manager();
        for content_type in [
            "image/webp",
            "image/jpg",
            "IMAGE/PNG",
            "image/png; charset=utf-8",
        ] {
            assert!(matches!(
                manager.create("x.png", content_type, &png_fixture(2, 2)),
                Err(Error::InvalidInput(_))
            ));
        }
        assert!(manager.list().unwrap().is_empty());
        assert_eq!(
            fs::read_dir(root.path().join(IMAGE_DIRECTORY))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn rejects_unidentified_and_truncated_image_data() {
        let runtime = runtime();
        let root = tempfile::tempdir().unwrap();
        let integration = start(runtime, root.path());
        let manager = integration.manager();
        let truncated_png = png_fixture(2, 2);
        let invalid_inputs: [&[u8]; 2] = [b"not an image", &truncated_png[..12]];
        for bytes in invalid_inputs {
            assert!(matches!(
                manager.create("bad.png", "image/png", bytes),
                Err(Error::InvalidInput(_))
            ));
        }
        assert!(manager.list().unwrap().is_empty());
    }

    #[test]
    fn rejects_uploads_over_ten_mib_before_decoding() {
        let runtime = runtime();
        let root = tempfile::tempdir().unwrap();
        let integration = start(runtime, root.path());
        let manager = integration.manager();
        let bytes = vec![0_u8; MAX_SIZE + 1];
        assert!(matches!(
            manager.create("large.jpg", "image/jpeg", &bytes),
            Err(Error::InvalidInput(message)) if message.contains("upload limit")
        ));
        assert!(manager.list().unwrap().is_empty());
    }

    #[test]
    fn accepts_a_small_valid_image_without_upscaling_thumbnail() {
        let runtime = runtime();
        let root = tempfile::tempdir().unwrap();
        let integration = start(runtime, root.path());
        let manager = integration.manager();
        let bytes = png_fixture(3, 2);
        let item = manager.create("tiny.png", "image/png", &bytes).unwrap();
        let thumbnail = manager
            .read_thumbnail(&item.id, "256x256-thumb.png")
            .unwrap();
        let decoded = decode_image(&thumbnail).unwrap();
        assert_eq!(decoded.dimensions(), (3, 2));
        assert!(
            root.path()
                .join(IMAGE_DIRECTORY)
                .join(&item.id)
                .join("256x256")
                .is_file()
        );
    }

    #[test]
    fn thumbnail_fit_preserves_aspect_and_never_crops() {
        let runtime = runtime();
        let root = tempfile::tempdir().unwrap();
        let integration = start(runtime, root.path());
        let manager = integration.manager();
        let bytes = png_fixture(800, 400);
        let item = manager.create("wide.png", "image/png", &bytes).unwrap();
        let thumbnail = manager.read_thumbnail(&item.id, "256x256").unwrap();
        assert_eq!(decode_image(&thumbnail).unwrap().dimensions(), (256, 128));
    }

    #[test]
    fn thumbnail_size_parser_matches_home_assistant_filename_prefix() {
        assert_eq!(
            parse_thumbnail_filename("256x256").unwrap(),
            ThumbnailSize::Size256
        );
        assert_eq!(
            parse_thumbnail_filename("512x512-avatar.png").unwrap(),
            ThumbnailSize::Size512
        );
        for filename in [
            "",
            "original",
            "257x257",
            "256x512",
            "256x256/../../x",
            "abc",
        ] {
            assert!(matches!(
                parse_thumbnail_filename(filename),
                Err(Error::InvalidInput(_))
            ));
        }
    }

    #[test]
    fn unknown_image_is_checked_before_thumbnail_filename() {
        let runtime = runtime();
        let root = tempfile::tempdir().unwrap();
        let integration = start(runtime, root.path());
        let manager = integration.manager();
        assert!(matches!(
            manager.read_thumbnail("00000000000000000000000000000000", "bad"),
            Err(Error::NotFound(_))
        ));
    }

    #[test]
    fn exif_orientation_is_applied_before_thumbnail_generation() {
        let raw = jpeg_fixture(2, 3);
        let expected = decode_image(&raw).unwrap().rotate90();
        let oriented = jpeg_with_exif_orientation(&raw, 6);
        let actual = decode_image(&oriented).unwrap();
        assert_eq!(actual.dimensions(), (3, 2));
        assert_eq!(actual, expected);

        let runtime = runtime();
        let root = tempfile::tempdir().unwrap();
        let integration = start(runtime, root.path());
        let manager = integration.manager();
        let item = manager
            .create("oriented.jpg", "image/jpeg", &oriented)
            .unwrap();
        let thumbnail = manager.read_thumbnail(&item.id, "256x256").unwrap();
        assert_eq!(decode_image(&thumbnail).unwrap().dimensions(), (3, 2));
    }

    #[test]
    fn png_thumbnail_keeps_alpha_pixels() {
        let runtime = runtime();
        let root = tempfile::tempdir().unwrap();
        let integration = start(runtime, root.path());
        let manager = integration.manager();
        let bytes = png_fixture(2, 2);
        let item = manager.create("alpha.png", "image/png", &bytes).unwrap();
        let thumbnail = manager.read_thumbnail(&item.id, "256x256").unwrap();
        let decoded = decode_image(&thumbnail).unwrap().to_rgba8();
        assert_eq!(decoded.get_pixel(0, 0).0[3], 96);
        assert_eq!(decoded.get_pixel(1, 0).0[3], 255);
    }

    #[test]
    fn jpeg_thumbnail_converts_alpha_to_rgb_and_uses_the_declared_format() {
        let runtime = runtime();
        let root = tempfile::tempdir().unwrap();
        let integration = start(runtime, root.path());
        let manager = integration.manager();
        let bytes = png_fixture(3, 2);
        // Home Assistant stores the submitted MIME type and decodes image data
        // by its bytes, then encodes thumbnails using the stored MIME type.
        let item = manager.create("rgba.jpg", "image/jpeg", &bytes).unwrap();
        assert_eq!(manager.read_original(&item.id).unwrap(), bytes);
        let thumbnail = manager.read_thumbnail(&item.id, "256x256").unwrap();
        assert_eq!(image::guess_format(&thumbnail).unwrap(), ImageFormat::Jpeg);
        assert_eq!(
            decode_image(&thumbnail).unwrap().color(),
            image::ColorType::Rgb8
        );
    }

    #[test]
    fn gif_thumbnail_is_a_decodable_gif_image() {
        let runtime = runtime();
        let root = tempfile::tempdir().unwrap();
        let integration = start(runtime, root.path());
        let manager = integration.manager();
        let item = manager
            .create("motion.gif", "image/gif", &gif_fixture(4, 3))
            .unwrap();
        let thumbnail = manager.read_thumbnail(&item.id, "256x256").unwrap();
        assert_eq!(image::guess_format(&thumbnail).unwrap(), ImageFormat::Gif);
        assert_eq!(decode_image(&thumbnail).unwrap().dimensions(), (4, 3));
    }

    #[test]
    fn cached_thumbnail_is_reused_after_original_is_unavailable() {
        let runtime = runtime();
        let root = tempfile::tempdir().unwrap();
        let integration = start(runtime, root.path());
        let manager = integration.manager();
        let item = manager
            .create("cached.png", "image/png", &png_fixture(5, 3))
            .unwrap();
        let first = manager.read_thumbnail(&item.id, "256x256").unwrap();
        fs::remove_file(
            root.path()
                .join(IMAGE_DIRECTORY)
                .join(&item.id)
                .join("original"),
        )
        .unwrap();
        assert_eq!(
            manager
                .read_thumbnail(&item.id, "256x256-anything")
                .unwrap(),
            first
        );
    }

    #[test]
    fn concurrent_thumbnail_requests_create_one_valid_cache() {
        let runtime = runtime();
        let root = tempfile::tempdir().unwrap();
        let integration = start(runtime, root.path());
        let manager = integration.manager();
        let item = manager
            .create("concurrent.png", "image/png", &png_fixture(900, 700))
            .unwrap();
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let manager = manager.clone();
                let id = item.id.clone();
                std::thread::spawn(move || manager.read_thumbnail(&id, "512x512").unwrap())
            })
            .collect();
        let outputs: Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect();
        assert!(outputs.iter().all(|bytes| bytes == &outputs[0]));
        assert_eq!(decode_image(&outputs[0]).unwrap().dimensions(), (512, 398));
    }

    #[test]
    fn update_changes_only_name_and_rejects_empty_name() {
        let runtime = runtime();
        let root = tempfile::tempdir().unwrap();
        let integration = start(runtime, root.path());
        let manager = integration.manager();
        let original = manager
            .create("before.png", "image/png", &png_fixture(4, 3))
            .unwrap();
        let updated = manager
            .update(
                &original.id,
                ImageUpdate {
                    name: Some("after.png".to_owned()),
                },
            )
            .unwrap();
        assert_eq!(updated.name, "after.png");
        assert_eq!(updated.id, original.id);
        assert_eq!(updated.content_type, original.content_type);
        assert_eq!(updated.filesize, original.filesize);
        assert_eq!(updated.uploaded_at, original.uploaded_at);
        assert!(matches!(
            manager.update(
                &original.id,
                ImageUpdate {
                    name: Some(String::new()),
                }
            ),
            Err(Error::InvalidInput(_))
        ));
    }

    #[test]
    fn empty_update_emits_an_updated_change_like_generic_collection() {
        let runtime = runtime();
        let root = tempfile::tempdir().unwrap();
        let integration = start(runtime, root.path());
        let manager = integration.manager();
        let item = manager
            .create("same.png", "image/png", &png_fixture(2, 2))
            .unwrap();
        let (_, mut events) = manager.snapshot_and_subscribe().unwrap();
        manager.update(&item.id, ImageUpdate::default()).unwrap();
        let event = events.try_recv().unwrap();
        assert_eq!(event.event_type, IMAGE_COLLECTION_UPDATED_SIGNAL);
        let change = &event.data["changes"][0];
        assert_eq!(change["change_type"], "updated");
        assert_eq!(change["image_id"], item.id);
        assert_eq!(change["item"]["id"], item.id);
    }

    #[test]
    fn exact_membership_snapshot_and_subscribe_include_added_change() {
        let runtime = runtime();
        let root = tempfile::tempdir().unwrap();
        let integration = start(runtime, root.path());
        let manager = integration.manager();
        let (snapshot, mut events) = manager.snapshot_and_subscribe().unwrap();
        assert!(snapshot.storage.is_empty());
        let item = manager
            .create("subscribed.png", "image/png", &png_fixture(2, 2))
            .unwrap();
        assert!(manager.contains(&item.id).unwrap());
        assert!(!manager.contains(&format!("{}suffix", item.id)).unwrap());
        let event = events.try_recv().unwrap();
        assert_eq!(event.event_type, IMAGE_COLLECTION_UPDATED_SIGNAL);
        let change = &event.data["changes"][0];
        assert_eq!(change["change_type"], "added");
        assert_eq!(change["image_id"], item.id);
        assert_eq!(change["item"]["content_type"], "image/png");
    }

    #[test]
    fn delete_removes_metadata_and_all_cached_files() {
        let runtime = runtime();
        let root = tempfile::tempdir().unwrap();
        let integration = start(runtime, root.path());
        let manager = integration.manager();
        let item = manager
            .create("delete.png", "image/png", &png_fixture(6, 4))
            .unwrap();
        manager.read_thumbnail(&item.id, "256x256").unwrap();
        let image_dir = root.path().join(IMAGE_DIRECTORY).join(&item.id);
        assert!(image_dir.is_dir());
        manager.delete(&item.id).unwrap();
        assert!(!image_dir.exists());
        assert_eq!(manager.get(&item.id).unwrap(), None);
        assert!(matches!(manager.delete(&item.id), Err(Error::NotFound(_))));
    }

    #[test]
    fn rejects_path_traversal_ids_without_touching_neighbor_files() {
        let runtime = runtime();
        let root = tempfile::tempdir().unwrap();
        let outside = root.path().join("outside");
        fs::write(&outside, b"keep").unwrap();
        let integration = start(runtime, root.path());
        let manager = integration.manager();
        for id in ["../outside", "../../outside", "/etc/passwd"] {
            assert!(matches!(manager.read_original(id), Err(Error::NotFound(_))));
        }
        assert_eq!(fs::read(outside).unwrap(), b"keep");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_image_directories_and_does_not_follow_them() {
        use std::os::unix::fs::symlink;

        let runtime = runtime();
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let sentinel = outside.path().join("original");
        fs::write(&sentinel, b"keep").unwrap();
        let integration = start(runtime, root.path());
        let manager = integration.manager();
        let item = manager
            .create("symlink.png", "image/png", &png_fixture(2, 2))
            .unwrap();
        let image_dir = root.path().join(IMAGE_DIRECTORY).join(&item.id);
        fs::remove_dir_all(&image_dir).unwrap();
        symlink(outside.path(), &image_dir).unwrap();
        assert!(matches!(
            manager.read_original(&item.id),
            Err(Error::Storage(_))
        ));
        assert!(matches!(manager.delete(&item.id), Err(Error::Storage(_))));
        assert_eq!(fs::read(sentinel).unwrap(), b"keep");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_thumbnail_cache_files() {
        use std::os::unix::fs::symlink;

        let runtime = runtime();
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let sentinel = outside.path().join("sentinel");
        fs::write(&sentinel, b"keep").unwrap();
        let integration = start(runtime, root.path());
        let manager = integration.manager();
        let item = manager
            .create("cache-link.png", "image/png", &png_fixture(2, 2))
            .unwrap();
        let cache_path = root
            .path()
            .join(IMAGE_DIRECTORY)
            .join(&item.id)
            .join("256x256");
        symlink(&sentinel, &cache_path).unwrap();
        assert!(matches!(
            manager.read_thumbnail(&item.id, "256x256"),
            Err(Error::Storage(_))
        ));
        assert_eq!(fs::read(sentinel).unwrap(), b"keep");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_a_symlink_as_the_explicit_data_directory() {
        use std::os::unix::fs::symlink;

        let runtime = runtime();
        let root = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        let link = root.path().join("link");
        symlink(target.path(), &link).unwrap();
        assert!(matches!(
            ImageIntegration::start(runtime, &link),
            Err(Error::InvalidInput(_))
        ));
    }

    #[test]
    fn upload_order_survives_update_and_runtime_restart() {
        let root = tempfile::tempdir().unwrap();
        let db_path = root.path().join("runtime.sqlite");
        let data_dir = root.path().join("config");
        let runtime = Runtime::new(Arc::new(SqliteStore::open(&db_path).unwrap())).unwrap();
        let integration = start(runtime.clone(), &data_dir);
        let manager = integration.manager();
        let first_bytes = png_fixture(7, 5);
        let second_bytes = png_fixture(9, 4);
        let first = manager
            .create("first.png", "image/png", &first_bytes)
            .unwrap();
        let second = manager
            .create("second.png", "image/png", &second_bytes)
            .unwrap();
        let first = manager
            .update(
                &first.id,
                ImageUpdate {
                    name: Some("renamed.png".to_owned()),
                },
            )
            .unwrap();
        assert_eq!(manager.list().unwrap(), vec![first.clone(), second.clone()]);
        integration.shutdown().unwrap();
        drop(manager);
        drop(runtime);

        let runtime = Runtime::new(Arc::new(SqliteStore::open(&db_path).unwrap())).unwrap();
        let restarted = start(runtime.clone(), &data_dir);
        let manager = restarted.manager();
        assert_eq!(manager.list().unwrap(), vec![first.clone(), second.clone()]);
        let (snapshot, _) = manager.snapshot_and_subscribe().unwrap();
        assert_eq!(snapshot.storage, vec![first.clone(), second.clone()]);
        assert_eq!(manager.get(&first.id).unwrap(), Some(first.clone()));
        assert_eq!(manager.read_original(&first.id).unwrap(), first_bytes);
        assert_eq!(manager.read_original(&second.id).unwrap(), second_bytes);
    }

    #[test]
    fn manager_handles_are_invalidated_after_shutdown() {
        let runtime = runtime();
        let root = tempfile::tempdir().unwrap();
        let integration = start(runtime, root.path());
        let manager = integration.manager();
        integration.shutdown().unwrap();
        assert!(matches!(manager.list(), Err(Error::Service(_))));
    }

    #[test]
    fn failed_runtime_metadata_write_cleans_the_new_image_directory() {
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(FailingMetadataStore::default());
        store.fail_next_set.store(true, Ordering::SeqCst);
        let runtime = Runtime::new(store).unwrap();
        let integration = start(runtime, root.path());
        let manager = integration.manager();
        assert!(matches!(
            manager.create("rollback.png", "image/png", &png_fixture(2, 2)),
            Err(Error::Storage(_))
        ));
        assert!(manager.list().unwrap().is_empty());
        assert_eq!(
            fs::read_dir(root.path().join(IMAGE_DIRECTORY))
                .unwrap()
                .count(),
            0
        );
    }

    #[derive(Default)]
    struct FailingMetadataStore {
        inner: MemoryStore,
        fail_next_set: AtomicBool,
    }

    impl Store for FailingMetadataStore {
        fn load_states(&self) -> Result<Vec<State>> {
            self.inner.load_states()
        }

        fn put_state(&self, state: &State, event: Option<&Event>) -> Result<()> {
            self.inner.put_state(state, event)
        }

        fn remove_state(&self, entity_id: &str, event: &Event) -> Result<()> {
            self.inner.remove_state(entity_id, event)
        }

        fn append_event(&self, event: &Event) -> Result<()> {
            self.inner.append_event(event)
        }

        fn get_data(&self, key: &str) -> Result<Option<Value>> {
            self.inner.get_data(key)
        }

        fn set_data(&self, key: &str, value: &Value) -> Result<()> {
            if self.fail_next_set.swap(false, Ordering::SeqCst) {
                return Err(Error::Storage("requested test failure".to_owned()));
            }
            self.inner.set_data(key, value)
        }

        fn delete_data(&self, key: &str) -> Result<bool> {
            self.inner.delete_data(key)
        }

        fn history(
            &self,
            entity_id: &str,
            start: chrono::DateTime<Utc>,
            end: chrono::DateTime<Utc>,
        ) -> Result<Vec<State>> {
            self.inner.history(entity_id, start, end)
        }

        fn history_query(&self, query: &HistoryQuery) -> Result<Vec<HistorySeries>> {
            self.inner.history_query(query)
        }

        fn event_history_query(&self, query: &EventHistoryQuery) -> Result<Vec<Event>> {
            self.inner.event_history_query(query)
        }

        fn purge_before(&self, cutoff: chrono::DateTime<Utc>) -> Result<PurgeResult> {
            self.inner.purge_before(cutoff)
        }

        fn repack(&self) -> Result<()> {
            self.inner.repack()
        }
    }
}
