//! Autentikasi + otorisasi untuk deployment multi-user.
//!
//! Token: `mb1.<base64url(payload)>.<hex hmac-sha256>` dengan payload:
//!   { sub, roots: [path], projects: [pid], exp: <unix>, admin: bool }
//!
//! - `roots`   : path canonical yang boleh dibuka user (dipakai projects/open)
//! - `projects`: pid yang boleh diakses user lewat endpoint lain
//! - `admin`   : bypass (bootstrap/operasional)
//!
//! Tanpa `MEMORYBOX_AUTH_SECRET`, mode auth DIMATIKAN (perilaku lama tetap
//! bekerja). Ini penting agar upgrade tidak memutus pengguna existing.

use anyhow::{anyhow, Result};
use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderMap, StatusCode};
use base64::Engine;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

type HmacSha256 = Hmac<Sha256>;
const PREFIX: &str = "mb1";
pub const SECRET_ENV: &str = "MEMORYBOX_AUTH_SECRET";
pub const TOKEN_ENV: &str = "MEMORYBOX_TOKEN";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Claims {
    pub sub: String,
    #[serde(default)]
    pub roots: Vec<String>,
    #[serde(default)]
    pub projects: Vec<String>,
    #[serde(default)]
    pub exp: u64,
    #[serde(default)]
    pub admin: bool,
    /// id token untuk revokasi; bila kosong dipakai fingerprint SHA-256 token.
    #[serde(default)]
    pub jti: Option<String>,
    /// agent/session yang boleh diakses token ini (kosong = tidak dibatasi)
    #[serde(default)]
    pub agents: Vec<String>,
    #[serde(default)]
    pub sessions: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct AuthContext {
    pub sub: String,
    pub roots: Vec<PathBuf>,
    pub projects: Vec<String>,
    pub admin: bool,
    /// kunci untuk pengecekan revokasi (jti atau fingerprint token).
    pub key: String,
    pub agents: Vec<String>,
    pub sessions: Vec<String>,
}

impl AuthContext {
    /// Konteks untuk mode auth-dimati: akses penuh (kompatibel dengan perilaku lama).
    pub fn open() -> Self {
        Self {
            sub: "local".into(),
            roots: Vec::new(),
            projects: Vec::new(),
            admin: true,
            key: "local".into(),
            agents: Vec::new(),
            sessions: Vec::new(),
        }
    }

    pub fn require_project(&self, pid: &str) -> Result<(), StatusCode> {
        if self.admin || self.projects.iter().any(|p| p == pid) {
            Ok(())
        } else {
            Err(StatusCode::FORBIDDEN)
        }
    }

    /// Path harus berada di bawah salah satu root yang diizinkan.
    /// Daftar agent yang diizinkan; None = tanpa pembatasan.
    pub fn allowed_agents(&self) -> Option<Vec<String>> {
        if self.admin || self.agents.is_empty() {
            None
        } else {
            Some(self.agents.clone())
        }
    }

    pub fn allowed_sessions(&self) -> Option<Vec<String>> {
        if self.admin || self.sessions.is_empty() {
            None
        } else {
            Some(self.sessions.clone())
        }
    }

    pub fn require_agent(&self, agent: &str) -> Result<(), StatusCode> {
        match self.allowed_agents() {
            None => Ok(()),
            Some(list) if list.iter().any(|a| a == agent) => Ok(()),
            Some(_) => Err(StatusCode::FORBIDDEN),
        }
    }

    pub fn require_session(&self, session: &str) -> Result<(), StatusCode> {
        match self.allowed_sessions() {
            None => Ok(()),
            Some(list) if list.iter().any(|s| s == session) => Ok(()),
            Some(_) => Err(StatusCode::FORBIDDEN),
        }
    }

    pub fn require_path(&self, path: &str) -> Result<(), StatusCode> {
        if self.admin {
            return Ok(());
        }
        // Prefer real paths: Windows canonicalize returns 8.3 short names,
        // while token roots are usually the long form; comparing only lexical
        // components rejects legitimate paths (and accepts symlink escapes).
        if let Ok(canonical) = std::fs::canonicalize(path) {
            let within_root = self.roots.iter().any(|root| {
                std::fs::canonicalize(root)
                    .map(|root| canonical.starts_with(root))
                    .unwrap_or(false)
            });
            return if within_root { Ok(()) } else { Err(StatusCode::FORBIDDEN) };
        }
        // Path does not exist yet (e.g. a future write): fall back to lexical.
        let target = normalize(path);
        if self.roots.iter().any(|r| target.starts_with(normalize(&r.to_string_lossy()))) {
            Ok(())
        } else {
            Err(StatusCode::FORBIDDEN)
        }
    }
}

/// Normalisasi leksikal (tanpa menyentuh disk): buang `.`, tau `..`.
fn normalize(s: &str) -> PathBuf {
    let mut out = PathBuf::new();
    for c in Path::new(s).components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[derive(Clone)]
pub enum AuthConfig {
    Disabled,
    Enabled { secret: Vec<u8> },
}

impl AuthConfig {
    pub fn from_env() -> Self {
        match std::env::var(SECRET_ENV) {
            Ok(s) if !s.is_empty() => Self::Enabled { secret: s.into_bytes() },
            _ => Self::Disabled,
        }
    }

    pub fn enabled(&self) -> bool {
        matches!(self, Self::Enabled { .. })
    }

    /// Verifikasi header `Authorization: Bearer <token>`.
    pub fn authenticate(
        &self,
        headers: &HeaderMap,
        revoked: &std::collections::HashSet<String>,
    ) -> Result<AuthContext, StatusCode> {
        let secret = match self {
            Self::Disabled => return Ok(AuthContext::open()),
            Self::Enabled { secret } => secret.as_slice(),
        };
        let raw = headers
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .ok_or(StatusCode::UNAUTHORIZED)?;
        let claims = verify_token(raw, secret).map_err(|_| StatusCode::UNAUTHORIZED)?;
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
        if claims.exp != 0 && claims.exp < now {
            return Err(StatusCode::UNAUTHORIZED);
        }
        let key = claims
            .jti
            .clone()
            .unwrap_or_else(|| fingerprint(raw));
        if revoked.contains(&key) {
            return Err(StatusCode::UNAUTHORIZED);
        }
        Ok(AuthContext {
            sub: claims.sub,
            roots: claims.roots.into_iter().map(PathBuf::from).collect(),
            projects: claims.projects,
            admin: claims.admin,
            key,
            agents: claims.agents,
            sessions: claims.sessions,
        })
    }
}

/// Buat token bertanda tangan (dipakai biner `hakomemory-token` dan test).
pub fn mint_token(claims: &Claims, secret: &[u8]) -> Result<String> {
    let payload = serde_json::to_vec(claims)?;
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload);
    let signing = format!("{PREFIX}.{b64}");
    let mut mac = HmacSha256::new_from_slice(secret).map_err(|e| anyhow!("hmac: {e}"))?;
    mac.update(signing.as_bytes());
    let sig = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
    Ok(format!("{signing}.{sig}"))
}

pub fn verify_token(token: &str, secret: &[u8]) -> Result<Claims> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 || parts[0] != PREFIX {
        return Err(anyhow!("format token salah"));
    }
    let mut mac = HmacSha256::new_from_slice(secret).map_err(|e| anyhow!("hmac: {e}"))?;
    mac.update(format!("{}.{}", parts[0], parts[1]).as_bytes());
    let got = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(parts[2])
        .map_err(|_| anyhow!("tanda tangan bukan base64"))?;
    mac.verify_slice(&got).map_err(|_| anyhow!("tanda tangan tidak cocok"))?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(parts[1])
        .map_err(|_| anyhow!("payload bukan base64"))?;
    Ok(serde_json::from_slice(&bytes)?)
}

/// Fingerprint deterministik untuk token tanpa `jti`.
pub fn fingerprint(token: &str) -> String {
    let d = Sha256::digest(token.as_bytes());
    d.iter().map(|b| format!("{b:02x}")).collect()
}

/// Kunci revokasi dari token yang sudah divalidasi.
pub fn token_key(config: &AuthConfig, token: &str) -> Result<String, StatusCode> {
    let secret = match config {
        AuthConfig::Disabled => return Err(StatusCode::BAD_REQUEST),
        AuthConfig::Enabled { secret } => secret.as_slice(),
    };
    let claims = verify_token(token, secret).map_err(|_| StatusCode::UNAUTHORIZED)?;
    Ok(claims.jti.unwrap_or_else(|| fingerprint(token)))
}
