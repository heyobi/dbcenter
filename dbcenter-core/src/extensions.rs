use crate::config::DEFAULT_POSTGRES_VERSION;
use crate::error::{DBCenterError, Result};
use crate::DBCenter;
use indicatif::{ProgressBar, ProgressStyle};
use std::fs;
use std::io::Read;
use std::path::Path;

/// PostGIS version info for CDN mapping
#[derive(Debug, Clone)]
struct PostGISVersion {
    version: &'static str,
    min_pg: u32,
    max_pg: u32,
}

/// pgvector version info
#[derive(Debug, Clone)]
struct ExtensionVersion {
    version: &'static str,
    min_pg: u32,
    max_pg: u32,
}

/// pgvector versions (Windows)
const PGVECTOR_WINDOWS_VERSIONS: &[ExtensionVersion] = &[
    ExtensionVersion { version: "0.8.1", min_pg: 13, max_pg: 18 },
    ExtensionVersion { version: "0.8.0", min_pg: 15, max_pg: 17 },
];

/// pgvector versions (Linux)
const PGVECTOR_LINUX_VERSIONS: &[ExtensionVersion] = &[
    ExtensionVersion { version: "0.8.1", min_pg: 12, max_pg: 18 },
];

/// pghttp versions (Windows)
const PGHTTP_WINDOWS_VERSIONS: &[ExtensionVersion] = &[
    ExtensionVersion { version: "1.7.0", min_pg: 18, max_pg: 18 }, // Only 18 has 1.7.0 in list
    ExtensionVersion { version: "1.6.3", min_pg: 17, max_pg: 17 },
    ExtensionVersion { version: "1.6.1", min_pg: 13, max_pg: 16 },
];

/// pghttp versions (Linux)
const PGHTTP_LINUX_VERSIONS: &[ExtensionVersion] = &[
    ExtensionVersion { version: "1.7.0", min_pg: 12, max_pg: 18 },
];

/// Available PostGIS versions on CDN for Windows
/// postgis-3.6.1: pg13-18
/// postgis-3.5.0: pg16-17
/// postgis-3.4.2: pg12
const POSTGIS_WINDOWS_VERSIONS: &[PostGISVersion] = &[
    PostGISVersion {
        version: "3.6.1",
        min_pg: 13,
        max_pg: 18,
    },
    PostGISVersion {
        version: "3.5.0",
        min_pg: 16,
        max_pg: 17,
    },
    PostGISVersion {
        version: "3.4.2",
        min_pg: 12,
        max_pg: 12,
    },
];

/// Available PostGIS versions on CDN for Linux
/// postgis-3.5.0: pg12-17
/// postgis-3.4.3: pg12-17
/// postgis-3.3.7: pg12-17
/// postgis-3.2.7: pg12-15
/// postgis-3.1.11: pg12-15
const POSTGIS_LINUX_VERSIONS: &[PostGISVersion] = &[
    PostGISVersion {
        version: "3.5.0",
        min_pg: 12,
        max_pg: 17,
    },
    PostGISVersion {
        version: "3.4.3",
        min_pg: 12,
        max_pg: 17,
    },
    PostGISVersion {
        version: "3.3.7",
        min_pg: 12,
        max_pg: 17,
    },
    PostGISVersion {
        version: "3.2.7",
        min_pg: 12,
        max_pg: 15,
    },
    PostGISVersion {
        version: "3.1.11",
        min_pg: 12,
        max_pg: 15,
    },
];

/// Install PostgreSQL extension
pub fn install_extension(db: &DBCenter, name: &str) -> Result<()> {
    install_extension_with_version(db, name, None)
}

/// Install PostgreSQL extension with optional version
pub fn install_extension_with_version(
    db: &DBCenter,
    name: &str,
    version: Option<&str>,
) -> Result<()> {
    match name.to_lowercase().as_str() {
        "postgis" => install_postgis(db, version),
        "pgvector" => install_pgvector(db, version),
        "pghttp" | "pgsql_http" | "http" => install_pghttp(db, version),
        "pg_trgm" | "pgtrgm" => install_bundled_extension(db, "pg_trgm"),
        "pg_cron" | "pgcron" => {
            tracing::warn!("pg_cron support coming soon");
            Err(DBCenterError::ExtensionError(
                "pg_cron extension not yet implemented".to_string(),
            ))
        }
        _ => Err(DBCenterError::ExtensionError(format!(
            "Unknown extension: {}. Supported extensions: postgis, pgvector, pghttp, pg_trgm, pg_cron (coming soon)",
            name
        ))),
    }
}

