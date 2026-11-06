//! Home Assistant style YAML loading.
//!
//! Includes resolve relative to their containing file. Secret lookup walks
//! from the requesting file's directory toward the configuration root.

use anyhow::{Result, anyhow, bail};
use serde_yaml::{Mapping, Value};
use std::{
    collections::{HashMap, HashSet},
    env, fs,
    io::{ErrorKind, Read},
    path::{Path, PathBuf},
};

const MAX_FILE_BYTES: u64 = 1024 * 1024;
const MAX_TOTAL_BYTES: u64 = 8 * 1024 * 1024;
const MAX_FILES: usize = 1024;
const MAX_DIRECTORY_ENTRIES: usize = 16 * 1024;
const MAX_INCLUDE_DEPTH: usize = 64;
const MAX_VALUE_DEPTH: usize = 128;
const SECRETS_FILE: &str = "secrets.yaml";

/// Parse plain YAML. Tagged values need a filename and are resolved by `load`.
#[allow(dead_code)] // Public parsing contract; Config::parse is test-only in this crate.
pub fn parse(source: &str) -> Result<Value> {
    if source.len() as u64 > MAX_FILE_BYTES {
        bail!("YAML source exceeds the 1 MiB limit");
    }
    let mut value = parse_document(source, "<source>")?;
    reject_tags(&value, Path::new("<source>"), 0)?;
    if value.is_mapping() {
        merge_packages(&mut value, Path::new("<source>"))?;
    }
    Ok(value)
}

/// Load a YAML file, resolving Home Assistant includes, secrets, and env vars.
/// Package configuration is merged into the root while remaining under
/// `homeassistant.packages`, like Home Assistant Core.
pub fn load(path: &Path) -> Result<Value> {
    let path = canonicalize(path, "configuration file")?;
    let root_dir = path
        .parent()
        .ok_or_else(|| anyhow!("configuration file has no parent directory"))?
        .to_path_buf();
    let mut state = LoaderState::new(root_dir);
    let mut value = load_file(&path, &mut state, true, 0)?;
    if value.is_null() {
        value = Value::Mapping(Mapping::new());
    }
    merge_packages(&mut value, &path)?;
    Ok(value)
}

struct LoaderState {
    config_dir: PathBuf,
    active_files: HashSet<PathBuf>,
    secrets: HashMap<PathBuf, Mapping>,
    files_read: usize,
    directory_entries: usize,
    total_bytes: u64,
}

impl LoaderState {
    fn new(config_dir: PathBuf) -> Self {
        Self {
            config_dir,
            active_files: HashSet::new(),
            secrets: HashMap::new(),
            files_read: 0,
            directory_entries: 0,
            total_bytes: 0,
        }
    }
}

fn load_file(
    path: &Path,
    state: &mut LoaderState,
    allow_secrets: bool,
    depth: usize,
) -> Result<Value> {
    if depth > MAX_INCLUDE_DEPTH {
        bail!(
            "YAML include depth exceeds {MAX_INCLUDE_DEPTH} at {}",
            path.display()
        );
    }
    let path = canonicalize(path, "YAML file")?;
    if !state.active_files.insert(path.clone()) {
        bail!("YAML include cycle detected at {}", path.display());
    }
    let result = load_file_inner(&path, state, allow_secrets, depth);
    state.active_files.remove(&path);
    result
}

