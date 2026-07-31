struct MetadataInjector;
struct HeaderExtractor;

impl Injector for MetadataInjector {
    fn set(&mut self, key: &str, value: String) {
        tracing::warn!(key, value, "metadata parse failed");
    }
}

impl Extractor for HeaderExtractor {
    fn get(&self, key: &str) -> Option<&str> {
        let error = "invalid header";
        let v = self.headers.get(key)?;
        tracing::warn!(%error, ?v, "header parse failed");
        None
    }
}

fn spans_raw_locations(request: &Request<()>, req: &reqwest::Request) {
    let url = req.url().clone();
    tracing::info_span!("http_request", "url.full" = %url);
    tracing::info_span!("grpc_request", uri = %request.uri());
}
