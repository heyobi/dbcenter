use std::fs::OpenOptions;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize)]
struct WatcherConfig {
    data_dir: PathBuf,
    persist: bool,
    service: bool,
    postgres_pid: Option<u32>,
    instance_id: Option<String>,
}

// Simple logging macro that writes to file
macro_rules! log {
    ($file:expr, $($arg:tt)*) => {{
        let msg = format!($($arg)*);
        let _ = writeln!($file, "[{}] {}", chrono_lite(), msg);
        let _ = $file.flush();
        eprintln!("[DBCENTER-WATCHER] {}", msg);
    }};
}

fn chrono_lite() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    format!("{}.{:03}", now.as_secs(), now.subsec_millis())
}

/// Check if a process is still running
#[cfg(windows)]
fn is_process_running(pid: u32) -> bool {
    use std::process::Command;

    let output = Command::new("tasklist")
        .args(["/FI", &format!("PID eq {}", pid), "/NH"])
        .output();

    match output {
        Ok(result) => {
            let stdout = String::from_utf8_lossy(&result.stdout);
            stdout.contains(&pid.to_string())
        }
        Err(_) => false,
    }
}

#[cfg(unix)]
fn is_process_running(pid: u32) -> bool {
    use std::path::Path;
    Path::new(&format!("/proc/{}", pid)).exists()
}

/// Wait for a process to terminate with timeout
fn wait_for_process_termination(log_file: &mut std::fs::File, pid: u32, timeout_secs: u32) -> bool {
    for i in 0..timeout_secs * 2 {
        if !is_process_running(pid) {
            log!(
                log_file,
                "Process {} sonlandi ({:.1} saniyede)",
                pid,
                (i as f32) / 2.0
            );
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    false
}

/// Clean Windows extended path prefix
fn clean_path(path: &PathBuf) -> PathBuf {
    let path_str = path.to_string_lossy();
    if path_str.starts_with(r"\\?\") {
        PathBuf::from(&path_str[4..])
    } else {
        path.clone()
    }
}

fn main() {
    // Open log file in temp directory
    let log_path = std::env::temp_dir().join("dbcenter-watcher.log");
    let mut log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .expect("Failed to open log file");

    log!(
        log_file,
        "=== Watcher baslatildi. PID: {} (v0.1.5) ===",
        std::process::id()
    );

    let stdin = io::stdin();
    let mut handle = stdin.lock();
    let mut buffer = String::new();

    // Read config
    let config: WatcherConfig = match handle.read_line(&mut buffer) {
        Ok(n) if n > 0 => match serde_json::from_str(buffer.trim()) {
            Ok(cfg) => cfg,
            Err(err) => {
                log!(log_file, "Config parse hatasi: {}", err);
                return;
            }
        },
        _ => {
            log!(log_file, "Config alinamadi!");
            return;
        }
    };

    log!(
        log_file,
        "Config: persist={}, service={}, postgres_pid={:?}",
        config.persist,
        config.service,
        config.postgres_pid
    );

    if config.service {
        log!(log_file, "Service mode - watcher sonlaniyor");
        return;
    }

    // Wait for Python to exit (EOF on stdin)
    buffer.clear();
    drop(handle); // Release stdin lock before blocking read

    let stdin2 = io::stdin();
    let mut handle2 = stdin2.lock();
    let python_status = match handle2.read_line(&mut buffer) {
        Ok(0) => "CRASH".to_string(),
        Ok(_) => buffer.trim().to_string(),
        Err(e) => {
            log!(log_file, "Stdin read error: {}", e);
            "ERROR".to_string()
        }
    };
    drop(handle2);

    log!(log_file, "Python durumu: {}", python_status);

    // FORCE_REMOVE means we should kill process and ensure it's dead
    // Previously we returned here, letting Python handle it, but Python didn't wait for shared memory release.
    // Now we stay and use our robust wait logic.
    let is_force_remove = python_status.eq_ignore_ascii_case("FORCE_REMOVE");
    if is_force_remove {
        log!(log_file, "FORCE_REMOVE - temizlik icin devam ediliyor...");
    }

    // Clean path
    let data_dir = clean_path(&config.data_dir);
    log!(log_file, "Data dir: {:?}", data_dir);

    // Get PID from config
    let pid_to_kill: Option<u32> = config.postgres_pid;

    if let Some(pid) = pid_to_kill {
        log!(log_file, "PostgreSQL kapatiliyor (PID: {})", pid);

        #[cfg(windows)]
        {
            log!(log_file, "taskkill calistiriliyor...");
            let result = std::process::Command::new("taskkill")
                .args(["/PID", &pid.to_string(), "/T", "/F"])
                .output();

            match result {
                Ok(output) => {
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    log!(log_file, "taskkill stdout: {}", stdout.trim());
                    log!(log_file, "taskkill stderr: {}", stderr.trim());
                    log!(log_file, "taskkill exit code: {:?}", output.status.code());

                    if wait_for_process_termination(&mut log_file, pid, 10) {
                        log!(log_file, "PostgreSQL kapatildi");
                    } else {
                        log!(log_file, "PostgreSQL hala calisiyor olabilir (timeout)");
                    }
                }
                Err(e) => {
                    log!(log_file, "taskkill hatasi: {}", e);
                }
            }
        }

        #[cfg(unix)]
        {
            log!(log_file, "kill -TERM calistiriliyor...");
            let result = std::process::Command::new("kill")
                .args(["-TERM", &pid.to_string()])
                .output();

            match result {
                Ok(_) => {
                    if wait_for_process_termination(&mut log_file, pid, 10) {
                        log!(log_file, "PostgreSQL kapatildi");
                    } else {
                        log!(log_file, "SIGKILL gonderiliyor");
                        let _ = std::process::Command::new("kill")
                            .args(["-9", &pid.to_string()])
                            .output();
                        wait_for_process_termination(&mut log_file, pid, 5);
                    }
                }
                Err(e) => {
                    log!(log_file, "kill hatasi: {}", e);
                }
            }
        }
    } else {
        log!(log_file, "Kapatilacak PID bulunamadi");
    }

    // Handle persist flag
    // Override: If FORCE_REMOVE, we always delete (unless service mode? No, force remove is explicit)
    let should_delete = !config.persist || is_force_remove;
    
    if should_delete {
        log!(log_file, "persist=false veya FORCE_REMOVE - dosyalar siliniyor");
        if data_dir.exists() {
            match std::fs::remove_dir_all(&data_dir) {
                Ok(_) => log!(log_file, "Dosyalar silindi"),
                Err(e) => log!(log_file, "Silme hatasi: {}", e),
            }
        }
    } else {
        log!(log_file, "persist=true - dosyalar korunuyor");
    }

    log!(log_file, "=== Temizlik tamamlandi ===");
    
    // Handshake signal for Python
    if is_force_remove {
        println!("CLEANUP_DONE");
    }
}
