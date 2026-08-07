# Negative provider examples for docs validation tests

These fixtures are intentionally invalid or unsafe. Keep each case clearly
marked and keep the failure mode asserted in Rust tests.

<!-- medley-doc-test:toml-negative:legacy-local-missing-auth-scheme -->
```toml
[model.legacy-ollama]
model = "codellama"
base_url = "http://localhost:11434/v1"
name = "Legacy Ollama Example"
context_window = 16384
```

<!-- medley-doc-test:toml-negative:invalid-env-key-warning -->
```toml
[model.bad-env-key]
model = "gpt-4o"
base_url = "https://api.openai.com/v1"
name = "Bad Env Key"
context_window = 200000
env_key = ["OPENAI_API_KEY", "   "]
```

<!-- medley-doc-test:toml-negative:unsafe-literal-secret-and-userinfo -->
```toml
[model.unsafe-creds]
model = "gpt-4o"
base_url = "https://user:pass@example.com/v1"
name = "Unsafe Credentials"
context_window = 200000
api_key = "sk-live-1234567890abcdef1234567890abcdef"
```
