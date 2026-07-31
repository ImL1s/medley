fn unsafe_url_derived_key(server_name: &str, server_url: &str) {
    let key = format!("{}:{}", server_name, server_url);
    tracing::info!(key = %key, "mcp stale save skipped");
}
