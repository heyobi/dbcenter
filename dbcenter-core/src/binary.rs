use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use crate::config::{DBCenterConfig, DEFAULT_POSTGRES_VERSION};
use crate::error::{DBCenterError, Result};

static VERSION_CACHE: OnceLock<Mutex<HashMap<String, Vec<String>>>> = OnceLock::new();

/// Normalize a user-supplied PostgreSQL version to a Maven artifact version string
pub fn normalize_postgres_version(input: &str) -> Result<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(DBCenterError::ConfigError(
            "PostgreSQL version cannot be empty".to_string(),
        ));
    }

    if trimmed.chars().all(|c| c.is_ascii_digit()) {
        return Ok(trimmed.to_string());
    }

    if trimmed
        .chars()
        .all(|c| c.is_ascii_digit() || c == '.' || c == '_')
    {
        return Ok(trimmed.to_string());
    }

    Err(DBCenterError::ConfigError(format!(
        "Unsupported PostgreSQL version format: {}",
        trimmed
    )))
}

/// Get the base directory for DBCenter (~/.dbcenter/postgres-<version>)
pub fn base_dir_for_version(version: &str) -> Result<PathBuf> {
    let home = dirs::home_dir()
        .ok_or_else(|| DBCenterError::ConfigError("Cannot find home directory".to_string()))?;

    let base = home.join(".dbcenter").join(format!("postgres-{}", version));
    fs::create_dir_all(&base)?;
    Ok(base)
}

fn version_cache() -> &'static Mutex<HashMap<String, Vec<String>>> {
    VERSION_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Ensure PostgreSQL binaries are available
pub fn ensure_binaries(config: &DBCenterConfig) -> Result<(PathBuf, PathBuf)> {
    if let (Some(initdb), Some(postgres)) =
        (config.initdb_path.clone(), config.postgres_path.clone())
    {
        return Ok((initdb, postgres));
    }

    let version = &config.postgres_version;
    let base_dir = base_dir_for_version(version)?;

    let exe_suffix = if cfg!(windows) { ".exe" } else { "" };
    let postgres_name = format!("postgres{}", exe_suffix);
    let initdb_name = format!("initdb{}", exe_suffix);

    // Check if binaries already exist
    if let Some(initdb) = find_binary_in_dir(&base_dir, &initdb_name) {
        if let Some(postgres) = find_binary_in_dir(&base_dir, &postgres_name) {
            tracing::info!("Found existing binaries");
            return Ok((initdb, postgres));
        }
    }

    // Download and extract binaries
    tracing::info!(
        "Downloading PostgreSQL {} binaries from Maven Central...",
        version
    );
    download_and_extract_binaries(&base_dir, version)?;

    // Find binaries again
    let initdb = find_binary_in_dir(&base_dir, &initdb_name).ok_or_else(|| {
        DBCenterError::BinaryError("initdb not found after extraction".to_string())
    })?;
    let postgres = find_binary_in_dir(&base_dir, &postgres_name).ok_or_else(|| {
        DBCenterError::BinaryError("postgres not found after extraction".to_string())
    })?;

    // Make executable on Unix
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&initdb)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&initdb, perms)?;

        let mut perms = fs::metadata(&postgres)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&postgres, perms)?;
    }

    Ok((initdb, postgres))
}

/// Find a binary file in a directory tree
fn find_binary_in_dir(base_dir: &Path, binary_name: &str) -> Option<PathBuf> {
    for entry in walkdir::WalkDir::new(base_dir)
        .follow_links(true)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if entry.file_name() == binary_name {
            return Some(entry.path().to_path_buf());
        }
    }
    None
}

/// Download and extract PostgreSQL binaries from Maven Central
fn download_and_extract_binaries(base_dir: &Path, version: &str) -> Result<()> {
    let platform = current_platform()?;
    let url = get_download_url(platform, version);
    let jar_path = base_dir.join(format!("postgres_dist-{}.jar", version));

    tracing::info!("Downloading from: {}", url);
    let response = reqwest::blocking::get(&url).map_err(|e| {
        download_failure(platform, version, format!("Download request failed: {}", e))
    })?;

    if !response.status().is_success() {
        return Err(download_failure(
            platform,
            version,
            format!("Download failed with status: {}", response.status()),
        ));
    }

    let bytes = response.bytes().map_err(|e| {
        download_failure(
            platform,
            version,
            format!("Failed to read download response: {}", e),
        )
    })?;

    fs::write(&jar_path, bytes.as_ref())?;
    extract_jar_archive(base_dir, &jar_path)?;
    fs::remove_file(&jar_path)?;

    Ok(())
}

fn current_platform() -> Result<&'static str> {
    if cfg!(target_os = "windows") {
        Ok("windows-amd64")
    } else if cfg!(target_os = "macos") {
        Ok("darwin-amd64")
    } else if cfg!(target_os = "linux") {
        Ok("linux-amd64")
    } else {
        Err(DBCenterError::BinaryError(
            "Unsupported platform".to_string(),
        ))
    }
}

