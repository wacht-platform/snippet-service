use serde_json::json;
use snippet::builtins::ReplaceFileContentTool;
use snippet::tools::{Tool, ToolContext, ToolError};

async fn replace(
    ctx: &ToolContext,
    path: &str,
    start_line: usize,
    end_line: usize,
    target_content: &str,
    replacement_content: &str,
) -> Result<snippet::tools::ToolResult, ToolError> {
    ReplaceFileContentTool
        .execute(
            ctx,
            json!({
                "path": path,
                "start_line": start_line,
                "end_line": end_line,
                "target_content": target_content,
                "replacement_content": replacement_content,
            }),
        )
        .await
}

#[tokio::test]
async fn relocates_unique_target_when_line_range_is_stale() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("file.txt"), "inserted\none\ntwo\nthree\n").unwrap();
    let ctx = ToolContext::new(dir.path()).unwrap();

    replace(&ctx, "file.txt", 2, 2, "two", "updated")
        .await
        .unwrap();

    assert_eq!(
        std::fs::read_to_string(dir.path().join("file.txt")).unwrap(),
        "inserted\none\nupdated\nthree\n"
    );
}

#[tokio::test]
async fn preserves_multiline_replacement_and_trailing_newline() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("file.txt"), "before\nold\nafter\n").unwrap();
    let ctx = ToolContext::new(dir.path()).unwrap();

    replace(&ctx, "file.txt", 2, 2, "old", "new one\nnew two\n")
        .await
        .unwrap();

    assert_eq!(
        std::fs::read_to_string(dir.path().join("file.txt")).unwrap(),
        "before\nnew one\nnew two\nafter\n"
    );
}

#[tokio::test]
async fn preserves_crlf_when_replacing_a_multiline_block() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("file.txt"),
        b"before\r\nold one\r\nold two\r\nafter\r\n",
    )
    .unwrap();
    let ctx = ToolContext::new(dir.path()).unwrap();

    replace(
        &ctx,
        "file.txt",
        2,
        3,
        "old one\nold two",
        "new one\r\nnew two\r\n",
    )
    .await
    .unwrap();

    assert_eq!(
        std::fs::read(dir.path().join("file.txt")).unwrap(),
        b"before\r\nnew one\r\nnew two\r\nafter\r\n"
    );
}

#[tokio::test]
async fn rejects_ambiguous_target_and_whitespace_mismatch() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("file.txt"), "same\nother\nsame\n").unwrap();
    let ctx = ToolContext::new(dir.path()).unwrap();

    let ambiguous = replace(&ctx, "file.txt", 2, 2, "same", "changed")
        .await
        .unwrap_err()
        .to_string();
    assert!(ambiguous.contains("matches 2 places"));

    let whitespace = replace(&ctx, "file.txt", 2, 2, " other ", "changed")
        .await
        .unwrap_err()
        .to_string();
    assert!(whitespace.contains("Target content was not found"));
    assert!(whitespace.contains("Current text near that range"));
}