/// Install PostGIS extension with optional version
fn install_postgis(db: &DBCenter, requested_version: Option<&str>) -> Result<()> {
    // Check if already installed
    let mut client = db.get_client();

    match client.query("SELECT PostGIS_Version() as version", &[]) {
        Ok(rows) => {
            if let Some(row) = rows.first() {
                let version: String = row.get(0);
                tracing::info!("PostGIS already installed: {}", version);
                return Ok(());
            }
        }
        Err(_) => {
            // Not installed, proceed with installation
        }
    }

    drop(client); // Release lock before downloading

    tracing::info!("Installing PostGIS extension...");

    if cfg!(windows) {
        install_postgis_windows(db, requested_version)?;
    } else {
        install_postgis_unix(db, requested_version)?;
    }

    // Activate extension in database
    let mut client = db.get_client();

    // First check if postgis.control exists
    let pg_root = get_postgres_root(db)?;
    let control_path = pg_root
        .join("share")
        .join("extension")
        .join("postgis.control");
    if !control_path.exists() {
        // Try alternate path
        let alt_control = pg_root
            .join("share")
            .join("postgresql")
            .join("extension")
            .join("postgis.control");
        if !alt_control.exists() {
            return Err(DBCenterError::ExtensionError(format!(
                "PostGIS control file not found. Expected at: {} or {}. The extension files may not have been extracted correctly.",
                control_path.display(), alt_control.display()
            )));
        }
    }

    match client.execute("CREATE EXTENSION IF NOT EXISTS postgis", &[]) {
        Ok(_) => {}
        Err(e) => {
            // Get detailed error info
            let err_msg = format!("{:?}", e);
            return Err(DBCenterError::ExtensionError(format!(
                "Failed to create extension: {}. Details: {}",
                e, err_msg
            )));
        }
    }

    tracing::info!("PostGIS extension activated");

    // Try optional extensions
    let _ = client.execute("CREATE EXTENSION IF NOT EXISTS postgis_raster", &[]);
    let _ = client.execute("CREATE EXTENSION IF NOT EXISTS postgis_topology", &[]);

    // Verify installation
    let rows = client
        .query("SELECT PostGIS_Version() as version", &[])
        .map_err(|e| {
            DBCenterError::ExtensionError(format!("PostGIS verification failed: {}", e))
        })?;

    if let Some(row) = rows.first() {
        let version: String = row.get(0);
        tracing::info!("PostGIS {} successfully installed!", version);
    }

    Ok(())
}

/// Install pgvector extension
fn install_pgvector(db: &DBCenter, requested_version: Option<&str>) -> Result<()> {
    install_simple_extension(
        db, 
        "vector", 
        "pgvector", 
        PGVECTOR_WINDOWS_VERSIONS, 
        PGVECTOR_LINUX_VERSIONS, 
        requested_version,
        "vector" // filename prefix
    )
}

/// Install pghttp extension
fn install_pghttp(db: &DBCenter, requested_version: Option<&str>) -> Result<()> {
     install_simple_extension(
        db, 
        "http", 
        "pghttp", 
        PGHTTP_WINDOWS_VERSIONS, 
        PGHTTP_LINUX_VERSIONS, 
        requested_version,
        "pghttp" // filename prefix
    )
}



/// Generic installer for bundled extensions (like pg_trgm)
fn install_bundled_extension(db: &DBCenter, ext_name: &str) -> Result<()> {
    let mut client = db.get_client();
    let create_sql = format!("CREATE EXTENSION IF NOT EXISTS \"{}\"", ext_name);
    
    match client.execute(&create_sql, &[]) {
        Ok(_) => {
            tracing::info!("Extension '{}' activated successfully.", ext_name);
            Ok(())
        },
        Err(e) => {
             tracing::error!("Failed to install bundled extension '{}': {}", ext_name, e);
             let err_msg = format!("{:?}", e);
             Err(DBCenterError::ExtensionError(format!(
                "Failed to create bundled extension '{}'. Ensure standard PostgreSQL contrib modules are installed. Details: {}",
                ext_name, err_msg
            )))
        }
    }
}

