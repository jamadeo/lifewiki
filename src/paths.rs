use anyhow::{bail, Context, Result};
use std::env;
use std::path::PathBuf;

pub fn home() -> Result<PathBuf> {
    if let Some(path) = env::var_os("LIFEWIKI_HOME") {
        return Ok(PathBuf::from(path));
    }
    let base = env::var_os("HOME").context("HOME is not set; set LIFEWIKI_HOME instead")?;
    Ok(PathBuf::from(base).join(".lifewiki"))
}

pub fn require_initialized(home: &std::path::Path) -> Result<()> {
    if !home.join(".git").exists() || !home.join("log").is_dir() {
        bail!(
            "{} is not a lifewiki repository; run `lw init`",
            home.display()
        );
    }
    Ok(())
}