fn get_download_url(platform: &str, version: &str) -> String {
    format!(
        "https://repo1.maven.org/maven2/io/zonky/test/postgres/embedded-postgres-binaries-{}/{}/embedded-postgres-binaries-{}-{}.jar",
        platform, version, platform, version
    )
}

fn get_metadata_url(platform: &str) -> String {
    format!(
        "https://repo1.maven.org/maven2/io/zonky/test/postgres/embedded-postgres-binaries-{}/maven-metadata.xml",
        platform
    )
}

fn fetch_available_versions(platform: &str) -> Option<Vec<String>> {
    let metadata_url = get_metadata_url(platform);
    let response = reqwest::blocking::get(&metadata_url).ok()?;
    if !response.status().is_success() {
        return None;
    }

    let text = response.text().ok()?;
    let mut versions = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("<version>") {
            if let Some(end) = rest.find("</version>") {
                versions.push(rest[..end].to_string());
            }
        }
    }

    if versions.is_empty() {
        None
    } else {
        versions.sort();
        versions.dedup();
        Some(versions)
    }
}

fn download_failure(platform: &str, version: &str, reason: String) -> DBCenterError {
    let mut message = format!(
        "{} (platform: {}, requested version: {})",
        reason, platform, version
    );

    if let Some(versions) = available_versions(platform) {
        if !versions.is_empty() {
            message.push_str("\nAvailable versions: ");
            message.push_str(&versions.join(", "));
        }
    }

    DBCenterError::BinaryError(message)
}

pub fn resolve_artifact_version(
    requested: Option<&str>,
    existing_major: Option<u32>,
) -> Result<(String, Option<String>)> {
    let platform = current_platform()?;
    let available = available_versions(platform);

    let default_major = DEFAULT_POSTGRES_VERSION
        .split('.')
        .next()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(18);

    let mut warning = None;

    let requested_clean = requested.and_then(|s| {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    });

    let (effective_major, exact_candidate, warned) = match existing_major {
        Some(existing) => {
            if let Some(req) = requested_clean.as_deref() {
                if let Some(req_major) = parse_major(req) {
                    if req_major != existing {
                        warning = Some(format!(
                            "Existing database initialized with PostgreSQL {}. Using that version instead of requested {}.",
                            existing, req
                        ));
                    }
                } else {
                    warning = Some(format!(
                        "Existing database initialized with PostgreSQL {}. Ignoring unrecognized requested version {}.",
                        existing, req
                    ));
                }
            }
            (existing, None, warning.is_some())
        }
        None => {
            if let Some(req) = requested_clean.as_deref() {
                if req.contains('.') {
                    let major = parse_major(req).ok_or_else(|| {
                        DBCenterError::ConfigError(format!(
                            "Unsupported PostgreSQL version format: {}",
                            req
                        ))
                    })?;
                    (major, Some(req.to_string()), false)
                } else {
                    let major = req.parse::<u32>().map_err(|_| {
                        DBCenterError::ConfigError(format!(
                            "Unsupported PostgreSQL version format: {}",
                            req
                        ))
                    })?;
                    (major, None, false)
                }
            } else {
                (default_major, None, false)
            }
        }
    };

    let mut chosen = if let Some(candidate) = exact_candidate {
        match &available {
            Some(list) if list.iter().any(|v| v == &candidate) => candidate,
            Some(list) => {
                warning = Some(format!(
                    "Requested PostgreSQL version {} not available. Using latest {}.x instead.",
                    candidate, effective_major
                ));
                best_version_for_major(list, effective_major)
                    .unwrap_or_else(|| format!("{}.0.0", effective_major))
            }
            None => candidate,
        }
    } else {
        match &available {
            Some(list) => best_version_for_major(list, effective_major)
                .unwrap_or_else(|| format!("{}.0.0", effective_major)),
            None => format!("{}.0.0", effective_major),
        }
    };

    if chosen.is_empty() {
        chosen = format!("{}.0.0", effective_major);
    }

    // Ensure warning is only emitted when a mismatch occurred
    if warned && warning.is_none() {
        warning = Some(format!(
            "Existing database initialized with PostgreSQL {}. Using that version.",
            effective_major
        ));
    }

    Ok((chosen, warning))
}

fn best_version_for_major(versions: &[String], major: u32) -> Option<String> {
    versions
        .iter()
        .filter(|v| parse_major(v).map(|m| m == major).unwrap_or(false))
        .max_by_key(|v| version_tuple(v))
        .cloned()
}

pub fn detect_existing_major_version(path: &Path) -> Option<u32> {
    let version_file = path.join("PG_VERSION");
    let contents = fs::read_to_string(version_file).ok()?;
    parse_major(contents.trim())
}

