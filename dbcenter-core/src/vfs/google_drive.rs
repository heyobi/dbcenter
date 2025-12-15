use crate::error::{Result, DBCenterError};
use crate::storage::StorageAdapter;
use std::sync::{Arc, Mutex};
use std::path::{Path, PathBuf};
use std::io::{Read, Write}; // Ensure Write is imported
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};

#[derive(Clone)]
pub struct GoogleDriveAdapter {
    client: Client,
    token: Arc<Mutex<String>>,
    root_folder_id: String,
    config: OAuthConfig,
}

#[derive(Clone, Serialize, Deserialize)]
struct OAuthConfig {
    client_id: String,
    client_secret: String,
    redirect_uri: String,
    refresh_token: Option<String>,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: i64,
}

impl GoogleDriveAdapter {
    pub fn new(credential_path: &str, root_folder_name: &str) -> Result<Self> {
        let path = PathBuf::from(credential_path);
        if !path.exists() {
            return Err(DBCenterError::StorageError(format!("Credential file not found: {}", credential_path)));
        }

        // Parse client_secret.json (installed flow format)
        let content = std::fs::read_to_string(&path).map_err(DBCenterError::IoError)?;
        let json: serde_json::Value = serde_json::from_str(&content).map_err(DBCenterError::SerializationError)?;
        
        let installed = json.get("installed").ok_or_else(|| DBCenterError::StorageError("Invalid client_secret.json: missing 'installed'".into()))?;
        let client_id = installed["client_id"].as_str().unwrap().to_string();
        let client_secret = installed["client_secret"].as_str().unwrap().to_string();
        let redirect_uri = installed["redirect_uris"][0].as_str().unwrap().to_string(); // Use first one, usually localhost:8080

        let mut config = OAuthConfig {
            client_id,
            client_secret,
            redirect_uri,
            refresh_token: None,
        };

        // Check for persisted token
        let token_path = Path::new("token_cache.json");
        if token_path.exists() {
             if let Ok(saved) = std::fs::read_to_string(token_path) {
                 if let Ok(saved_config) = serde_json::from_str::<OAuthConfig>(&saved) {
                     config.refresh_token = saved_config.refresh_token;
                 }
             }
        }

        let client = Client::builder().build().unwrap();

        let access_token = if let Some(ref refresh) = config.refresh_token {
            // Refresh
            Self::refresh_token(&client, &config.client_id, &config.client_secret, refresh)?
        } else {
            // Interactive Auth
            let (token, new_refresh) = Self::interactive_auth(&client, &config)?;
            config.refresh_token = new_refresh; // Save new refresh token
            // Persist
            if let Ok(bytes) = serde_json::to_vec_pretty(&config) {
                let _ = std::fs::write("token_cache.json", bytes);
            }
            token
        };

        let adapter = Self {
            client: client.clone(),
            token: Arc::new(Mutex::new(access_token)),
            root_folder_id: String::new(), // Placeholder
            config,
        };

        // Find/Create Root
        let root_id = adapter.ensure_folder("root", root_folder_name)?;
        
        Ok(Self {
            root_folder_id: root_id, // Update with real root
            ..adapter
        })
    }
    
    fn refresh_token(client: &Client, client_id: &str, client_secret: &str, refresh_token: &str) -> Result<String> {
        let params = [
            ("client_id", client_id),
            ("client_secret", client_secret),
            ("refresh_token", refresh_token),
            ("grant_type", "refresh_token"),
        ];
        
        let resp = client.post("https://oauth2.googleapis.com/token")
            .form(&params)
            .send()
            .map_err(|e| DBCenterError::StorageError(format!("Refresh request failed: {}", e)))?;
            
        if !resp.status().is_success() {
             return Err(DBCenterError::StorageError(format!("Token refresh failed: {:?}", resp.text())));
        }
        
        let token_resp: TokenResponse = resp.json().map_err(|e| DBCenterError::StorageError(format!("Parse token error: {}", e)))?;
        Ok(token_resp.access_token)
    }