fn load_file_inner(
    path: &Path,
    state: &mut LoaderState,
    allow_secrets: bool,
    depth: usize,
) -> Result<Value> {
    state.files_read += 1;
    if state.files_read > MAX_FILES {
        bail!(
            "YAML load exceeds the {MAX_FILES}-file limit at {}",
            path.display()
        );
    }
    let metadata =
        fs::metadata(path).map_err(|_| anyhow!("cannot read YAML file {}", path.display()))?;
    if !metadata.is_file() {
        bail!("YAML path is not a file: {}", path.display());
    }
    if metadata.len() > MAX_FILE_BYTES {
        bail!("YAML file exceeds the 1 MiB limit: {}", path.display());
    }
    let remaining_budget = MAX_TOTAL_BYTES.saturating_sub(state.total_bytes);
    let read_limit = MAX_FILE_BYTES.min(remaining_budget).saturating_add(1);
    let mut bytes = Vec::new();
    fs::File::open(path)
        .map_err(|_| anyhow!("cannot read YAML file {}", path.display()))?
        .take(read_limit)
        .read_to_end(&mut bytes)
        .map_err(|_| anyhow!("cannot read YAML file {}", path.display()))?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        bail!("YAML file exceeds the 1 MiB limit: {}", path.display());
    }
    state.total_bytes = state.total_bytes.saturating_add(bytes.len() as u64);
    if state.total_bytes > MAX_TOTAL_BYTES {
        bail!(
            "YAML load exceeds the 8 MiB total byte budget at {}",
            path.display()
        );
    }
    let source = std::str::from_utf8(&bytes)
        .map_err(|_| anyhow!("YAML file is not UTF-8: {}", path.display()))?;
    let value = parse_document(source, path.to_string_lossy().as_ref())?;
    resolve_value(value, path, state, allow_secrets, depth, 0)
}

fn parse_document(source: &str, filename: &str) -> Result<Value> {
    serde_yaml::from_str(source).map_err(|error: serde_yaml::Error| {
        if let Some(location) = error.location() {
            anyhow!(
                "invalid YAML in {filename} at line {}, column {}",
                location.line(),
                location.column()
            )
        } else {
            anyhow!("invalid YAML in {filename}")
        }
    })
}