/// Generic installer for standard zip/tar extensions (like pgvector, pghttp)
fn install_simple_extension(
    db: &DBCenter,
    ext_name: &str, // e.g. "vector" or "http" (dbname)
    folder_name: &str, // e.g. "pgvector" (CDN folder)
    win_versions: &[ExtensionVersion],
    nix_versions: &[ExtensionVersion],
    requested_version: Option<&str>,
    file_prefix: &str, // e.g. "vector" or "pghttp"
) -> Result<()> {
    let mut client = db.get_client();
    // Check if installed using basic query
    let check_sql = format!("SELECT extname FROM pg_extension WHERE extname = '{}'", ext_name);
    match client.query(&check_sql, &[]) {
        Ok(rows) => {
            if !rows.is_empty() {
                tracing::info!("Extension '{}' already installed.", ext_name);
                return Ok(());
            }
        }
        Err(_) => {}
    }
    drop(client);

    tracing::info!("Installing {} extension...", ext_name);

    if cfg!(windows) {
        install_extension_windows(db, folder_name, win_versions, requested_version, file_prefix)?;
    } else {
        install_extension_unix(db, folder_name, nix_versions, requested_version, file_prefix)?;
    }

    // Activate
    let mut client = db.get_client();
    let create_sql = format!("CREATE EXTENSION IF NOT EXISTS \"{}\"", ext_name);
    
    match client.execute(&create_sql, &[]) {
        Ok(_) => {
            tracing::info!("Extension '{}' activated successfully.", ext_name);
            Ok(())
        },
        Err(e) => {
             let err_msg = format!("{:?}", e);
             Err(DBCenterError::ExtensionError(format!(
                "Failed to create extension '{}': {}. Details: {}",
                ext_name, e, err_msg
            )))
        }
    }
}

fn install_extension_windows(
    db: &DBCenter, 
    folder_name: &str,
    versions: &[ExtensionVersion], 
    requested_version: Option<&str>,
    file_prefix: &str
) -> Result<()> {
    let pg_version = postgres_major_version(db);
    // Reuse logic from find_best_postgis_version but need generic struct. 
    // Since ExtensionVersion and PostGISVersion are identical structure, we can cast or clone logic.
    // For simplicity, I'll essentially duplicate or adapt the finder logic locally or make finder generic.
    // Let's adapt logic here for now.
    
    let (ver_str, target_pg) = find_best_extension_version(pg_version, versions, requested_version)?;
    
    let url = format!(
        "https://cdn.db.center/extensions/{}/{}-{}-windows-amd64-pg{}.zip?token=admin-secret-key",
        folder_name, file_prefix, ver_str, target_pg
    );

    let base_dir = crate::binary::base_dir_for_version(db.postgres_version())?;
    let cache_dir = base_dir.join("cache").join("extensions");
    fs::create_dir_all(&cache_dir)?;

    let zip_path = cache_dir.join(format!("{}-{}-pg{}.zip", file_prefix, ver_str, target_pg));

    if !zip_path.exists() {
        download_with_progress(&url, &zip_path, &format!("{} {}", file_prefix, ver_str))?;
    } else {
        tracing::info!("Using cached {} {} bundle", file_prefix, ver_str);
    }

    let pg_root = get_postgres_root(db)?;
    extract_postgis_bundle(&zip_path, &pg_root)?; // Reuse the robust extractor

    Ok(())
}