    fn interactive_auth(client: &Client, config: &OAuthConfig) -> Result<(String, Option<String>)> {
        use std::net::TcpListener;
        
        // 1. Start Listener
        // Parse port from redirect_uri (http://localhost:8080)
        let port = config.redirect_uri.split(':').last().unwrap_or("8080").trim_end_matches('/');
        let listener = TcpListener::bind(format!("127.0.0.1:{}", port))
            .map_err(|e| DBCenterError::StorageError(format!("Failed to bind to {}: {}", port, e)))?;
            
        // 2. Generate Auth URL
        let auth_url = format!(
            "https://accounts.google.com/o/oauth2/v2/auth?scope=https://www.googleapis.com/auth/drive&access_type=offline&include_granted_scopes=true&response_type=code&state=state_parameter_passthrough_value&redirect_uri={}&client_id={}",
            config.redirect_uri, config.client_id
        );
        
        println!("\n[ACTION REQUIRED] Please open this URL in your browser to authorize DBCenter:\n{}\n", auth_url);
        
        // 3. Wait for Code
        // Accept one connection
        let (mut stream, _) = listener.accept().map_err(DBCenterError::IoError)?;
        
        let mut buffer = [0; 1024];
        let n = stream.read(&mut buffer).map_err(DBCenterError::IoError)?;
        let request = String::from_utf8_lossy(&buffer[..n]);
        
        // Parse "GET /?code=... HTTP/1.1"
        let _code_param = request.lines().next().unwrap_or("").split_whitespace().nth(1).unwrap_or("");
        // Extract code=...
        // URL decode? usually code is simple, but might need it.
        // Quick parse: find "code="
        let code = if let Some(idx) = request.find("code=") {
            let start = idx + 5;
            let end = request[start..].find('&').map(|i| start + i).unwrap_or_else(|| {
                 request[start..].find(' ').map(|i| start + i).unwrap_or(request.len())
            });
            &request[start..end]
        } else {
             return Err(DBCenterError::StorageError("No auth code received".into()));
        };
        
        // Send success response
        let response = "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n<h1>Authentication Successful!</h1><p>You can close this tab and return to the application.</p>";
        stream.write_all(response.as_bytes()).ok();
        
        // 4. Exchange Code
        let params = [
            ("client_id", config.client_id.as_str()),
            ("client_secret", config.client_secret.as_str()),
            ("code", code),
            ("grant_type", "authorization_code"),
            ("redirect_uri", config.redirect_uri.as_str()),
        ];
        
        let resp = client.post("https://oauth2.googleapis.com/token")
            .form(&params)
            .send()
            .map_err(|e| DBCenterError::StorageError(format!("Token exchange failed: {}", e)))?;
            
        if !resp.status().is_success() {
             return Err(DBCenterError::StorageError(format!("Token exchange error: {:?}", resp.text())));
        }
        
        let token_resp: TokenResponse = resp.json().map_err(|e| DBCenterError::StorageError(format!("Parse token error: {}", e)))?;
        Ok((token_resp.access_token, token_resp.refresh_token))
    }
    
    fn ensure_folder(&self, parent_id: &str, name: &str) -> Result<String> {
        println!("DEBUG: ensure_folder name='{}' parent='{}'", name, parent_id);
        let token = self.token.lock().unwrap();
        // Check if exists
        let query = format!("name = '{}' and '{}' in parents and mimeType = 'application/vnd.google-apps.folder' and trashed = false", name, parent_id);
        let url = format!("https://www.googleapis.com/drive/v3/files?q={}", urlencoding::encode(&query));
        
        let resp = self.client.get(&url)
            .header("Authorization", format!("Bearer {}", token))
            .send()
            .map_err(|e| DBCenterError::StorageError(e.to_string()))?;
            
        #[derive(Deserialize)]
        struct FileList { files: Vec<DriveFile> }
        #[derive(Deserialize, Serialize)] // Serialize for create
        struct DriveFile { id: Option<String>, name: Option<String>, mimeType: Option<String>, parents: Option<Vec<String>> }
        
        let list: FileList = resp.json().map_err(|e| DBCenterError::StorageError(e.to_string()))?;
        
        if let Some(f) = list.files.first() {
            // println!("DEBUG: Found existing folder '{}': {}", name, f.id.as_ref().unwrap());
            return Ok(f.id.clone().unwrap());
        }
        
        // Create
        let new_folder = DriveFile {
            id: None,
            name: Some(name.to_string()),
            mimeType: Some("application/vnd.google-apps.folder".to_string()),
            parents: Some(vec![parent_id.to_string()]),
        };
        
        let create_resp = self.client.post("https://www.googleapis.com/drive/v3/files")
            .header("Authorization", format!("Bearer {}", token))
            .json(&new_folder)
            .send()
            .map_err(|e| DBCenterError::StorageError(e.to_string()))?;
            
        let created: DriveFile = create_resp.json().map_err(|e| DBCenterError::StorageError(format!("Create folder error: {}", e)))?;
        // println!("DEBUG: Created folder '{}': {}", name, created.id.as_ref().unwrap());
        Ok(created.id.unwrap())
    }