fn reject_tags(value: &Value, source: &Path, depth: usize) -> Result<()> {
    if depth > MAX_VALUE_DEPTH {
        bail!(
            "YAML nesting depth exceeds {MAX_VALUE_DEPTH} in {}",
            source.display()
        );
    }
    match value {
        Value::Tagged(_) => bail!(
            "tagged YAML is not supported by parse in {}",
            source.display()
        ),
        Value::Sequence(items) => {
            for item in items {
                reject_tags(item, source, depth + 1)?;
            }
        }
        Value::Mapping(items) => {
            for (key, item) in items {
                reject_tags(key, source, depth + 1)?;
                reject_tags(item, source, depth + 1)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn resolve_value(
    value: Value,
    source: &Path,
    state: &mut LoaderState,
    allow_secrets: bool,
    include_depth: usize,
    value_depth: usize,
) -> Result<Value> {
    if value_depth > MAX_VALUE_DEPTH {
        bail!(
            "YAML nesting depth exceeds {MAX_VALUE_DEPTH} in {}",
            source.display()
        );
    }
    match value {
        Value::Tagged(tagged) => {
            let tag = tagged.tag.to_string();
            match tag.as_str() {
                "!include" => {
                    let name = tagged_string(&tagged.value, source, "!include")?;
                    let value = load_file(
                        &resolve_path(source, name),
                        state,
                        allow_secrets,
                        include_depth + 1,
                    )?;
                    Ok(if value.is_null() {
                        Value::Mapping(Mapping::new())
                    } else {
                        value
                    })
                }
                "!include_dir_list" => {
                    let dir = tagged_string(&tagged.value, source, "!include_dir_list")?;
                    let mut result = Vec::new();
                    for file in collect_yaml_files(&resolve_path(source, dir), state)? {
                        if is_secrets_file(&file) {
                            continue;
                        }
                        let value = load_file(&file, state, allow_secrets, include_depth + 1)?;
                        if !value.is_null() {
                            result.push(value);
                        }
                    }
                    Ok(Value::Sequence(result))
                }
                "!include_dir_named" => {
                    let dir = tagged_string(&tagged.value, source, "!include_dir_named")?;
                    let mut result = Mapping::new();
                    for file in collect_yaml_files(&resolve_path(source, dir), state)? {
                        if is_secrets_file(&file) {
                            continue;
                        }
                        let stem =
                            file.file_stem()
                                .and_then(|name| name.to_str())
                                .ok_or_else(|| {
                                    anyhow!("invalid YAML filename in {}", file.display())
                                })?;
                        let mut value = load_file(&file, state, allow_secrets, include_depth + 1)?;
                        if value.is_null() {
                            value = Value::Mapping(Mapping::new());
                        }
                        // HA assigns by basename in traversal order; the last
                        // lexically ordered match wins when names collide.
                        result.insert(Value::String(stem.to_owned()), value);
                    }
                    Ok(Value::Mapping(result))
                }
                "!include_dir_merge_named" => {
                    let dir = tagged_string(&tagged.value, source, "!include_dir_merge_named")?;
                    let mut result = Mapping::new();
                    for file in collect_yaml_files(&resolve_path(source, dir), state)? {
                        if is_secrets_file(&file) {
                            continue;
                        }
                        if let Value::Mapping(items) =
                            load_file(&file, state, allow_secrets, include_depth + 1)?
                        {
                            // HA mapping.update semantics: later files replace matching keys.
                            for (key, value) in items {
                                result.insert(key, value);
                            }
                        }
                    }
                    Ok(Value::Mapping(result))
                }
                "!include_dir_merge_list" => {
                    let dir = tagged_string(&tagged.value, source, "!include_dir_merge_list")?;
                    let mut result = Vec::new();
                    for file in collect_yaml_files(&resolve_path(source, dir), state)? {
                        if is_secrets_file(&file) {
                            continue;
                        }
                        if let Value::Sequence(mut items) =
                            load_file(&file, state, allow_secrets, include_depth + 1)?
                        {
                            result.append(&mut items);
                        }
                    }
                    Ok(Value::Sequence(result))
                }
                "!secret" => {
                    if !allow_secrets {
                        bail!(
                            "secrets are not supported while loading {}",
                            source.display()
                        );
                    }
                    let name = tagged_string(&tagged.value, source, "!secret")?;
                    lookup_secret(name, source, state, include_depth)
                }
                "!env_var" => resolve_env_var(&tagged.value, source),
                _ => bail!("unsupported YAML tag {tag} in {}", source.display()),
            }
        }
        Value::Sequence(mut items) => {
            for item in &mut items {
                *item = resolve_value(
                    std::mem::replace(item, Value::Null),
                    source,
                    state,
                    allow_secrets,
                    include_depth,
                    value_depth + 1,
                )?;
            }
            Ok(Value::Sequence(items))
        }
        Value::Mapping(items) => {
            let mut resolved = Mapping::new();
            for (key, value) in items {
                let key = resolve_value(
                    key,
                    source,
                    state,
                    allow_secrets,
                    include_depth,
                    value_depth + 1,
                )?;
                let value = resolve_value(
                    value,
                    source,
                    state,
                    allow_secrets,
                    include_depth,
                    value_depth + 1,
                )?;
                resolved.insert(key, value);
            }
            Ok(Value::Mapping(resolved))
        }
        scalar => Ok(scalar),
    }
}

fn tagged_string<'a>(value: &'a Value, source: &Path, tag: &str) -> Result<&'a str> {
    value
        .as_str()
        .ok_or_else(|| anyhow!("{tag} requires a string argument in {}", source.display()))
}

fn resolve_path(source: &Path, requested: &str) -> PathBuf {
    let path = Path::new(requested);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        source.parent().unwrap_or_else(|| Path::new(".")).join(path)
    }
}

fn canonicalize(path: &Path, description: &str) -> Result<PathBuf> {
    fs::canonicalize(path).map_err(|error| {
        if error.kind() == ErrorKind::NotFound {
            anyhow!("{description} does not exist: {}", path.display())
        } else {
            anyhow!("cannot access {description}: {}", path.display())
        }
    })
}

fn collect_yaml_files(directory: &Path, state: &mut LoaderState) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    collect_yaml_files_inner(directory, state, &mut files, 0)?;
    Ok(files)
}