fn install_extension_unix(
     db: &DBCenter, 
     folder_name: &str,
     versions: &[ExtensionVersion], 
     requested_version: Option<&str>,
     file_prefix: &str
) -> Result<()> {
    let pg_version = postgres_major_version(db);
    let (ver_str, target_pg) = find_best_extension_version(pg_version, versions, requested_version)?;

    let url = format!(
        "https://cdn.db.center/extensions/{}/{}-{}-linux-amd64-pg{}.tar.gz?token=admin-secret-key",
        folder_name, file_prefix, ver_str, target_pg
    );

    let base_dir = crate::binary::base_dir_for_version(db.postgres_version())?;
    let cache_dir = base_dir.join("cache").join("extensions");
    fs::create_dir_all(&cache_dir)?;

    let tar_path = cache_dir.join(format!("{}-{}-pg{}.tar.gz", file_prefix, ver_str, target_pg));

    if !tar_path.exists() {
        download_with_progress(&url, &tar_path, &format!("{} {}", file_prefix, ver_str))?;
    }

    let pg_root = get_postgres_root(db)?;
    extract_postgis_tarball(&tar_path, &pg_root)?;

    Ok(())
}

fn find_best_extension_version(
    pg_version: u32,
    versions: &[ExtensionVersion],
    requested_version: Option<&str>,
) -> Result<(&'static str, u32)> {
    if let Some(req_ver) = requested_version {
         for v in versions {
            if v.version == req_ver {
                if pg_version >= v.min_pg && pg_version <= v.max_pg {
                    return Ok((v.version, pg_version));
                } else if pg_version > v.max_pg {
                     tracing::warn!("PG {} too new for {} {}. Using pg{}", pg_version, req_ver, v.version, v.max_pg);
                     return Ok((v.version, v.max_pg));
                }
            }
         }
         return Err(DBCenterError::ExtensionError(format!("Requested version {} not found", req_ver)));
    }

    for v in versions {
        if pg_version >= v.min_pg && pg_version <= v.max_pg {
            return Ok((v.version, pg_version));
        }
    }
    
    // Fallback newest
    if let Some(v) = versions.first() {
        if pg_version > v.max_pg {
             tracing::warn!("PG {} too new. Using {} pg{}", pg_version, v.version, v.max_pg);
             return Ok((v.version, v.max_pg));
        }
    }

    Err(DBCenterError::ExtensionError(format!("No compatible version for PG {}", pg_version)))
}

/// Find the best PostGIS version for the given PostgreSQL version
fn find_best_postgis_version(
    pg_version: u32,
    versions: &[PostGISVersion],
    requested_version: Option<&str>,
) -> Result<(&'static str, u32)> {
    // If a specific version is requested, try to use it
    if let Some(req_ver) = requested_version {
        for v in versions {
            if v.version == req_ver {
                if pg_version >= v.min_pg && pg_version <= v.max_pg {
                    return Ok((v.version, pg_version));
                } else if pg_version > v.max_pg {
                    // Try with max supported pg version
                    tracing::warn!(
                        "PostgreSQL {} not supported by PostGIS {}. Using pg{} binaries (may have compatibility issues).",
                        pg_version, req_ver, v.max_pg
                    );
                    return Ok((v.version, v.max_pg));
                } else {
                    return Err(DBCenterError::ExtensionError(format!(
                        "PostGIS {} requires PostgreSQL {} or newer, but you have PostgreSQL {}.",
                        req_ver, v.min_pg, pg_version
                    )));
                }
            }
        }
        return Err(DBCenterError::ExtensionError(format!(
            "Requested PostGIS version {} not available on CDN.",
            req_ver
        )));
    }

    // Auto-select best version
    // First, try to find an exact match
    for v in versions {
        if pg_version >= v.min_pg && pg_version <= v.max_pg {
            tracing::info!(
                "Selected PostGIS {} for PostgreSQL {}",
                v.version,
                pg_version
            );
            return Ok((v.version, pg_version));
        }
    }

    // If PostgreSQL version is too new, use the latest PostGIS with its max supported pg version
    if let Some(v) = versions.first() {
        if pg_version > v.max_pg {
            tracing::warn!(
                "PostgreSQL {} is newer than PostGIS {} max support (pg{}). Using pg{} binaries.",
                pg_version,
                v.version,
                v.max_pg,
                v.max_pg
            );
            return Ok((v.version, v.max_pg));
        }
    }

    // If PostgreSQL version is too old
    Err(DBCenterError::ExtensionError(format!(
        "PostgreSQL {} is not supported. Minimum required: PostgreSQL 12.",
        pg_version
    )))
}

