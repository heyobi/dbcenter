use crate::binary;
use crate::config::DBCenterConfig;
use crate::error::{DBCenterError, Result};
use crate::utils;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub struct PostgresProcess {
    /// The postmaster's PID. On Windows the server is started by `pg_ctl`,
    /// which exits once it has launched it, so this comes from `postmaster.pid`
    /// rather than from a child handle.
    pid: Option<u32>,
    host: String,
    port: u16,
    /// Unix socket directory, kept so callers can locate the socket and so the
    /// path stays owned for the lifetime of the process. Unused on Windows,
    /// which connects over TCP.
    #[allow(dead_code)]
    socket_dir: Option<PathBuf>,
}

/// Pick a directory for the Unix socket that PostgreSQL will actually accept.
///
/// `sockaddr_un.sun_path` holds 108 bytes including the terminator, so the
/// socket file - `<dir>/.s.PGSQL.<port>` - cannot exceed that. Placing it under
/// the data directory is the obvious choice and keeps everything together, but
/// a data directory more than about sixty characters deep pushes it over, and
/// PostgreSQL then fails to start with an error that points at the socket
/// rather than at the path length. Deep paths are ordinary in CI workspaces and
/// containers, so fall back to a short directory under the system temp dir,
/// named from a hash of the data directory so two databases never collide.
fn choose_socket_dir(data_dir: &Path, port: u16) -> Result<PathBuf> {
    // Longest name PostgreSQL will create inside the directory.
    let socket_file_len = format!("/.s.PGSQL.{}", port).len();
    const SUN_PATH_MAX: usize = 107;

    let preferred = data_dir.join("sockets");
    if preferred.as_os_str().len() + socket_file_len <= SUN_PATH_MAX {
        return Ok(preferred);
    }

    let digest = {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        data_dir.hash(&mut hasher);
        hasher.finish()
    };
    let fallback = std::env::temp_dir().join(format!("dbcenter-{:016x}", digest));

    if fallback.as_os_str().len() + socket_file_len > SUN_PATH_MAX {
        return Err(DBCenterError::ConfigError(format!(
            "No usable Unix socket directory: '{}' is too long and the temp \
             directory '{}' is too long as well. Set TMPDIR to a shorter path.",
            preferred.display(),
            fallback.display()
        )));
    }

    tracing::info!(
        "Data directory path is too long for a Unix socket; using {} instead",
        fallback.display()
    );
    Ok(fallback)
}

/// `pg_ctl`, which sits beside `postgres` in the distribution's `bin`.
fn pg_ctl_path(postgres_path: &Path) -> Option<PathBuf> {
    postgres_path.parent().and_then(|p| p.parent()).map(|p| {
        p.join("bin").join(if cfg!(windows) {
            "pg_ctl.exe"
        } else {
            "pg_ctl"
        })
    })
}

/// The postmaster's PID, from the first line of `postmaster.pid`.
fn postmaster_pid(data_dir: &Path) -> Option<u32> {
    fs::read_to_string(data_dir.join("postmaster.pid"))
        .ok()?
        .lines()
        .next()?
        .trim()
        .parse()
        .ok()
}