fn collect_yaml_files_inner(
    directory: &Path,
    state: &mut LoaderState,
    files: &mut Vec<PathBuf>,
    depth: usize,
) -> Result<()> {
    if depth > MAX_INCLUDE_DEPTH {
        bail!(
            "YAML include directory depth exceeds {MAX_INCLUDE_DEPTH} at {}",
            directory.display()
        );
    }
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(_) => bail!("cannot read YAML include directory {}", directory.display()),
    };
    let (mut directories, mut yaml_files) = (Vec::new(), Vec::new());
    for entry in entries {
        state.directory_entries += 1;
        if state.directory_entries > MAX_DIRECTORY_ENTRIES {
            bail!(
                "YAML directory traversal exceeds the entry limit at {}",
                directory.display()
            );
        }
        let entry = entry
            .map_err(|_| anyhow!("cannot read YAML include directory {}", directory.display()))?;
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let kind = entry.file_type().map_err(|_| {
            anyhow!(
                "cannot inspect YAML include entry in {}",
                directory.display()
            )
        })?;
        if kind.is_dir() {
            directories.push(entry.path());
        } else if kind.is_file()
            && entry.path().extension().and_then(|ext| ext.to_str()) == Some("yaml")
        {
            yaml_files.push(entry.path());
        }
    }
    directories.sort();
    yaml_files.sort();
    files.extend(yaml_files);
    for directory in directories {
        collect_yaml_files_inner(&directory, state, files, depth + 1)?;
    }
    Ok(())
}

fn is_secrets_file(path: &Path) -> bool {
    path.file_name().and_then(|name| name.to_str()) == Some(SECRETS_FILE)
}

fn resolve_env_var(value: &Value, source: &Path) -> Result<Value> {
    let args = tagged_string(value, source, "!env_var")?
        .split_whitespace()
        .collect::<Vec<_>>();
    let Some(name) = args.first() else {
        bail!(
            "!env_var requires an environment variable name in {}",
            source.display()
        );
    };
    match env::var(name) {
        Ok(value) => Ok(Value::String(value)),
        Err(_) if args.len() > 1 => Ok(Value::String(args[1..].join(" "))),
        // Never include the default argument in an error: it may be sensitive.
        Err(_) => bail!(
            "environment variable for !env_var is not defined in {}",
            source.display()
        ),
    }
}

fn lookup_secret(
    name: &str,
    requester: &Path,
    state: &mut LoaderState,
    depth: usize,
) -> Result<Value> {
    let mut directory = requester
        .parent()
        .ok_or_else(|| anyhow!("requesting YAML file has no directory"))?;
    loop {
        if !directory.starts_with(&state.config_dir) {
            break;
        }
        let path = directory.join(SECRETS_FILE);
        let secrets = load_secrets(&path, state, depth)?;
        if let Some(value) = secrets.get(Value::String(name.to_owned())) {
            return Ok(value.clone());
        }
        if directory == state.config_dir {
            break;
        }
        let Some(parent) = directory.parent() else {
            break;
        };
        directory = parent;
    }
    bail!("secret {name} is not defined for {}", requester.display())
}

fn load_secrets(path: &Path, state: &mut LoaderState, depth: usize) -> Result<Mapping> {
    let canonical = match fs::canonicalize(path) {
        Ok(path) => path,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Mapping::new()),
        Err(_) => bail!("cannot access secrets file {}", path.display()),
    };
    if let Some(secrets) = state.secrets.get(&canonical) {
        return Ok(secrets.clone());
    }
    let value = load_file(&canonical, state, false, depth + 1)?;
    let Value::Mapping(mapping) = value else {
        bail!("secrets file must contain a mapping: {}", path.display());
    };
    state.secrets.insert(canonical, mapping.clone());
    Ok(mapping)
}