/// Download file with progress bar
fn download_with_progress(url: &str, dest_path: &Path, description: &str) -> Result<()> {
    use reqwest::blocking::Client;
    use std::io::Write;

    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(600)) // 10 minute timeout for large files
        .build()
        .map_err(|e| {
            DBCenterError::ExtensionError(format!("Failed to create HTTP client: {}", e))
        })?;

    let mut response = client
        .get(url)
        .send()
        .map_err(|e| DBCenterError::ExtensionError(format!("Download failed: {}", e)))?;

    if !response.status().is_success() {
        return Err(DBCenterError::ExtensionError(format!(
            "Download failed with status: {}. URL: {}",
            response.status(),
            url
        )));
    }

    let total_size = response.content_length().unwrap_or(0);

    // Create progress bar
    let pb = if total_size > 0 {
        let pb = ProgressBar::new(total_size);
        pb.set_style(
            ProgressStyle::default_bar()
                .template("{msg}\n{spinner:.green} [{elapsed_precise}] [{wide_bar:.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec}, {eta})")
                .unwrap_or_else(|_| ProgressStyle::default_bar())
                .progress_chars("#>-"),
        );
        pb.set_message(format!("Downloading {}", description));
        Some(pb)
    } else {
        tracing::info!("Downloading {} (size unknown)...", description);
        None
    };

    // Download with streaming
    let mut file = fs::File::create(dest_path)?;
    let mut downloaded: u64 = 0;
    let mut buffer = [0u8; 8192];

    loop {
        let bytes_read = response.read(&mut buffer).map_err(|e| {
            DBCenterError::ExtensionError(format!("Failed to read response: {}", e))
        })?;

        if bytes_read == 0 {
            break;
        }

        file.write_all(&buffer[..bytes_read])?;
        downloaded += bytes_read as u64;

        if let Some(ref pb) = pb {
            pb.set_position(downloaded);
        }
    }

    if let Some(pb) = pb {
        pb.finish_with_message(format!("{} downloaded successfully!", description));
    }

    tracing::info!("{} downloaded: {} bytes", description, downloaded);

    Ok(())
}

/// Install PostGIS on Windows
fn install_postgis_windows(db: &DBCenter, requested_version: Option<&str>) -> Result<()> {
    tracing::info!("Downloading PostGIS bundle for Windows...");

    let pg_version = postgres_major_version(db);
    let (postgis_version, target_pg_version) =
        find_best_postgis_version(pg_version, POSTGIS_WINDOWS_VERSIONS, requested_version)?;

    // Use cdn.db.center
    let url = format!(
        "https://cdn.db.center/extensions/postgis/postgis-{}-windows-amd64-pg{}.zip?token=admin-secret-key",
        postgis_version, target_pg_version
    );

    // Cache directory
    let base_dir = crate::binary::base_dir_for_version(db.postgres_version())?;
    let cache_dir = base_dir.join("cache").join("extensions");
    fs::create_dir_all(&cache_dir)?;

    let zip_path = cache_dir.join(format!(
        "postgis-{}-pg{}.zip",
        postgis_version, target_pg_version
    ));

    // Download if not cached
    if !zip_path.exists() {
        let description = format!(
            "PostGIS {} for PostgreSQL {}",
            postgis_version, target_pg_version
        );
        download_with_progress(&url, &zip_path, &description)?;
    } else {
        tracing::info!("Using cached PostGIS {} bundle", postgis_version);
    }

    // Extract to PostgreSQL directory
    let pg_root = get_postgres_root(db)?;
    extract_postgis_bundle(&zip_path, &pg_root)?;

    Ok(())
}

