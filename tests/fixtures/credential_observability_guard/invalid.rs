fn invalid(auth: &str) {
    tracing::warn!(
        request = ?build_request(auth),
        auth_prefix = &auth[..8],
        "provider request failed"
    );
    let _deployment = deployment_id_from_key(auth);
}