fn merge_packages(config: &mut Value, source: &Path) -> Result<()> {
    let Some(root) = config.as_mapping() else {
        bail!("configuration in {} must be a mapping", source.display());
    };
    let Some(homeassistant) = root
        .get(Value::String("homeassistant".into()))
        .and_then(Value::as_mapping)
    else {
        return Ok(());
    };
    let Some(packages) = homeassistant.get(Value::String("packages".into())) else {
        return Ok(());
    };
    let Some(packages) = packages.as_mapping() else {
        bail!(
            "homeassistant.packages in {} must be a mapping",
            source.display()
        );
    };
    let mut sections = Vec::new();
    for (name, definition) in packages {
        let Some(name) = name.as_str() else {
            bail!("package names in {} must be strings", source.display());
        };
        if !is_slug(name) {
            bail!("invalid package name in {}", source.display());
        }
        let Some(definition) = definition.as_mapping() else {
            bail!(
                "package definition in {} must be a mapping",
                source.display()
            );
        };
        for (domain, value) in definition {
            let Some(domain) = domain.as_str() else {
                bail!(
                    "package section names in {} must be strings",
                    source.display()
                );
            };
            if !is_domain(domain) {
                bail!("invalid package section name in {}", source.display());
            }
            if domain != "homeassistant" {
                sections.push((domain.to_owned(), value.clone()));
            }
        }
    }
    for (domain, value) in sections {
        let root = config.as_mapping_mut().expect("root mapping was checked");
        let key = Value::String(domain.clone());
        let current = root.remove(&key).unwrap_or(Value::Null);
        let value = merge_package_section(current, value, source, &domain)?;
        root.insert(key, value);
    }
    Ok(())
}

fn merge_package_section(
    mut current: Value,
    package: Value,
    source: &Path,
    domain: &str,
) -> Result<Value> {
    // HA asks each integration schema whether a component is list based.
    // These are the list sections represented by this crate's strict config.
    if matches!(domain, "automation" | "scene" | "rest" | "template") {
        let mut items = match current {
            Value::Null => Vec::new(),
            Value::Sequence(items) => items,
            value => vec![value],
        };
        match package {
            Value::Null => {}
            Value::Sequence(package_items) => items.extend(package_items),
            package_value => items.push(package_value),
        }
        items.retain(|value| !is_falsy(value));
        return Ok(Value::Sequence(items));
    }
    match package {
        Value::Null => Ok(if current.is_null() {
            Value::Mapping(Mapping::new())
        } else {
            current
        }),
        Value::Sequence(mut package_items) => {
            let mut items = match current {
                Value::Sequence(items) => items,
                Value::Null => Vec::new(),
                value => vec![value],
            };
            items.append(&mut package_items);
            items.retain(|value| !is_falsy(value));
            Ok(Value::Sequence(items))
        }
        Value::Mapping(package_mapping) => {
            if package_mapping.is_empty() {
                return Ok(if current.is_null() {
                    Value::Mapping(Mapping::new())
                } else {
                    current
                });
            }
            if current.is_null() {
                current = Value::Mapping(Mapping::new());
            }
            let Value::Mapping(mut mapping) = current else {
                bail!(
                    "package configuration requires a mapping in {}",
                    source.display()
                );
            };
            merge_mapping(&mut mapping, &package_mapping, source)?;
            Ok(Value::Mapping(mapping))
        }
        _ => bail!(
            "package configuration has an unsupported value in {}",
            source.display()
        ),
    }
}