/// Install PostGIS on Unix (Linux/macOS)
fn install_postgis_unix(db: &DBCenter, requested_version: Option<&str>) -> Result<()> {
    tracing::info!("Installing PostGIS for Unix/Linux...");

    let pg_version = postgres_major_version(db);
    let (postgis_version, target_pg_version) =
        find_best_postgis_version(pg_version, POSTGIS_LINUX_VERSIONS, requested_version)?;

    let base_dir = crate::binary::base_dir_for_version(db.postgres_version())?;

    // Use cdn.db.center
    let url = format!(
        "https://cdn.db.center/extensions/postgis/postgis-{}-linux-amd64-pg{}.tar.gz?token=admin-secret-key",
        postgis_version, target_pg_version
    );

    let cache_dir = base_dir.join("cache").join("extensions");
    fs::create_dir_all(&cache_dir)?;

    let tar_path = cache_dir.join(format!(
        "postgis-{}-pg{}.tar.gz",
        postgis_version, target_pg_version
    ));

    if !tar_path.exists() {
        let description = format!(
            "PostGIS {} for PostgreSQL {}",
            postgis_version, target_pg_version
        );
        download_with_progress(&url, &tar_path, &description)?;
    } else {
        tracing::info!("Using cached PostGIS {} bundle", postgis_version);
    }

    // Extract
    let pg_root = get_postgres_root(db)?;
    extract_postgis_tarball(&tar_path, &pg_root)?;

    Ok(())
}

