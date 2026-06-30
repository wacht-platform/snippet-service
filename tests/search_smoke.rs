use serde_json::json;
use snippet::builtins::SearchContentTool;
use snippet::tools::{Tool, ToolContext};

// search_content treats the query as a regex (with a literal fallback), so the
// grep-style patterns models write (`a|b`, `.*`) actually match.
#[tokio::test]
async fn search_content_regex_and_literal() {
    let dir = std::env::temp_dir().join(format!("snip_search_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.go"), "type Deployment struct {}\nfunc SignIn() {}\n").unwrap();
    let ctx = ToolContext::new(dir.clone()).unwrap();
    let n = |r: snippet::tools::ToolResult| r.value["data"]["count"].as_u64().unwrap();

    // alternation regex matches one of the branches
    assert_eq!(n(SearchContentTool.execute(&ctx, json!({"query": "SignIn|SignUp"})).await.unwrap()), 1);
    // plain text (a valid regex) still matches as substring, case-insensitive
    assert_eq!(n(SearchContentTool.execute(&ctx, json!({"query": "type deployment"})).await.unwrap()), 1);
    // .* spans within a line
    assert_eq!(n(SearchContentTool.execute(&ctx, json!({"query": "func.*SignIn"})).await.unwrap()), 1);

    std::fs::remove_dir_all(&dir).ok();
}