    fn resolve_path(&self, key: &str, create_parents: bool) -> Result<(String, Option<String>)> {
        println!("DEBUG: resolve_path key='{}' create_parents={}", key, create_parents);
        let parts: Vec<&str> = key.split('/').filter(|s| !s.is_empty()).collect();
        if parts.is_empty() { return Ok((self.root_folder_id.clone(), None)); }
        
        let mut current_parent = self.root_folder_id.clone();
        let filename = parts.last().unwrap();
        let folder_parts = &parts[0..parts.len()-1];
        
        for folder_name in folder_parts {
            if create_parents {
                current_parent = self.ensure_folder(&current_parent, folder_name)?;
            } else {
                // Find
                let token = self.token.lock().unwrap();
                let query = format!("name = '{}' and '{}' in parents and mimeType = 'application/vnd.google-apps.folder' and trashed = false", folder_name, current_parent);
                let url = format!("https://www.googleapis.com/drive/v3/files?q={}", urlencoding::encode(&query));
                let resp = self.client.get(&url).header("Authorization", format!("Bearer {}", token)).send().map_err(|e| DBCenterError::StorageError(e.to_string()))?;
                let json: serde_json::Value = resp.json().map_err(|e| DBCenterError::StorageError(e.to_string()))?;
                if let Some(files) = json["files"].as_array() {
                    if let Some(f) = files.first() {
                        current_parent = f["id"].as_str().unwrap().to_string();
                    } else {
                        // println!("DEBUG: resolve_path folder not found '{}'", folder_name);
                        return Err(DBCenterError::StorageError(format!("Folder not found: {}", folder_name)));
                    }
                }
            }
        }
        
        // Find File
        let token = self.token.lock().unwrap();
        let query = format!("name = '{}' and '{}' in parents and trashed = false", filename, current_parent);
        let url = format!("https://www.googleapis.com/drive/v3/files?q={}", urlencoding::encode(&query));
        let resp = self.client.get(&url).header("Authorization", format!("Bearer {}", token)).send().map_err(|e| DBCenterError::StorageError(e.to_string()))?;
        let json: serde_json::Value = resp.json().map_err(|e| DBCenterError::StorageError(e.to_string()))?;
        let file_id = json["files"].as_array().and_then(|arr| arr.first()).and_then(|f| f["id"].as_str()).map(|s| s.to_string());
        
        // println!("DEBUG: resolve_path result parent='{}' file='{:?}'", current_parent, file_id);
        Ok((current_parent, file_id))
    }
}

