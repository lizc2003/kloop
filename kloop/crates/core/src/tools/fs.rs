use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use serde_json::Value;

use super::str_arg;
use super::ToolCtx;

pub(super) async fn read_file_tool(input: &Value) -> Result<String> {
    let path = str_arg(input, "path", "read_file")?;
    let offset = input["offset"].as_u64().unwrap_or(1).max(1) as usize;
    let limit = input["limit"].as_u64().unwrap_or(2000) as usize;
    let content = tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("read_file: cannot read {path}"))?;
    let out: Vec<String> = content
        .lines()
        .enumerate()
        .skip(offset - 1)
        .take(limit)
        .map(|(i, line)| format!("{}\t{line}", i + 1))
        .collect();
    if out.is_empty() {
        return Ok("(no lines in requested range)".into());
    }
    Ok(out.join("\n"))
}

pub(super) async fn write_file_tool(input: &Value) -> Result<String> {
    let path = str_arg(input, "path", "write_file")?;
    let content = str_arg(input, "content", "write_file")?;
    if let Some(parent) = std::path::Path::new(path).parent() {
        if !parent.as_os_str().is_empty() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("write_file: cannot create {}", parent.display()))?;
        }
    }
    tokio::fs::write(path, content)
        .await
        .with_context(|| format!("write_file: cannot write {path}"))?;
    Ok(format!("wrote {} bytes to {path}", content.len()))
}

pub(super) async fn edit_file_tool(input: &Value) -> Result<String> {
    let path = str_arg(input, "path", "edit_file")?;
    let old = str_arg(input, "old_string", "edit_file")?;
    let new = str_arg(input, "new_string", "edit_file")?;
    let replace_all = input["replace_all"].as_bool().unwrap_or(false);
    if old.is_empty() {
        bail!("edit_file: old_string must not be empty");
    }
    let content = tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("edit_file: cannot read {path}"))?;
    let count = content.matches(old).count();
    if count == 0 {
        bail!("edit_file: old_string not found in {path}");
    }
    if count > 1 && !replace_all {
        bail!("edit_file: old_string matches {count} times in {path}; add surrounding context to disambiguate or set replace_all");
    }
    let updated = if replace_all {
        content.replace(old, new)
    } else {
        content.replacen(old, new, 1)
    };
    tokio::fs::write(path, updated)
        .await
        .with_context(|| format!("edit_file: cannot write {path}"))?;
    let n = if replace_all { count } else { 1 };
    Ok(format!("edited {path} ({n} replacement(s))"))
}

pub(super) async fn read_offloaded_tool(input: &Value, ctx: &ToolCtx) -> Result<String> {
    let id = str_arg(input, "id", "read_offloaded")?;
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        bail!("read_offloaded: invalid id (only [A-Za-z0-9-] allowed)");
    }
    let path = ctx.cfg.offload_dir.join(format!("{id}.txt"));
    tokio::fs::read_to_string(&path)
        .await
        .with_context(|| format!("read_offloaded: no offloaded output with id {id}"))
}

#[cfg(test)]
mod tests {
    use crate::tools::testutil::*;
    use serde_json::json;

    fn temp_file(tag: &str, content: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("kloop-tool-{}-{tag}", std::process::id()));
        std::fs::write(&path, content).unwrap();
        path
    }

    #[tokio::test]
    async fn read_file_numbers_lines_with_offset_and_limit() {
        let path = temp_file("read", "alpha\nbeta\ngamma\ndelta\n");
        let ctx = test_ctx(0, "read");
        let (out, is_error) = run_tool(
            "read_file",
            json!({"path": path.to_str().unwrap(), "offset": 2, "limit": 2}),
            &ctx,
        )
        .await;
        assert!(!is_error);
        assert_eq!(out, "2\tbeta\n3\tgamma");

        let (out, is_error) =
            run_tool("read_file", json!({"path": "/nonexistent/kloop"}), &ctx).await;
        assert!(is_error);
        assert!(out.contains("cannot read"));
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn write_file_creates_parent_directories() {
        let dir = std::env::temp_dir().join(format!("kloop-write-{}", std::process::id()));
        let path = dir.join("deep/nested/file.txt");
        let ctx = test_ctx(0, "write");
        let (out, is_error) = run_tool(
            "write_file",
            json!({"path": path.to_str().unwrap(), "content": "created"}),
            &ctx,
        )
        .await;
        assert!(!is_error, "{out}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "created");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn edit_file_replaces_errors_and_replace_all() {
        let path = temp_file("edit", "one two two three");
        let p = path.to_str().unwrap();
        let ctx = test_ctx(0, "edit");

        // Ambiguous match without replace_all is an error and changes nothing.
        let (out, is_error) = run_tool(
            "edit_file",
            json!({"path": p, "old_string": "two", "new_string": "2"}),
            &ctx,
        )
        .await;
        assert!(is_error);
        assert!(out.contains("2 times"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "one two two three");

        // Missing old_string is an error.
        let (out, is_error) = run_tool(
            "edit_file",
            json!({"path": p, "old_string": "zzz", "new_string": "2"}),
            &ctx,
        )
        .await;
        assert!(is_error);
        assert!(out.contains("not found"));

        // replace_all rewrites every occurrence.
        let (_, is_error) = run_tool(
            "edit_file",
            json!({"path": p, "old_string": "two", "new_string": "2", "replace_all": true}),
            &ctx,
        )
        .await;
        assert!(!is_error);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "one 2 2 three");

        // Unique match replaces exactly once.
        let (_, is_error) = run_tool(
            "edit_file",
            json!({"path": p, "old_string": "one", "new_string": "1"}),
            &ctx,
        )
        .await;
        assert!(!is_error);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "1 2 2 three");
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn read_offloaded_round_trip_and_id_validation() {
        let ctx = test_ctx(0, "offloaded");
        std::fs::create_dir_all(&ctx.cfg.offload_dir).unwrap();
        std::fs::write(ctx.cfg.offload_dir.join("off-7777.txt"), "full payload").unwrap();

        let (out, is_error) = run_tool("read_offloaded", json!({"id": "off-7777"}), &ctx).await;
        assert!(!is_error);
        assert_eq!(out, "full payload");

        // Path traversal shapes are rejected before touching the filesystem.
        let (out, is_error) =
            run_tool("read_offloaded", json!({"id": "../../etc/passwd"}), &ctx).await;
        assert!(is_error);
        assert!(out.contains("invalid id"));
        let _ = std::fs::remove_dir_all(&ctx.cfg.offload_dir);
    }
}
