//! 路径解析的唯一入口：规范化最近存在的祖先，再续接尚未创建的叶子。
//!
//! 权限侧与沙箱侧必须对同一个写目标得出同一个物理落点。这里逐分量交给
//! `canonicalize`，因此 `..` 会先经过符号链接再回退，而不是被词法消掉；
//! 断链、权限错误或中间非目录都返回 `None`，让调用方保守处理，不能退到
//! 父目录把符号链接吞掉。

use std::path::Path;
use std::path::PathBuf;

pub(crate) fn resolve_nearest_existing(path: &Path) -> Option<PathBuf> {
    resolve(path, /*dangling_is_failure*/ true)
}

/// 同一次逐分量解析，但把断链叶子当成缺失分量。只供 deny/ask 的别名证据：
/// 它可能把目标归到父目录下，不能据此生成 allow 或记忆授权。
pub(crate) fn resolve_nearest_existing_loose(path: &Path) -> Option<PathBuf> {
    resolve(path, /*dangling_is_failure*/ false)
}

fn resolve(path: &Path, dangling_is_failure: bool) -> Option<PathBuf> {
    let mut ancestor = path;
    let mut missing = Vec::new();
    loop {
        match std::fs::canonicalize(ancestor) {
            Ok(mut resolved) => {
                for component in missing.iter().rev() {
                    resolved.push(component);
                }
                return Some(resolved);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if dangling_is_failure && std::fs::symlink_metadata(ancestor).is_ok() {
                    return None;
                }
                missing.push(ancestor.file_name()?.to_os_string());
                ancestor = ancestor.parent()?;
            }
            Err(_) => return None,
        }
    }
}