/// Extract PostGIS bundle (Windows)
fn extract_postgis_bundle(zip_path: &Path, pg_root: &Path) -> Result<()> {
    use zip::ZipArchive;

    tracing::info!("Extracting PostGIS files from {:?} to {:?}", zip_path, pg_root);

    let file = fs::File::open(zip_path)?;
    let mut archive = ZipArchive::new(file)
        .map_err(|e| DBCenterError::ExtensionError(format!("Failed to open ZIP: {}", e)))?;

    // First pass: check if there is a common root directory
    let mut common_root: Option<String> = None;
    let mut has_common_root = true;

    for i in 0..archive.len() {
        if let Ok(file) = archive.by_index(i) {
            let name = file.name();
            // Skip paths ending with / (directories) for root check, 
            // unless they are the only thing. 
            // Better: get the first component.
            let parts: Vec<&str> = name.split('/').collect();
            if parts.is_empty() {
                continue;
            }
            
            let first_part = parts[0];
            
            // If we haven't set a common root yet
            if common_root.is_none() {
                common_root = Some(first_part.to_string());
            } else {
                if common_root.as_deref() != Some(first_part) {
                    has_common_root = false;
                    break;
                }
            }
        }
    }
    
    // If we only have files at root (len 1 parts), common_root might set to filename.
    // If has_common_root is true, we must ensure that common_root is actually a directory 
    // that contains EVERY file. 
    // If the zip contains just "foo.txt", common_root="foo.txt". We shouldn't strip it.
    // So we should only strip if the zip ENTRIES imply a directory structure where everything 
    // is under `dir/`.
    // Actually, simple heuristic: if has_common_root is true, we strip the first component.
    // BUT we must be careful not to strip files if the "root" is just a file itself?
    // In a standard zip with a folder, you have `folder/`, `folder/file`.
    // If you have `file1`, `file2`, common root mismatch.
    // If you have `folder/file1`, `folder/file2`. Common root `folder`.
    // Using `strip_prefix` boolean.
    
    let strip_prefix = has_common_root && common_root.is_some();
    if strip_prefix {
        tracing::info!("Detected common root '{:?}', stripping it.", common_root);
    } else {
        tracing::info!("No common root detected, extracting as is.");
    }

    // Since ZipArchive consumes the file in our variable, and we can't easily reset it without seeking?
    // ZipArchive doesn't expose underlying reader easily for seek.
    // We should re-open the file.
    let file = fs::File::open(zip_path)?;
    let mut archive = ZipArchive::new(file)
        .map_err(|e| DBCenterError::ExtensionError(format!("Failed to reopen ZIP: {}", e)))?;

    let mut file_count = 0;

    for i in 0..archive.len() {
        let mut file = archive.by_index(i).map_err(|e| {
            DBCenterError::ExtensionError(format!("Failed to read ZIP entry: {}", e))
        })?;

        let name = file.name().to_string(); 
        let parts: Vec<&str> = name.split('/').collect();

        // Determine target path
        let target_path = if strip_prefix {
             if parts.len() <= 1 {
                 continue;
             }
             pg_root.join(&parts[1..].join("/"))
        } else {
             pg_root.join(&name)
        };

        if file.is_dir() {
            fs::create_dir_all(&target_path)?;
        } else {
            // Perform extraction
            // Check if we need to reorganize the file (e.g. DLLs at root)
            // This happens if zip is flat OR if zip has a container folder with flat files inside.
            // We use component count to robustly detect if the file is directly under pg_root
            let is_root_dll = name.to_lowercase().ends_with(".dll") && 
                              target_path.components().count() == pg_root.components().count() + 1;
            
            if is_root_dll {
                let filename = Path::new(&name).file_name().unwrap_or_default();
                
                // Extract directly to lib (avoiding root lock)
                let lib_target = pg_root.join("lib").join(filename);
                if let Some(parent) = lib_target.parent() { fs::create_dir_all(parent)?; }
                
                match fs::File::create(&lib_target) {
                    Ok(mut outfile) => {
                         if let Err(e) = std::io::copy(&mut file, &mut outfile) {
                             let is_lock_error = e.raw_os_error() == Some(32); // Windows sharing violation
                             if is_lock_error {
                                 tracing::warn!("Skipping locked file (lib): {} ({})", name, e);
                             } else {
                                 return Err(DBCenterError::ExtensionError(format!("Failed to write to lib {}: {}", name, e)));
                             }
                         } else {
                             file_count += 1;
                         }
                    }
                    Err(e) => {
                         let is_lock_error = e.raw_os_error() == Some(32);
                         if is_lock_error {
                             tracing::warn!("Skipping locked file (lib): {} ({})", name, e);
                         } else {
                             return Err(DBCenterError::ExtensionError(format!("Failed to create file in lib {}: {}", name, e)));
                         }
                    }
                }
                
                // Copy to bin
                let bin_target = pg_root.join("bin").join(filename);
                if let Some(parent) = bin_target.parent() { fs::create_dir_all(parent)?; }
                // ignore copy errors to bin if locked (unlikely since we just created lib file)
                // actually we copy FROM lib_target.
                // If lib_target skipped, we might fail here?
                if lib_target.exists() {
                     if let Err(e) = fs::copy(&lib_target, &bin_target) {
                         let is_lock_error = e.raw_os_error() == Some(32);
                         if is_lock_error {
                             tracing::warn!("Skipping locked file (bin): {} ({})", name, e);
                         } else {
                             tracing::warn!("Failed to copy to bin {}: {}", name, e);
                         }
                     }
                }
                
            } else {
                if let Some(parent) = target_path.parent() {
                    fs::create_dir_all(parent)?;
                }
                
                match fs::File::create(&target_path) {
                    Ok(mut outfile) => {
                        if let Err(e) = std::io::copy(&mut file, &mut outfile) {
                           let is_lock_error = e.raw_os_error() == Some(32);
                           if is_lock_error {
                               tracing::warn!("Skipping locked file: {} ({})", name, e);
                           } else {
                               return Err(DBCenterError::ExtensionError(format!("Failed to extract {}: {}", name, e)));
                           }
                        } else {
                           file_count += 1;
                        }
                    }
                    Err(e) => {
                        let is_lock_error = e.raw_os_error() == Some(32);
                        if is_lock_error {
                            tracing::warn!("Skipping locked file: {} ({})", name, e);
                        } else {
                            return Err(DBCenterError::ExtensionError(format!("Failed to create {}: {}", name, e)));
                        }
                    }
                }
            }
        }
    }

    tracing::info!("Extracted {} PostGIS files", file_count);
    Ok(())
}