fn merge_mapping(current: &mut Mapping, package: &Mapping, source: &Path) -> Result<()> {
    for (key, package_value) in package {
        if key.as_str().is_none() {
            bail!(
                "package configuration contains a non-string key in {}",
                source.display()
            );
        }
        let existing = current.get(key).cloned().unwrap_or(Value::Null);
        let merged = match package_value {
            Value::Mapping(nested) if !nested.is_empty() => {
                let mut current_nested = match existing {
                    Value::Null => Mapping::new(),
                    Value::Mapping(mapping) => mapping,
                    _ => bail!(
                        "package configuration has a duplicate scalar entry in {}",
                        source.display()
                    ),
                };
                merge_mapping(&mut current_nested, nested, source)?;
                Value::Mapping(current_nested)
            }
            Value::Mapping(_) => {
                if existing.is_null() {
                    Value::Mapping(Mapping::new())
                } else {
                    existing
                }
            }
            Value::Sequence(items) => {
                let mut merged = match existing {
                    Value::Null => Vec::new(),
                    Value::Sequence(items) => items,
                    value => vec![value],
                };
                merged.extend(items.iter().cloned());
                merged.retain(|value| !is_falsy(value));
                Value::Sequence(merged)
            }
            scalar => {
                if !existing.is_null() {
                    bail!(
                        "package configuration has a duplicate scalar entry in {}",
                        source.display()
                    );
                }
                scalar.clone()
            }
        };
        current.insert(key.clone(), merged);
    }
    Ok(())
}