fn available_versions(platform: &str) -> Option<Vec<String>> {
    if let Ok(cache) = version_cache().lock() {
        if let Some(cached) = cache.get(platform) {
            return Some(cached.clone());
        }
    }

    let fetched = fetch_available_versions(platform)?;
    if let Ok(mut cache) = version_cache().lock() {
        cache.insert(platform.to_string(), fetched.clone());
    }
    Some(fetched)
}

fn parse_major(input: &str) -> Option<u32> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }

    let numeric: String = trimmed.chars().take_while(|c| c.is_ascii_digit()).collect();

    if numeric.is_empty() {
        return None;
    }

    numeric.parse().ok()
}

fn version_tuple(value: &str) -> (u32, u32, u32, u32) {
    let mut parts = value.split(['.', '_']);
    let major = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
    let minor = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
    let patch = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
    let build = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
    (major, minor, patch, build)
}

/// Extract JAR archive and its inner archive
fn extract_jar_archive(base_dir: &Path, jar_path: &Path) -> Result<()> {
    use std::io::Read;

    tracing::info!("Extracting JAR archive...");

    let file = fs::File::open(jar_path)?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|e| DBCenterError::BinaryError(format!("Failed to open JAR: {}", e)))?;

    // Find inner archive (.txz or .zip)
    let mut inner_archive_data = None;
    let mut inner_name = String::new();

    for i in 0..archive.len() {
        let mut file = archive
            .by_index(i)
            .map_err(|e| DBCenterError::BinaryError(format!("Failed to read JAR entry: {}", e)))?;

        let name = file.name().to_string();
        if name.ends_with(".txz") || name.ends_with(".tar.gz") || name.ends_with(".zip") {
            tracing::info!("Found inner archive: {}", name);
            let mut data = Vec::new();
            file.read_to_end(&mut data)?;
            inner_archive_data = Some(data);
            inner_name = name;
            break;
        }
    }

    let data = inner_archive_data
        .ok_or_else(|| DBCenterError::BinaryError("No inner archive found in JAR".to_string()))?;

    // Extract inner archive
    if inner_name.ends_with(".zip") {
        extract_zip_data(base_dir, &data)?;
    } else {
        extract_tar_xz_data(base_dir, &data)?;
    }

    Ok(())
}

/// Extract ZIP data
fn extract_zip_data(base_dir: &Path, data: &[u8]) -> Result<()> {
    use std::io::Cursor;

    let cursor = Cursor::new(data);
    let mut archive = zip::ZipArchive::new(cursor)
        .map_err(|e| DBCenterError::BinaryError(format!("Failed to open ZIP: {}", e)))?;

    for i in 0..archive.len() {
        let mut file = archive
            .by_index(i)
            .map_err(|e| DBCenterError::BinaryError(format!("Failed to read ZIP entry: {}", e)))?;

        let outpath = base_dir.join(file.name());

        if file.is_dir() {
            fs::create_dir_all(&outpath)?;
        } else {
            if let Some(parent) = outpath.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut outfile = fs::File::create(&outpath)?;
            std::io::copy(&mut file, &mut outfile)?;
        }
    }

    Ok(())
}

/// Extract tar.xz data
fn extract_tar_xz_data(base_dir: &Path, data: &[u8]) -> Result<()> {
    use std::io::Cursor;

    let cursor = Cursor::new(data);
    let decompressor = xz2::read::XzDecoder::new(cursor);
    let mut archive = tar::Archive::new(decompressor);

    archive
        .unpack(base_dir)
        .map_err(|e| DBCenterError::BinaryError(format!("Failed to extract tar.xz: {}", e)))?;

    Ok(())
}

/// Find pg_ctl binary for a given data directory by reading PG_VERSION
pub fn find_pg_ctl_for_data_dir(data_dir: &Path) -> Option<PathBuf> {
    // Read PG_VERSION to determine the major version
    let pg_version_file = data_dir.join("PG_VERSION");
    let major_version = fs::read_to_string(&pg_version_file).ok()?;
    let major_version = major_version.trim();

    // Find the base directory for this version
    let home = dirs::home_dir()?;
    let base_pattern = home.join(".dbcenter");

    // Look for postgres-<version> directories
    if let Ok(entries) = fs::read_dir(&base_pattern) {
        for entry in entries.filter_map(|e| e.ok()) {
            let dir_name = entry.file_name();
            let dir_name_str = dir_name.to_string_lossy();

            // Match directories like postgres-17, postgres-17.0.0, etc.
            if dir_name_str.starts_with(&format!("postgres-{}", major_version)) {
                let pg_ctl_name = if cfg!(windows) {
                    "pg_ctl.exe"
                } else {
                    "pg_ctl"
                };
                if let Some(pg_ctl) = find_binary_in_dir(&entry.path(), pg_ctl_name) {
                    return Some(pg_ctl);
                }
            }
        }
    }

    None
}