/// Extract PostGIS tarball (Unix)
fn extract_postgis_tarball(tar_path: &Path, pg_root: &Path) -> Result<()> {
    use flate2::read::GzDecoder;
    use tar::Archive;

    tracing::info!("Extracting extension tarball from {:?} to {:?}", tar_path, pg_root);

    // Detect target layout quirks
    // Linux builds often use share/postgresql/extension instead of share/extension
    let use_share_postgresql = pg_root.join("share").join("postgresql").exists();
    let use_lib_postgresql = pg_root.join("lib").join("postgresql").exists();
    
    if use_share_postgresql {
        tracing::debug!("Detected share/postgresql layout, will adjust extension paths.");
    }
    if use_lib_postgresql {
         tracing::debug!("Detected lib/postgresql layout, will adjust library paths.");
    }

    // First pass: detect common root
    let tar_file = fs::File::open(tar_path)?;
    let decompressor = GzDecoder::new(tar_file);
    let mut archive = Archive::new(decompressor);

    let mut common_root: Option<String> = None;
    let mut has_common_root = true;
    let mut entries_count = 0;

    for entry in archive.entries().map_err(|e| DBCenterError::ExtensionError(format!("Failed to read tar entries: {}", e)))? {
        let file = entry.map_err(|e| DBCenterError::ExtensionError(format!("Failed to read tar entry: {}", e)))?;
        let path = file.path().map_err(|e| DBCenterError::ExtensionError(format!("Failed to get path: {}", e)))?;
        let path_str = path.to_string_lossy();
        
        let parts: Vec<&str> = path_str.split('/').collect();
        if parts.is_empty() { continue; }
        
        let first_part = parts[0];
        
        if common_root.is_none() {
            common_root = Some(first_part.to_string());
        } else if common_root.as_deref() != Some(first_part) {
            has_common_root = false;
            break; 
        }
        entries_count += 1;
    }

    if entries_count == 0 {
        return Err(DBCenterError::ExtensionError("Empty tarball".to_string()));
    }

    let strip_prefix = has_common_root && common_root.is_some();
    if strip_prefix {
        tracing::info!("Detected common root '{:?}', stripping it.", common_root);
    }

    // Re-open for extraction
    let tar_file = fs::File::open(tar_path)?;
    let decompressor = GzDecoder::new(tar_file);
    let mut archive = Archive::new(decompressor);

    for entry in archive.entries().map_err(|e| DBCenterError::ExtensionError(format!("Failed to read tar entries: {}", e)))? {
        let mut file = entry.map_err(|e| DBCenterError::ExtensionError(format!("Failed to read tar entry: {}", e)))?;
        let path = file.path().map_err(|e| DBCenterError::ExtensionError(format!("Failed to get path: {}", e)))?;
        let path_str = path.to_string_lossy().to_string(); // Own the string
        
        let mut parts: Vec<&str> = path_str.split('/').collect();
         
        if strip_prefix {
            if parts.len() <= 1 {
                continue; // Skip the root dir itself
            }
            parts = parts[1..].to_vec();
        }
        
        // Handle layout adjustments
        let final_path_buf; 
        if parts.is_empty() { continue; }

        if parts[0] == "share" && use_share_postgresql {
             // Inject "postgresql" -> share/postgresql/...
             let mut new_parts = vec!["share", "postgresql"];
             new_parts.extend_from_slice(&parts[1..]);
             final_path_buf = pg_root.join(new_parts.join("/"));
        } else if parts[0] == "lib" && use_lib_postgresql {
             // Inject "postgresql" -> lib/postgresql/...
             let mut new_parts = vec!["lib", "postgresql"];
             new_parts.extend_from_slice(&parts[1..]);
             final_path_buf = pg_root.join(new_parts.join("/"));
        } else {
             final_path_buf = pg_root.join(parts.join("/"));
        }
        
        let target_path = final_path_buf;

        if let Some(parent) = target_path.parent() {
            fs::create_dir_all(parent)?;
        }

        file.unpack(&target_path).map_err(|e| {
             DBCenterError::ExtensionError(format!("Failed to unpack to {}: {}", target_path.display(), e))
        })?;
    }

    tracing::info!("Extension files extracted");
    Ok(())
}

/// Get PostgreSQL root directory
fn get_postgres_root(db: &DBCenter) -> Result<std::path::PathBuf> {
    let base_dir = crate::binary::base_dir_for_version(db.postgres_version())?;

    // Check for pgsql subdirectory
    let pgsql_dir = base_dir.join("pgsql");
    if pgsql_dir.exists() {
        Ok(pgsql_dir)
    } else {
        Ok(base_dir)
    }
}

/// Get PostgreSQL major version
fn postgres_major_version(db: &DBCenter) -> u32 {
    let fallback = DEFAULT_POSTGRES_VERSION
        .split('.')
        .next()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(18);

    db.postgres_version()
        .split('.')
        .next()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(fallback)
}
