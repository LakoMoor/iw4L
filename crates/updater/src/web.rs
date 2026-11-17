use std::ffi::OsString;
use std::path::{Path, PathBuf};

pub type Result<T> = std::result::Result<T, String>;

#[derive(Clone, Debug)]
pub struct CommunityFile {
    pub path: PathBuf,
    pub name: String,
}

pub fn communities() -> &'static [CommunityFile] {
    &[]
}

pub fn selected_path() -> Option<&'static Path> {
    None
}

pub fn restart_with_community(_path: &Path) -> Result<()> {
    Err("community servers are not available in the browser".into())
}

#[derive(Clone, Debug)]
pub struct Community {
    pub schema: u32,
    pub name: String,
    pub master: Master,
    pub updates: Updates,
}

#[derive(Clone, Debug)]
pub struct Master {
    pub address: String,
    pub server_name: String,
}

#[derive(Clone, Debug)]
pub struct Updates {
    pub url: String,
    pub ca_pem: String,
}

impl Community {
    pub fn parse(_bytes: &[u8]) -> Result<Self> {
        Err("community descriptors are not used in the browser".into())
    }
}

pub fn selected() -> Option<&'static Community> {
    None
}

pub fn startup() -> Result<Option<Vec<OsString>>> {
    Ok(None)
}