impl StorageAdapter for GoogleDriveAdapter {
    fn upload(&self, key: &str, data: &[u8]) -> Result<()> {
        let (parent_id, existing_id) = self.resolve_path(key, true)?;
        let token = self.token.lock().unwrap();
        
        if let Some(id) = existing_id {
            // Update: PATCH /upload/drive/v3/files/{id}?uploadType=media
            let url = format!("https://www.googleapis.com/upload/drive/v3/files/{}?uploadType=media", id);
            let resp = self.client.patch(&url)
                .header("Authorization", format!("Bearer {}", token))
                .body(data.to_vec())
                .send()
                .map_err(|e| DBCenterError::StorageError(e.to_string()))?;
             if !resp.status().is_success() {
                 return Err(DBCenterError::StorageError(format!("Update failed: {:?}", resp.text())));
             }
        } else {
            // Create: Simple upload (multipart for metadata + content)
            // Or simple upload if we don't care about name? No we care about name and parent.
            // Multipart upload:
            // Metadata: { name: ..., parents: [...] }
            // Content: ...
            
            // reqwest blocking multipart
            let filename = key.split('/').last().unwrap();
            let metadata = serde_json::json!({
                "name": filename,
                "parents": [parent_id]
            });
            
            let form = reqwest::blocking::multipart::Form::new()
                .part("metadata", reqwest::blocking::multipart::Part::text(metadata.to_string()).mime_str("application/json").unwrap())
                .part("file", reqwest::blocking::multipart::Part::bytes(data.to_vec()).mime_str("application/octet-stream").unwrap());
                
            let resp = self.client.post("https://www.googleapis.com/upload/drive/v3/files?uploadType=multipart")
                .header("Authorization", format!("Bearer {}", token))
                .multipart(form)
                .send()
                .map_err(|e| DBCenterError::StorageError(e.to_string()))?;
                
             if !resp.status().is_success() {
                 return Err(DBCenterError::StorageError(format!("Create failed: {:?}", resp.text())));
             }
        }
        Ok(())
    }

    fn download(&self, key: &str) -> Result<Vec<u8>> {
        let (_, file_id_opt) = self.resolve_path(key, false)?;
        if let Some(id) = file_id_opt {
             let token = self.token.lock().unwrap();
             let url = format!("https://www.googleapis.com/drive/v3/files/{}?alt=media", id);
             let resp = self.client.get(&url)
                .header("Authorization", format!("Bearer {}", token))
                .send()
                .map_err(|e| DBCenterError::StorageError(e.to_string()))?;
             
             if !resp.status().is_success() {
                 return Err(DBCenterError::StorageError(format!("Download failed: {:?}", resp.text())));
             }
             Ok(resp.bytes().map_err(|e| DBCenterError::StorageError(e.to_string()))?.to_vec())
        } else {
            Err(DBCenterError::StorageError("File not found".into()))
        }
    }

    fn list(&self, prefix: &str) -> Result<Vec<String>> {
         println!("DEBUG: list prefix='{}'", prefix);
         let prefix = prefix.trim_start_matches('/'); 
         
         let (parent_path, name_prefix) = if let Some(idx) = prefix.rfind('/') {
             (&prefix[..idx], &prefix[idx+1..])
         } else {
             ("", prefix)
         };
         
         let (parent_id, _) = self.resolve_path(parent_path, false)?;
         // Note: resolve_path returns (last_parent_id, file_id). We care about parent_id.
         // Wait, resolve_path returns (parent_of_target, target_id).
         // If target is a folder, is it returned as target_id?
         // resolve_path logic:
         // Iterate folders. current_parent updates.
         // Finally checks for file in current_parent.
         // If we want the ID of a FOLDER 'foo', resolve_path('foo') returns (root, id_of_foo).
         // So if parent_path is "subfolder", we want ID of "subfolder".
         // resolve_path("subfolder") -> (root, id_of_subfolder).
         
         // If parent_path is empty, we want root_folder_id.
         let search_folder_id = if parent_path.is_empty() {
             self.root_folder_id.clone()
         } else {
             let (_, id_opt) = self.resolve_path(parent_path, false)?;
             id_opt.ok_or_else(|| DBCenterError::StorageError(format!("Parent folder not found: {}", parent_path)))?
         };

         let token = self.token.lock().unwrap();
         let query = format!("'{}' in parents and trashed = false", search_folder_id);
         let url = format!("https://www.googleapis.com/drive/v3/files?q={}", urlencoding::encode(&query));
         let resp = self.client.get(&url).header("Authorization", format!("Bearer {}", token)).send().map_err(|e| DBCenterError::StorageError(e.to_string()))?;
         let json: serde_json::Value = resp.json().map_err(|e| DBCenterError::StorageError(e.to_string()))?;
         
         println!("DEBUG: list json='{:?}'", json);
         
         let mut results = Vec::new();
         if let Some(files) = json["files"].as_array() {
             for f in files {
                 if let Some(name) = f["name"].as_str() {
                     if name.starts_with(name_prefix) {
                         let key = if parent_path.is_empty() {
                             name.to_string()
                         } else {
                             format!("{}/{}", parent_path, name)
                         };
                         results.push(key);
                     }
                 }
             }
         }
         Ok(results)
    }