impl PostgresProcess {
    /// Start PostgreSQL server
    pub fn start(config: &DBCenterConfig) -> Result<Self> {
        let (_initdb_path, postgres_path) = binary::ensure_binaries(config)?;

        // Determine listen host and port
        let (listen_host, port, socket_dir) = if cfg!(windows) {
            let listen_host = config
                .host
                .clone()
                .unwrap_or_else(|| "127.0.0.1".to_string());
            let port = match config.port {
                Some(p) => p,
                None => utils::get_free_port()?,
            };
            (listen_host, port, None)
        } else {
            // On Unix the port only names the socket file, which is already
            // namespaced by its directory, so the default stays 5432.
            let port = config.port.unwrap_or(5432);
            let socket_dir = choose_socket_dir(&config.data_dir, port)?;
            fs::create_dir_all(&socket_dir)?;
            let host = socket_dir.to_string_lossy().to_string();
            (host, port, Some(socket_dir))
        };
        // Start PostgreSQL
        let log_path = config.data_dir.join("server.log");

        let mut cmd = Command::new(&postgres_path);

        // CRITICAL FIX: On Windows, add the bin directory AND root directory to PATH
        // This allows PostgreSQL to find DLLs for extensions like PostGIS which might be in root or bin
        #[cfg(windows)]
        {
            if let Some(bin_dir) = postgres_path.parent() {
                let mut new_path_parts = Vec::new();
                new_path_parts.push(bin_dir.to_string_lossy().to_string());

                if let Some(root_dir) = bin_dir.parent() {
                    new_path_parts.push(root_dir.to_string_lossy().to_string());
                }

                if let Ok(path_var) = std::env::var("PATH") {
                    new_path_parts.push(path_var);
                }

                let new_path = new_path_parts.join(";");
                cmd.env("PATH", new_path);
            }
        }

        cmd.arg("-D").arg(&config.data_dir);

        if cfg!(windows) {
            cmd.arg("-h").arg(&listen_host);
            cmd.arg("-p").arg(port.to_string());
            cmd.arg("-c").arg("dynamic_shared_memory_type=windows");
        } else {
            cmd.arg("-k").arg(socket_dir.as_ref().unwrap());
            // The port names the Unix socket file too, so it has to be passed
            // whether or not TCP is on. Leaving it to the TCP branch left a
            // socket-only server sitting on .s.PGSQL.5432 while every client
            // built from this config knocked on the configured port.
            cmd.arg("-p").arg(port.to_string());
            // The Unix socket is always there for local clients and the watcher.
            // TCP is off unless a host was configured: a database that is
            // embedded in a process has no business on the network by default,
            // but one running in a container has to be reachable from outside
            // it, so `host` opens that up explicitly.
            match &config.host {
                Some(tcp_host) if !tcp_host.is_empty() => {
                    cmd.arg("-h").arg(tcp_host);
                    tracing::info!("Also listening on TCP {}:{}", tcp_host, port);
                }
                _ => {
                    cmd.arg("-h").arg("");
                }
            }
        }

        // On Windows, through pg_ctl. PostgreSQL refuses to run under an
        // administrator's token ("Execution of PostgreSQL by a user with
        // administrative permissions is not permitted"), and starting
        // postgres.exe directly meant anyone running Python as administrator -
        // GitHub's Windows runners included - could not open a database at all.
        // pg_ctl launches the server with that token's privileges dropped, which
        // is PostgreSQL's own answer to it, and is harmless for everyone else.
        #[cfg(windows)]
        let started_by_pg_ctl = match pg_ctl_path(&postgres_path).filter(|p| p.exists()) {
            Some(pg_ctl) => {
                // Truncate the log and let go of it. pg_ctl has cmd.exe open it
                // for appending, and an open handle of ours here made that open
                // fail: postgres never ran, the log stayed empty, and pg_ctl -W
                // still reported success.
                fs::File::create(&log_path)?;
                start_with_pg_ctl(&pg_ctl, &cmd, &config.data_dir, &log_path)?;
                true
            }
            None => {
                tracing::warn!(
                    "pg_ctl.exe not found beside postgres.exe; starting the server \
                     directly, which fails under an administrator account"
                );
                false
            }
        };
        #[cfg(not(windows))]
        let started_by_pg_ctl = false;

        let mut pid = None;
        if !started_by_pg_ctl {
            let log_file = fs::File::create(&log_path)?;
            cmd.stdout(Stdio::from(log_file.try_clone()?));
            cmd.stderr(Stdio::from(log_file));
            let child = cmd.spawn().map_err(|e| {
                DBCenterError::ProcessError(format!("Failed to start PostgreSQL: {}", e))
            })?;
            pid = Some(child.id());
        }

        let mut process = PostgresProcess {
            pid,
            host: listen_host.clone(),
            port,
            socket_dir,
        };

        // Wait for server to be ready
        let conn_str = process.connection_string(config);
        wait_for_ready(&conn_str, &config.data_dir)?;

        if process.pid.is_none() {
            process.pid = postmaster_pid(&config.data_dir);
        }

        tracing::info!("PostgreSQL listening on {}:{}", listen_host, port);

        Ok(process)
    }

    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    pub fn get_port(&self) -> u16 {
        self.port
    }

