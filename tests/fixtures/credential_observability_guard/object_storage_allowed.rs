struct ZdrVideoOutputS3Config {
    key_prefix: String,
}

fn object_key(config: &ZdrVideoOutputS3Config, name: &str) -> String {
    format!("{}{name}", config.key_prefix)
}