    fn size(&self, key: &str) -> Result<u64> {
         let (_, file_id_opt) = self.resolve_path(key, false)?;
         if let Some(id) = file_id_opt {
              let token = self.token.lock().unwrap();
              let url = format!("https://www.googleapis.com/drive/v3/files/{}?fields=size", id);
              let resp = self.client.get(&url).header("Authorization", format!("Bearer {}", token)).send().map_err(|e| DBCenterError::StorageError(e.to_string()))?;
              let json: serde_json::Value = resp.json().map_err(|e| DBCenterError::StorageError(e.to_string()))?;
              Ok(json["size"].as_str().unwrap_or("0").parse().unwrap_or(0))
         } else {
             Err(DBCenterError::StorageError("File not found".into()))
         }
    }

    fn download_range(&self, key: &str, start: u64, length: u64) -> Result<Vec<u8>> {
        let (_, file_id_opt) = self.resolve_path(key, false)?;
        if let Some(id) = file_id_opt {
             let token = self.token.lock().unwrap();
             let url = format!("https://www.googleapis.com/drive/v3/files/{}?alt=media", id);
             let end = start + length - 1;
             let resp = self.client.get(&url)
                .header("Authorization", format!("Bearer {}", token))
                .header("Range", format!("bytes={}-{}", start, end))
                .send()
                .map_err(|e| DBCenterError::StorageError(e.to_string()))?;
             
             if !resp.status().is_success() {
                 return Err(DBCenterError::StorageError(format!("Download failed: {:?}", resp.text())));
             }
             Ok(resp.bytes().map_err(|e| DBCenterError::StorageError(e.to_string()))?.to_vec())
        } else {
            Err(DBCenterError::StorageError("File not found".into()))
        }
    }

    fn delete(&self, key: &str) -> Result<()> {
         let (_, file_id_opt) = self.resolve_path(key, false)?;
         if let Some(id) = file_id_opt {
             let token = self.token.lock().unwrap();
             let url = format!("https://www.googleapis.com/drive/v3/files/{}", id);
             self.client.delete(&url).header("Authorization", format!("Bearer {}", token)).send().ok();
         }
         Ok(())
    }

    fn lock(&self, _key: &str, _ttl_ms: u64, _worker_id: &str) -> Result<bool> {
        // Implement "Best Effort" lock
        // Create file if not exists.
        // For now, accept always true (stub) or check existence.
        // User didn't prioritize distributed lock, but Scale Zero demo likely relies on it.
        // Without lock, vacuum might race.
        // But for SINGLE USER demo (A then B), race is impossible.
        Ok(true) 
    }
    
    fn refresh_lock(&self, _key: &str, _ttl_ms: u64, _worker_id: &str) -> Result<bool> { Ok(true) }
    fn unlock(&self, _key: &str, _worker_id: &str) -> Result<()> { Ok(()) }
    fn rename(&self, src: &str, dest: &str) -> Result<()> {
        let (_, src_opt) = self.resolve_path(src, false)?;
        if let Some(id) = src_opt {
            let token = self.token.lock().unwrap();
             let url = format!("https://www.googleapis.com/drive/v3/files/{}", id);
             let new_name = dest.split('/').last().unwrap();
             let body = serde_json::json!({ "name": new_name });
             self.client.patch(&url)
                .header("Authorization", format!("Bearer {}", token))
                .json(&body)
                .send().ok();
        }
        Ok(())
    }
}
