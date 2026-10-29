//! 插件自己的落盘目录，由握手时的 `HostEnv.data_dir` 指定。
//! 机器标识与组件文件必须在握手应答前完成初始化，不能回落到临时目录。

use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

static DATA_DIR: OnceLock<PathBuf> = OnceLock::new();

pub fn init(data_dir: &str) -> io::Result<()> {
    let trimmed = data_dir.trim();
    if trimmed.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "宿主未提供插件数据目录"));
    }
    let dir = PathBuf::from(trimmed);
    if !dir.is_absolute() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "插件数据目录必须是绝对路径"));
    }
    if let Some(current) = DATA_DIR.get() {
        return if current == &dir {
            Ok(())
        } else {
            Err(io::Error::new(io::ErrorKind::AlreadyExists, "插件数据目录已初始化为其它路径"))
        };
    }
    std::fs::create_dir_all(&dir)?;
    migrate_legacy(&dir)?;
    DATA_DIR.set(dir).map_err(|_| {
        io::Error::new(io::ErrorKind::AlreadyExists, "插件数据目录已初始化")
    })
}

pub fn data_dir() -> io::Result<PathBuf> {
    DATA_DIR.get().cloned().ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotConnected, "插件数据目录尚未初始化")
    })
}

fn migrate_legacy(dir: &Path) -> io::Result<()> {
    let legacy = std::env::temp_dir().join("agent2api-providers");
    if legacy == dir {
        return Ok(());
    }
    for name in ["qoder-machine-id", "qoder-umid/runtime-info", "qoder-umid/manifest.json"] {
        copy_legacy(&legacy.join(name), &dir.join(name))?;
    }
    Ok(())
}

fn copy_legacy(source: &Path, target: &Path) -> io::Result<()> {
    let metadata = match std::fs::symlink_metadata(source) {
        Ok(metadata) if metadata.is_file() => metadata,
        Ok(_) => return Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    match std::fs::symlink_metadata(target) {
        Ok(_) => return Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let Some(parent) = target.parent() else {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "迁移目标没有父目录"));
    };
    std::fs::create_dir_all(parent)?;
    let mut input = std::fs::File::open(source)?;
    let mut output = match std::fs::OpenOptions::new().write(true).create_new(true).open(target) {
        Ok(output) => output,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => return Ok(()),
        Err(error) => return Err(error),
    };
    let result = (|| {
        std::io::copy(&mut input, &mut output)?;
        output.set_permissions(metadata.permissions())?;
        output.sync_all()
    })();
    drop(output);
    if result.is_err() {
        let _ = std::fs::remove_file(target);
    }
    result
}
