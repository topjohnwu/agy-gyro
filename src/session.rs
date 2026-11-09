// SPDX-License-Identifier: MIT

use serde::{Deserialize, Serialize};

/// Metadata identifying an individual `agy` instance connected to the gateway.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: String,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
}

/// Generates a random RFC 4122 version 4 UUID string (`xxxxxxxx-xxxx-4xxx-yxxx-xxxxxxxxxxxx`).
pub fn generate_session_uuid() -> String {
    let mut bytes: [u8; 16] = rand::random();
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0],
        bytes[1],
        bytes[2],
        bytes[3],
        bytes[4],
        bytes[5],
        bytes[6],
        bytes[7],
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15]
    )
}

/// Returns the short 8-character prefix of a session ID (everything before the first `-`)
/// for compact display in logs and TUI tabs.
pub fn short_session_id(id: &str) -> &str {
    id.split('-').next().unwrap_or(id)
}

impl SessionMeta {
    /// Returns the short display prefix of this session's ID.
    pub fn short_id(&self) -> &str {
        short_session_id(&self.id)
    }

    /// Formats a bracketed log prefix (`"[#<short_id> <label>] "`) for this session.
    pub fn log_prefix(&self) -> String {
        format!("[#{} {}] ", self.short_id(), self.label)
    }
}

/// Sanitizes a session label (such as a workspace directory name) for header encoding and display.
pub fn sanitize_session_label(raw: &str) -> String {
    let cleaned: String = raw
        .trim()
        .chars()
        .map(|c| if c == ':' || c.is_control() { '-' } else { c })
        .take(40)
        .collect();
    if cleaned.is_empty() {
        "agy".to_string()
    } else {
        cleaned
    }
}

/// Encodes session metadata into the `AGY_LLM_GATEWAY_API_KEY` token format (`gyro:<id>:<pid>:<label>`).
pub fn format_session_api_key(session_id: &str, pid: Option<u32>, label: &str) -> String {
    format!(
        "gyro:{}:{}:{}",
        session_id.trim(),
        pid.unwrap_or(0),
        sanitize_session_label(label)
    )
}

/// Parses a `gyro:<id>:<pid>:<label>` token from an `Authorization` or `x-goog-api-key` value.
pub fn parse_session_api_key(raw_token: &str) -> Option<SessionMeta> {
    let trimmed = raw_token.trim();
    let token = trimmed
        .strip_prefix("Bearer ")
        .or_else(|| trimmed.strip_prefix("bearer "))
        .map(str::trim)
        .unwrap_or(trimmed);

    let rest = token.strip_prefix("gyro:")?;
    let mut parts = rest.splitn(3, ':');
    let id = parts.next()?.trim();
    if id.is_empty() {
        return None;
    }
    let pid_str = parts.next().unwrap_or("0").trim();
    let pid = pid_str.parse::<u32>().ok().filter(|&p| p > 0);
    let label = sanitize_session_label(parts.next().unwrap_or("agy"));

    Some(SessionMeta {
        id: id.to_string(),
        label,
        pid,
    })
}

/// Extracts session metadata from request headers (`Authorization: Bearer gyro:...` or `x-goog-api-key`).
pub fn extract_session_from_headers(headers: &axum::http::HeaderMap) -> Option<SessionMeta> {
    if let Some(auth) = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        && let Some(meta) = parse_session_api_key(auth)
    {
        return Some(meta);
    }
    if let Some(key) = headers.get("x-goog-api-key").and_then(|v| v.to_str().ok())
        && let Some(meta) = parse_session_api_key(key)
    {
        return Some(meta);
    }
    None
}

/// Returns `true` if the OS process with the given `pid` is currently alive.
pub fn is_pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(unix)]
    {
        let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
        if rc == 0 {
            return true;
        }
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(windows)]
    {
        const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
        const STILL_ACTIVE: u32 = 259;
        unsafe extern "system" {
            fn OpenProcess(
                dwDesiredAccess: u32,
                bInheritHandle: i32,
                dwProcessId: u32,
            ) -> *mut std::ffi::c_void;
            fn GetExitCodeProcess(hProcess: *mut std::ffi::c_void, lpExitCode: *mut u32) -> i32;
            fn CloseHandle(hObject: *mut std::ffi::c_void) -> i32;
        }
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle.is_null() {
                return false;
            }
            let mut exit_code: u32 = 0;
            let ok = GetExitCodeProcess(handle, &mut exit_code);
            CloseHandle(handle);
            ok != 0 && exit_code == STILL_ACTIVE
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        true
    }
}