fn is_slug(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

fn is_domain(value: &str) -> bool {
    is_slug(value)
}

fn is_falsy(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Bool(value) => !value,
        Value::Number(value) => {
            value.as_i64() == Some(0) || value.as_u64() == Some(0) || value.as_f64() == Some(0.0)
        }
        Value::String(value) => value.is_empty(),
        Value::Sequence(value) => value.is_empty(),
        Value::Mapping(value) => value.is_empty(),
        Value::Tagged(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, path::Path};
    use tempfile::tempdir;

    fn write(path: impl AsRef<Path>, contents: &str) {
        let path = path.as_ref();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn get<'a>(value: &'a Value, key: &str) -> &'a Value {
        value
            .as_mapping()
            .unwrap()
            .get(Value::String(key.into()))
            .unwrap()
    }

    #[test]
    fn parse_plain_yaml_and_reject_tags() {
        assert_eq!(
            get(&parse("key: value\n").unwrap(), "key").as_str(),
            Some("value")
        );
        assert!(
            parse("key: !include other.yaml\n")
                .unwrap_err()
                .to_string()
                .contains("tagged YAML")
        );
    }

    #[test]
    fn fixture_resolves_relative_include_and_secret() {
        let dir = tempdir().unwrap();
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/yaml/basic");
        copy_tree(&fixture, dir.path());
        let value = load(&dir.path().join("configuration.yaml")).unwrap();
        assert_eq!(get(&value, "name").as_str(), Some("fixture"));
        assert_eq!(
            get(get(&value, "light"), "token").as_str(),
            Some("fixture-secret")
        );
        assert_eq!(
            get(get(&value, "light"), "details").as_str(),
            Some("nested include")
        );
    }

    #[test]
    fn include_directories_are_recursive_sorted_and_skip_hidden_and_secrets() {
        let dir = tempdir().unwrap();
        write(
            dir.path().join("configuration.yaml"),
            "list: !include_dir_list parts\nnamed: !include_dir_named parts\nmerged_list: !include_dir_merge_list parts\nmerged_named: !include_dir_merge_named parts\n",
        );
        write(dir.path().join("parts/b.yaml"), "- b\n");
        write(dir.path().join("parts/a.yaml"), "- a\n");
        write(dir.path().join("parts/nested/a.yaml"), "- nested-a\n");
        write(dir.path().join("parts/.hidden.yaml"), "- hidden\n");
        write(
            dir.path().join("parts/secrets.yaml"),
            "token: should-not-be-included\n",
        );
        write(
            dir.path().join("parts/values.yaml"),
            "shared: root\nonly_root: yes\n",
        );
        write(
            dir.path().join("parts/nested/values.yaml"),
            "shared: nested\nonly_nested: yes\n",
        );
        let value = load(&dir.path().join("configuration.yaml")).unwrap();
        let list = get(&value, "list").as_sequence().unwrap();
        assert_eq!(list.len(), 5);
        assert_eq!(list[0].as_sequence().unwrap()[0].as_str(), Some("a"));
        assert_eq!(list[1].as_sequence().unwrap()[0].as_str(), Some("b"));
        assert_eq!(list[3].as_sequence().unwrap()[0].as_str(), Some("nested-a"));
        let named = get(&value, "named").as_mapping().unwrap();
        assert!(named.get(Value::String("secrets".into())).is_none());
        assert!(named.get(Value::String(".hidden".into())).is_none());
        // Two nested files have the same basename; the later sorted path wins.
        assert_eq!(
            named
                .get(Value::String("a".into()))
                .unwrap()
                .as_sequence()
                .unwrap()[0]
                .as_str(),
            Some("nested-a")
        );
        assert_eq!(
            named
                .get(Value::String("values".into()))
                .unwrap()
                .as_mapping()
                .unwrap()
                .get(Value::String("shared".into()))
                .unwrap()
                .as_str(),
            Some("nested")
        );
        let merged_list = get(&value, "merged_list").as_sequence().unwrap();
        assert_eq!(merged_list.len(), 3);
        assert_eq!(merged_list[2].as_str(), Some("nested-a"));
        let merged = get(&value, "merged_named").as_mapping().unwrap();
        assert_eq!(
            merged.get(Value::String("shared".into())).unwrap().as_str(),
            Some("nested")
        );
        assert_eq!(
            merged
                .get(Value::String("only_root".into()))
                .unwrap()
                .as_str(),
            Some("yes")
        );
    }

    #[test]
    fn secrets_are_hierarchical_missing_secrets_fail_without_values() {
        let dir = tempdir().unwrap();
        write(
            dir.path().join("configuration.yaml"),
            "root_secret: !secret token\nchild: !include sub/child.yaml\n",
        );
        write(
            dir.path().join("secrets.yaml"),
            "token: root-value\ninherited: from-root\n",
        );
        write(
            dir.path().join("sub/child.yaml"),
            "local: !secret token\ninherited: !secret inherited\n",
        );
        write(dir.path().join("sub/secrets.yaml"), "token: local-value\n");
        let value = load(&dir.path().join("configuration.yaml")).unwrap();
        assert_eq!(get(&value, "root_secret").as_str(), Some("root-value"));
        assert_eq!(
            get(get(&value, "child"), "local").as_str(),
            Some("local-value")
        );
        assert_eq!(
            get(get(&value, "child"), "inherited").as_str(),
            Some("from-root")
        );
        write(dir.path().join("missing.yaml"), "value: !secret absent\n");
        let error = load(&dir.path().join("missing.yaml"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("absent"));
        assert!(!error.contains("root-value"));
    }

    #[test]
    fn env_var_defaults_work_and_are_not_echoed_on_errors() {
        let dir = tempdir().unwrap();
        write(
            dir.path().join("configuration.yaml"),
            "fallback: !env_var YAML_LOADER_UNSET safe default\nmissing: !env_var YAML_LOADER_UNSET\n",
        );
        let error = load(&dir.path().join("configuration.yaml"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("environment variable"));
        assert!(!error.contains("safe default"));
        write(
            dir.path().join("configuration.yaml"),
            "fallback: !env_var YAML_LOADER_UNSET safe default\n",
        );
        assert_eq!(
            get(
                &load(&dir.path().join("configuration.yaml")).unwrap(),
                "fallback"
            )
            .as_str(),
            Some("safe default")
        );
    }

    #[test]
    fn malformed_yaml_is_filename_aware_and_does_not_echo_values() {
        let dir = tempdir().unwrap();
        write(
            dir.path().join("configuration.yaml"),
            "child: !include bad.yaml\n",
        );
        write(
            dir.path().join("bad.yaml"),
            "password: hidden-secret\nbroken: [\n",
        );
        let error = load(&dir.path().join("configuration.yaml"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("bad.yaml"));
        assert!(error.contains("line"));
        assert!(!error.contains("hidden-secret"));
    }

    #[test]
    fn include_cycles_are_filename_aware() {
        let dir = tempdir().unwrap();
        write(
            dir.path().join("configuration.yaml"),
            "child: !include child.yaml\n",
        );
        write(
            dir.path().join("child.yaml"),
            "root: !include configuration.yaml\n",
        );
        let error = load(&dir.path().join("configuration.yaml"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("cycle"));
        assert!(error.contains("configuration.yaml"));
    }

    #[test]
    fn include_depth_and_file_size_limits_are_enforced() {
        let dir = tempdir().unwrap();
        write(
            dir.path().join("configuration.yaml"),
            "child: !include f0.yaml\n",
        );
        for index in 0..=MAX_INCLUDE_DEPTH + 1 {
            let next = format!("f{}.yaml", index + 1);
            let contents = if index < MAX_INCLUDE_DEPTH + 1 {
                format!("child: !include {next}\n")
            } else {
                "done: true\n".to_owned()
            };
            write(dir.path().join(format!("f{index}.yaml")), &contents);
        }
        let error = load(&dir.path().join("configuration.yaml"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("include depth exceeds"));

        let oversized = "x".repeat(MAX_FILE_BYTES as usize + 1);
        write(dir.path().join("oversized.yaml"), &oversized);
        let error = load(&dir.path().join("oversized.yaml"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("1 MiB limit"));
    }

    #[test]
    fn packages_merge_list_and_mapping_values_but_preserve_source_definition() {
        let dir = tempdir().unwrap();
        write(
            dir.path().join("configuration.yaml"),
            "homeassistant:\n  packages:\n    lights:\n      input_boolean:\n        package_light:\n          name: From package\n      automation:\n        id: package_automation\ninput_boolean:\n  existing:\n    name: Existing\nautomation:\n  - id: base_automation\n",
        );
        let value = load(&dir.path().join("configuration.yaml")).unwrap();
        let booleans = get(&value, "input_boolean").as_mapping().unwrap();
        assert!(booleans.contains_key(Value::String("existing".into())));
        assert!(booleans.contains_key(Value::String("package_light".into())));
        let automation = get(&value, "automation").as_sequence().unwrap();
        assert_eq!(automation.len(), 2);
        assert_eq!(
            get(&automation[1], "id").as_str(),
            Some("package_automation")
        );
        assert!(
            get(get(&value, "homeassistant"), "packages")
                .as_mapping()
                .unwrap()
                .contains_key(Value::String("lights".into()))
        );
    }

    #[test]
    fn parse_applies_package_merging_to_tag_free_yaml() {
        let value = parse("homeassistant:\n  packages:\n    helpers:\n      input_boolean:\n        office:\n          name: Office\n").unwrap();
        assert_eq!(
            get(get(&value, "input_boolean"), "office")
                .as_mapping()
                .unwrap()
                .get(Value::String("name".into()))
                .unwrap()
                .as_str(),
            Some("Office")
        );
        assert!(
            get(get(&value, "homeassistant"), "packages")
                .as_mapping()
                .unwrap()
                .contains_key(Value::String("helpers".into()))
        );
    }

    #[test]
    fn package_duplicate_scalar_is_an_error_without_value_disclosure() {
        let dir = tempdir().unwrap();
        write(
            dir.path().join("configuration.yaml"),
            "homeassistant:\n  packages:\n    conflict:\n      input_text:\n        label:\n          name: package-secret-value\ninput_text:\n  label:\n    name: root-value\n",
        );
        let error = load(&dir.path().join("configuration.yaml"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("duplicate scalar entry"));
        assert!(!error.contains("package-secret-value"));
        assert!(!error.contains("root-value"));
    }

    fn copy_tree(from: &Path, to: &Path) {
        fs::create_dir_all(to).unwrap();
        for entry in fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            let destination = to.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                copy_tree(&entry.path(), &destination);
            } else {
                fs::copy(entry.path(), destination).unwrap();
            }
        }
    }
}
