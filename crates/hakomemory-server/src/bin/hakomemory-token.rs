//! Mint token akses memory-box (HMAC-SHA256).
//!
//! Secret dibaca dari MEMORYBOX_AUTH_SECRET (jangan lewat argv).
//! Contoh:
//!   MEMORYBOX_AUTH_SECRET=rahasia hakomemory-token --sub alice \
//!     --projects 830d232f9c86 --roots /home/alice/proj --days 30

use anyhow::{anyhow, Result};
use hakomemory_server::auth::{mint_token, Claims};

fn next(args: &[String], i: &mut usize, flag: &str) -> Result<String> {
    *i += 1;
    args.get(*i)
        .cloned()
        .ok_or_else(|| anyhow!("{flag} butuh nilai"))
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let secret = std::env::var("MEMORYBOX_AUTH_SECRET")
        .map_err(|_| anyhow!("MEMORYBOX_AUTH_SECRET belum diset"))?;
    let mut sub = String::new();
    let mut projects = Vec::new();
    let mut roots = Vec::new();
    let mut days = 30u64;
    let mut admin = false;
    let mut jti: Option<String> = None;
    let mut agents: Vec<String> = Vec::new();
    let mut sessions: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--sub" => sub = next(&args, &mut i, "--sub")?,
            "--projects" => {
                projects = next(&args, &mut i, "--projects")?
                    .split(',')
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect()
            }
            "--roots" => {
                roots = next(&args, &mut i, "--roots")?
                    .split(',')
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect()
            }
            "--days" => days = next(&args, &mut i, "--days")?.parse()?,
            "--admin" => admin = true,
            "--jti" => jti = Some(next(&args, &mut i, "--jti")?),
            "--agents" => {
                agents = next(&args, &mut i, "--agents")?
                    .split(',').filter(|s| !s.is_empty()).map(String::from).collect()
            }
            "--sessions" => {
                sessions = next(&args, &mut i, "--sessions")?
                    .split(',').filter(|s| !s.is_empty()).map(String::from).collect()
            }
            "--help" => {
                println!("{}", include_str!("TOKEN_HELP.txt"));
                return Ok(());
            }
            _ => {}
        }
        i += 1;
    }
    if sub.is_empty() && !admin {
        return Err(anyhow!("--sub wajib (kecuali --admin)"));
    }
    let exp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs()
        + days * 86400;
    let token = mint_token(
        &Claims {
            sub: if sub.is_empty() { "admin".into() } else { sub },
            roots,
            projects,
            exp,
            admin,
            jti: Some(jti.unwrap_or_else(|| {
                let nanos = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0);
                format!("jti-{nanos}-{}", std::process::id())
            })),
            agents,
            sessions,
        },
        secret.as_bytes(),
    )?;
    println!("{token}");
    Ok(())
}