    pub fn connection_string(&self, config: &DBCenterConfig) -> String {
        let user = &config.user;
        let dbname = &config.dbname;

        // For connection, always use localhost on Windows
        let conn_host = if cfg!(windows) {
            "127.0.0.1".to_string()
        } else {
            self.host.clone()
        };

        // The port belongs in the string on Unix too. A Unix socket is named
        // .s.PGSQL.<port>, so leaving it out makes libpq look for 5432 - which
        // was every port until a configurable one became possible, and then the
        // server listened on one socket while we knocked on another.
        if let Some(ref password) = config.password {
            format!(
                "host={} port={} dbname={} user={} password={}",
                conn_host, self.port, dbname, user, password
            )
        } else {
            format!(
                "host={} port={} dbname={} user={}",
                conn_host, self.port, dbname, user
            )
        }
    }

    /// Stop the server and, if the database is not being persisted, delete it.
    pub fn shutdown(&self, config: &DBCenterConfig) -> Result<()> {
        self.stop(config)?;

        // Cleanup if not persisting
        if !config.persist && !config.daemon_mode {
            // Wait for file handles to be released before attempting cleanup
            self.wait_for_directory_cleanup(&config.data_dir)?;
        }
        Ok(())
    }

    /// Stop the server and leave the data directory alone.
    ///
    /// Split out from `shutdown` for restarts. Deleting the data directory is
    /// what `persist = false` means at the end of a database's life, and doing
    /// it in the middle of a restart destroyed the database that was about to
    /// be started again - which is every non-persistent database that installs
    /// pg_duckdb, since that install has to restart to pick up
    /// shared_preload_libraries.
    pub fn stop(&self, config: &DBCenterConfig) -> Result<()> {
        // If daemon_mode is enabled, don't shutdown the process
        if config.daemon_mode {
            tracing::info!("Daemon mode enabled - PostgreSQL will continue running");
            return Ok(());
        }

        tracing::info!("Shutting down PostgreSQL at {:?}", config.data_dir);

        // Shutdown runs both on the explicit call and again from Drop. Without
        // postmaster.pid the server is already down, and going on to the
        // fallback would send SIGTERM to a PID that has exited - and that the
        // kernel is free to have handed to something else by now.
        if !config.data_dir.join("postmaster.pid").exists() {
            tracing::debug!("PostgreSQL is already stopped");
            return Ok(());
        }

        // First, try graceful shutdown with pg_ctl
        let pg_ctl_result = if let Ok((_, postgres_path)) = binary::ensure_binaries(config) {
            let pg_ctl_path = pg_ctl_path(&postgres_path);

            if let Some(pg_ctl) = pg_ctl_path {
                if pg_ctl.exists() {
                    tracing::info!("Using pg_ctl for graceful shutdown");
                    let output = Command::new(&pg_ctl)
                        .arg("stop")
                        .arg("-D")
                        .arg(&config.data_dir)
                        .arg("-m")
                        .arg("fast")
                        .arg("-t")
                        .arg("10")
                        .arg("-w") // Wait for shutdown to complete
                        .output();

                    match output {
                        Ok(result) if result.status.success() => {
                            tracing::info!("PostgreSQL shut down gracefully via pg_ctl");
                            Some(())
                        }
                        Ok(result) => {
                            tracing::warn!(
                                "pg_ctl failed: {}",
                                String::from_utf8_lossy(&result.stderr)
                            );
                            None
                        }
                        Err(e) => {
                            tracing::warn!("Failed to execute pg_ctl: {}", e);
                            None
                        }
                    }
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };

        // If pg_ctl didn't work, fall back to process termination
        if pg_ctl_result.is_none() {
            if let Some(pid) = self.pid {
                tracing::info!("Falling back to process termination for PID {}", pid);

                #[cfg(windows)]
                {
                    // Kill all postgres child processes first
                    let _ = Command::new("taskkill")
                        .args(["/F", "/T", "/PID", &pid.to_string()])
                        .output();

                    // Wait for process to actually terminate by polling
                    self.wait_for_process_termination(pid)?;
                }

                #[cfg(unix)]
                {
                    let _ = Command::new("kill")
                        .args(["-TERM", &pid.to_string()])
                        .output();

                    // Wait for process to actually terminate
                    self.wait_for_process_termination(pid)?;
                }
            }
        }

        // Additional cleanup: remove postmaster.pid if it exists
        let postmaster_pid = config.data_dir.join("postmaster.pid");
        if postmaster_pid.exists() {
            let _ = fs::remove_file(&postmaster_pid);
            tracing::info!("Removed postmaster.pid");
        }

        Ok(())
    }

    /// Wait for directory to be ready for cleanup by attempting removal with retries
    fn wait_for_directory_cleanup(&self, dir: &std::path::Path) -> Result<()> {
        let max_attempts = 20;
        let retry_interval = Duration::from_millis(250);

        for attempt in 1..=max_attempts {
            match fs::remove_dir_all(dir) {
                Ok(_) => {
                    tracing::info!("Removed data directory on attempt {}", attempt);
                    return Ok(());
                }
                Err(e) if attempt < max_attempts => {
                    tracing::debug!("Cleanup attempt {} failed: {}, retrying...", attempt, e);
                    std::thread::sleep(retry_interval);
                }
                Err(e) => {
                    tracing::error!(
                        "Failed to remove data directory after {} attempts: {}",
                        max_attempts,
                        e
                    );
                    return Err(DBCenterError::IoError(e));
                }
            }
        }

        Ok(())
    }

    /// Wait for a process to terminate by polling
    fn wait_for_process_termination(&self, pid: u32) -> Result<()> {
        let timeout = Duration::from_secs(10);
        let start = Instant::now();
        let poll_interval = Duration::from_millis(100);

        loop {
            if start.elapsed() > timeout {
                return Err(DBCenterError::ProcessError(format!(
                    "Process {} did not terminate within timeout",
                    pid
                )));
            }

            let is_running = self.is_process_running(pid)?;

            if !is_running {
                tracing::info!("Process {} terminated successfully", pid);
                return Ok(());
            }

            std::thread::sleep(poll_interval);
        }
    }

    /// Check if a process is still running
    fn is_process_running(&self, pid: u32) -> Result<bool> {
        #[cfg(windows)]
        {
            let output = Command::new("tasklist")
                .args(["/FI", &format!("PID eq {}", pid), "/NH"])
                .output()
                .map_err(|e| {
                    DBCenterError::ProcessError(format!("Failed to check process: {}", e))
                })?;

            let output_str = String::from_utf8_lossy(&output.stdout);
            Ok(output_str.contains(&pid.to_string()))
        }

        #[cfg(unix)]
        {
            let status = Command::new("kill")
                .args(["-0", &pid.to_string()])
                .status()
                .map_err(|e| {
                    DBCenterError::ProcessError(format!("Failed to check process: {}", e))
                })?;

            Ok(status.success())
        }
    }
}

/// Initialize database with initdb if needed
pub fn initialize_db(config: &DBCenterConfig) -> Result<()> {
    let pg_version_file = config.data_dir.join("PG_VERSION");

    if pg_version_file.exists() {
        tracing::info!("Database already initialized");

        // Update pg_hba.conf if password is set but auth was trust before
        if config.password.is_some() {
            update_pg_hba_for_password(&config.data_dir)?;
        }

        return Ok(());
    }

    // Create data directory if it doesn't exist
    fs::create_dir_all(&config.data_dir).map_err(DBCenterError::IoError)?;

    let (initdb_path, _) = binary::ensure_binaries(config)?;

    tracing::info!("Initializing database at {:?}", config.data_dir);

    // Determine auth method based on password
    let auth_method = if config.password.is_some() {
        "scram-sha-256"
    } else {
        "trust"
    };

    // Ensure parent directory exists for password file, but NOT data_dir itself
    // (initdb wants data_dir to be empty or non-existent)
    if let Some(parent) = config.data_dir.parent() {
        fs::create_dir_all(parent).map_err(DBCenterError::IoError)?;
    }

    let mut cmd = Command::new(&initdb_path);
    cmd.arg("-D")
        .arg(&config.data_dir)
        .arg("-U")
        .arg(&config.user)
        .arg("--auth")
        .arg(auth_method)
        .arg("--encoding=UTF8")
        .arg("--no-locale");

    // Set password if provided - use temporary file in parent directory
    let temp_pwfile = if let Some(ref password) = config.password {
        // Create temp file in parent directory, not inside data_dir
        let pwfile = config.data_dir.with_extension("initdb_password");
        fs::write(&pwfile, password.as_bytes()).map_err(DBCenterError::IoError)?;
        cmd.arg("--pwfile").arg(&pwfile);
        Some(pwfile)
    } else {
        None
    };

    let output = cmd
        .output()
        .map_err(|e| DBCenterError::ProcessError(format!("initdb failed: {}", e)))?;

    // Clean up temporary password file
    if let Some(pwfile) = temp_pwfile {
        let _ = fs::remove_file(pwfile);
    }

    if !output.status.success() {
        return Err(DBCenterError::ProcessError(format!(
            "initdb failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }

    // Update pg_hba.conf to allow network connections if host is not localhost
    update_pg_hba_for_network(
        &config.data_dir,
        config.host.as_deref(),
        config.password.is_some(),
    )?;

    // Configure dynamic_library_path for extensions
    // This is critical for PostGIS to find its dependencies from extensions folder
    update_postgresql_conf_library_path(&config.data_dir)?;

    tracing::info!(
        "Database initialized successfully with auth method: {}",
        auth_method
    );
    Ok(())
}

/// Update pg_hba.conf to allow network connections
fn update_pg_hba_for_network(
    data_dir: &std::path::Path,
    host: Option<&str>,
    has_password: bool,
) -> Result<()> {
    let pg_hba_path = data_dir.join("pg_hba.conf");

    // Check if we need network access (host is 0.0.0.0 or non-localhost)
    let needs_network =
        host.is_some_and(|h| h == "0.0.0.0" || (!h.starts_with("127.") && h != "localhost"));

    if needs_network {
        // Never `trust` from anywhere. Listening on a network interface without
        // a password used to write a rule that let anyone who could reach the
        // port in as the superuser. Requiring scram fails closed instead: with
        // no password set nobody can authenticate, which is the right outcome
        // for a database that was exposed by accident.
        if !has_password {
            tracing::error!(
                "PostgreSQL is listening on a network interface but no password \
                 is configured. Network connections will be refused until one is \
                 set; pass .password(..) or set DBCENTER_PASSWORD."
            );
        }
        let entry = "\n# Allow network connections\nhost    all             all             0.0.0.0/0               scram-sha-256\n".to_string();

        fs::OpenOptions::new()
            .append(true)
            .open(&pg_hba_path)
            .and_then(|mut file| {
                use std::io::Write;
                file.write_all(entry.as_bytes())
            })
            .map_err(DBCenterError::IoError)?;

        tracing::info!("Updated pg_hba.conf for network access with scram-sha-256");
    }

    Ok(())
}

/// Set a `postgresql.conf` setting, replacing the existing line if there is one.
///
/// Returns whether the file actually changed, which is what callers use to
/// decide if a restart is owed. Commented-out defaults count as the existing
/// line: PostgreSQL ships most settings that way, and appending a second
/// definition below one leaves two lines claiming the same key.
pub(crate) fn set_conf_setting(data_dir: &Path, key: &str, value: &str) -> Result<bool> {
    let conf_path = data_dir.join("postgresql.conf");
    let content = fs::read_to_string(&conf_path).map_err(DBCenterError::IoError)?;
    let setting = format!("{} = '{}'", key, value.replace('\'', "\\'"));

    let mut lines: Vec<String> = content.lines().map(|s| s.to_string()).collect();
    let mut replaced = false;
    for line in lines.iter_mut() {
        let trimmed = line.trim().trim_start_matches('#').trim_start();
        if trimmed.starts_with(key) && trimmed[key.len()..].trim_start().starts_with('=') {
            if *line == setting {
                return Ok(false);
            }
            *line = setting.clone();
            replaced = true;
            break;
        }
    }

    let updated = if replaced {
        let mut out = lines.join("\n");
        out.push('\n');
        out
    } else {
        format!("{}\n# DBCenter\n{}\n", content.trim_end(), setting)
    };

    fs::write(&conf_path, updated).map_err(DBCenterError::IoError)?;
    tracing::info!("postgresql.conf: {}", setting);
    Ok(true)
}

/// Add a library to `shared_preload_libraries` without dropping what is there.
///
/// This setting is a list, and it is the one setting an extension cannot ask
/// for at run time - PostgreSQL only reads it at startup. Overwriting it would
/// silently disable whatever a previous extension had put there, so the
/// existing entries are parsed and the new one appended.
pub(crate) fn add_shared_preload_library(data_dir: &Path, library: &str) -> Result<bool> {
    let conf_path = data_dir.join("postgresql.conf");
    let content = fs::read_to_string(&conf_path).map_err(DBCenterError::IoError)?;

    let mut existing: Vec<String> = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') || !trimmed.starts_with("shared_preload_libraries") {
            continue;
        }
        if let Some((_, rhs)) = trimmed.split_once('=') {
            let rhs = rhs.trim().trim_matches('\'');
            let rhs = rhs.split('#').next().unwrap_or(rhs);
            existing = rhs
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
        }
    }

    if existing.iter().any(|l| l == library) {
        return Ok(false);
    }
    existing.push(library.to_string());
    set_conf_setting(data_dir, "shared_preload_libraries", &existing.join(","))
}

/// Configure dynamic_library_path in postgresql.conf to include extension directories
fn update_postgresql_conf_library_path(data_dir: &std::path::Path) -> Result<()> {
    let conf_path = data_dir.join("postgresql.conf");

    // We want to add '$libdir' plus the extension directory relative to the installation
    // Usually it's in share/extension or similar, but for safety we can add multiple paths or just '$libdir'
    // Actually, looking at the error: "could not access file "$libdir/postgis-3": No such file or directory"
    // And Python fix was: dynamic_library_path = '.../lib'
    //
    // The issue is that usually extensions are installed into lib directory.
    // If we look at binary.rs, we see that we are running inside .dbcenter/postgres-version.
    // The structure usually is:
    //   bin/
    //   lib/
    //   share/
    //   data/
    //
    // PostGIS .so/.dll files should be in `lib`.
    // Default dynamic_library_path is usually '$libdir'.
    //
    // Use an absolute path to the lib directory of the current binary installation.
    // However, initialize_db only knows about data_dir.
    // We need to find the binary directory relative to data_dir OR assuming standard layout.
    //
    // BUT! data_dir can be anywhere. The binaries are in .dbcenter/postgres-X.
    // We need to find the lib dir relative to the postgres executable.

    // Let's try to locate the lib directory by finding pg_ctl.
    // We can recycle binary::find_pg_ctl_for_data_dir logic or just search up/around.

    if let Some(pg_ctl) = binary::find_pg_ctl_for_data_dir(data_dir) {
        if let Some(bin_dir) = pg_ctl.parent() {
            // Look for lib directory relative to bin
            if let Some(root_dir) = bin_dir.parent() {
                // $libdir comes first: it is where the distribution's own
                // modules live, and extension scripts reference it directly
                // (AS '$libdir/pg_trgm'). Extra directories are only a
                // fallback for extensions we unpack ourselves.
                let mut search_paths = vec!["$libdir".to_string()];

                // Modules usually sit in lib/postgresql; some distributions
                // put them straight in lib, or beside the binaries.
                for candidate in [
                    root_dir.join("lib").join("postgresql"),
                    root_dir.join("lib"),
                    root_dir.to_path_buf(),
                ] {
                    if !candidate.exists() {
                        continue;
                    }
                    let path_str = candidate
                        .to_string_lossy()
                        .replace("\\", "/")
                        .replace("'", "\\'");
                    if !search_paths.contains(&path_str) {
                        search_paths.push(path_str);
                    }
                }

                // PostgreSQL splits dynamic_library_path on the platform's
                // path separator: ';' on Windows, ':' everywhere else.
                // Using ';' on Linux collapses the whole list into one
                // nonexistent directory, so no extension can ever load.
                let separator = if cfg!(windows) { ";" } else { ":" };

                let setting = format!("dynamic_library_path = '{}'", search_paths.join(separator));

                // Read and update config
                if let Ok(content) = fs::read_to_string(&conf_path) {
                    let mut lines: Vec<String> = content.lines().map(|s| s.to_string()).collect();
                    let mut found = false;

                    for line in lines.iter_mut() {
                        let trimmed = line.trim();
                        if trimmed.starts_with("dynamic_library_path")
                            || (trimmed.starts_with("#")
                                && trimmed.contains("dynamic_library_path"))
                        {
                            *line = setting.clone();
                            found = true;
                            break;
                        }
                    }

                    let updated = if found {
                        lines.join("\n")
                    } else {
                        format!(
                            "{}\n# DBCenter: Extension library path\n{}\n",
                            content, setting
                        )
                    };

                    fs::write(&conf_path, updated).map_err(DBCenterError::IoError)?;
                    tracing::info!(
                        "Updated postgresql.conf with dynamic_library_path = {}",
                        setting
                    );
                }
            }
        }
    }

    Ok(())
}

/// Update pg_hba.conf when password is added to existing database
fn update_pg_hba_for_password(data_dir: &std::path::Path) -> Result<()> {
    let pg_hba_path = data_dir.join("pg_hba.conf");

    if let Ok(content) = fs::read_to_string(&pg_hba_path) {
        // Replace trust with scram-sha-256
        let updated = content.replace("trust", "scram-sha-256");
        fs::write(&pg_hba_path, updated).map_err(DBCenterError::IoError)?;
        tracing::info!("Updated pg_hba.conf to use scram-sha-256 authentication");
    }

    Ok(())
}

/// Wait for PostgreSQL to be ready
/// Start the server the way `cmd` would, but through `pg_ctl start`.
///
/// `cmd` is the `postgres` invocation already built for a direct start; its
/// arguments other than `-D` become pg_ctl's `-o` options and its environment
/// (the PATH that lets extensions find their DLLs) is carried over.
///
/// `-w`, so pg_ctl confirms the server came up and says what went wrong if it
/// did not, instead of reporting success for a launch that failed.
#[cfg(windows)]
fn start_with_pg_ctl(pg_ctl: &Path, cmd: &Command, data_dir: &Path, log_path: &Path) -> Result<()> {
    let mut options: Vec<String> = Vec::new();
    let mut args = cmd.get_args();
    while let Some(arg) = args.next() {
        if arg == "-D" {
            args.next();
            continue;
        }
        let arg = arg.to_string_lossy();
        // pg_ctl splices the options into a command line, so an argument
        // containing a space has to arrive quoted.
        options.push(if arg.contains(' ') {
            format!("\"{}\"", arg)
        } else {
            arg.into_owned()
        });
    }

    let mut pg_ctl_cmd = Command::new(pg_ctl);
    for (key, value) in cmd.get_envs() {
        match value {
            Some(value) => pg_ctl_cmd.env(key, value),
            None => pg_ctl_cmd.env_remove(key),
        };
    }
    // No pipes: the server pg_ctl leaves running inherits its handles, so
    // capturing its output would wait for an end-of-file that never comes.
    // Why a start failed is in the server log, which is reported instead.
    let status = pg_ctl_cmd
        .arg("start")
        .arg("-w")
        .arg("-t")
        .arg("60")
        .arg("-D")
        .arg(data_dir)
        .arg("-l")
        .arg(log_path)
        .arg("-o")
        .arg(options.join(" "))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| {
            DBCenterError::ProcessError(format!("Failed to run pg_ctl to start PostgreSQL: {}", e))
        })?;
    if !status.success() {
        let log = fs::read_to_string(log_path).unwrap_or_default();
        return Err(DBCenterError::ProcessError(format!(
            "pg_ctl could not start PostgreSQL ({}). Server log:\n{}",
            status, log
        )));
    }
    Ok(())
}

fn wait_for_ready(conn_str: &str, data_dir: &std::path::Path) -> Result<()> {
    use postgres::{Client, NoTls};

    let timeout = Duration::from_secs(15);
    let start = Instant::now();
    let poll_interval = Duration::from_millis(300);
    let mut last_error: Option<String> = None;

    tracing::info!("Waiting for PostgreSQL to be ready at: {}", conn_str);

    loop {
        if start.elapsed() > timeout {
            // Read server.log for error details
            let log_path = data_dir.join("server.log");
            let log_content = fs::read_to_string(&log_path)
                .unwrap_or_else(|_| "Could not read server.log".to_string());

            let last_lines: String = log_content
                .lines()
                .rev()
                .take(20)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join("\n");

            return Err(DBCenterError::ProcessError(format!(
                "PostgreSQL failed to start within timeout.\nConnection string: {}\nLast error: {:?}\nServer log (last 20 lines):\n{}",
                conn_str,
                last_error,
                last_lines
            )));
        }

        match Client::connect(conn_str, NoTls) {
            Ok(mut client) => match client.simple_query("SELECT 1") {
                Ok(_) => {
                    tracing::info!("PostgreSQL is ready");
                    return Ok(());
                }
                Err(e) => {
                    last_error = Some(format!("Query failed: {}", e));
                    std::thread::sleep(poll_interval);
                }
            },
            Err(e) => {
                last_error = Some(format!("Connection failed: {}", e));
                std::thread::sleep(poll_interval);
            }
        }
    }
}

/// Check if a process is running (global function for checking external processes)
pub fn is_process_running_global(pid: u32) -> bool {
    #[cfg(windows)]
    {
        Command::new("tasklist")
            .args(["/FI", &format!("PID eq {}", pid), "/NH"])
            .output()
            .map(|output| {
                let output_str = String::from_utf8_lossy(&output.stdout);
                output_str.contains(&pid.to_string())
            })
            .unwrap_or(false)
    }

    #[cfg(unix)]
    {
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }
}

/// Wait for a process to terminate
pub fn wait_for_pid(pid: u32, timeout_secs: u64) -> bool {
    let start = Instant::now();
    let timeout = Duration::from_secs(timeout_secs);

    while start.elapsed() < timeout {
        if !is_process_running_global(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}
