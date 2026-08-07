pub mod find_protoc;

use anyhow::Context;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::{fs, iter};

/// Find the protoc well-known types include directory.
///
/// When PROTOC is set (e.g., in Bazel), the include directory is typically
/// at `../include` relative to the `bin/protoc` binary. For example:
/// - PROTOC = `/path/to/external/protoc_linux_x86_64/bin/protoc`
/// - Include = `/path/to/external/protoc_linux_x86_64/include`
///
/// This is needed because Bazel places the protoc binary and include files
/// in separate locations within the sandbox, and protoc doesn't automatically
/// find them without an explicit -I flag.
fn find_protoc_include_dir(protoc: Option<&Path>) -> Option<PathBuf> {
    let protoc = protoc?;

    // protoc is typically at .../bin/protoc, so include is at .../include
    let parent = protoc.parent()?; // .../bin
    let grandparent = parent.parent()?; // .../
    let include_dir = grandparent.join("include");

    // Everything downstream needs this decoded — the `-I` flag is built with
    // `format!`, and protoc's dependency output is read back as UTF-8 — so an
    // undecodable include directory fails the build rather than being used.
    //
    // Discovering it at all is new. This is reached from a `protoc` resolved
    // off `PATH`, which used to be the bare name: `parent()` of that is `""`
    // and `parent()` of `""` is `None`, so the walk stopped here and no
    // include directory was ever derived. Resolving the real path is what
    // exposed the sibling `include`, so declining an undecodable one restores
    // exactly what those builds had before, rather than trading a slow build
    // for a broken one. Handling such a path end to end is worth doing, but it
    // is a different change than this one: tracked in #88.
    //
    // Ahead of `is_dir` only because it answers without a syscall; the two
    // gates are independent and either order gives the same result.

    include_dir.is_dir().then_some(include_dir)
}

/// The `cargo:rerun-if-changed` value for a located `protoc`, or `None` when
/// there is nothing safe to emit.
///
/// A perfectly usable `protoc` can still yield nothing here, in two ways:
///
/// * **The path does not resolve from the package root.** Cargo resolves
///   `rerun-if-changed` relative to the package root and calls a missing entry
///   permanently dirty — it says so outright: "Dirty <crate>: the file
///   `protoc` is missing". Emitting an unresolvable path does the opposite of
///   what the directive is for: instead of rebuilding when protoc changes, it
///   rebuilds always, and takes every crate downstream with it.
/// * **The path is not UTF-8.** `rerun-if-changed` has no encoding for those
///   bytes, but `Command` and `PATH` lookup take `OsStr`, so such a protoc
///   runs fine. Failing the build over it would break a working configuration
///   to protect a rebuild trigger.
///
/// Either way the protoc itself is still used — only this one line is dropped.
/// The cost is that edits to the protoc binary no longer force a rebuild; the
/// `.proto` dependency lines emitted alongside it keep the build script
/// tracked regardless.
///
/// The encoding gate comes first so it is reachable without a filesystem that
/// permits such a name — APFS and HFS+ reject non-UTF-8 filenames outright, so
/// checking existence first would make that branch untestable on macOS.
fn rerun_if_changed_for_protoc(protoc: &Path) -> Option<&str> {
    let path = protoc.to_str()?;
    protoc.try_exists().unwrap_or(false).then_some(path)
}

pub struct XaiProtoBuilder {
    builder: tonic_prost_build::Builder,
    file_descriptor_set_path: Option<PathBuf>,
    gen_pbjson: bool,
    pbjson_ignore_unknown_fields: bool,
    pbjson_preserve_proto_field_names: bool,
}

impl XaiProtoBuilder {
    fn map_builder(
        self,
        f: impl FnOnce(tonic_prost_build::Builder) -> tonic_prost_build::Builder,
    ) -> Self {
        Self {
            builder: f(self.builder),
            ..self
        }
    }

    pub fn btree_map<S: AsRef<str>>(self, paths: impl IntoIterator<Item = S>) -> Self {
        self.map_builder(|b| paths.into_iter().fold(b, |b, path| b.btree_map(path)))
    }

    pub fn bytes<S: AsRef<str>>(self, paths: impl IntoIterator<Item = S>) -> Self {
        self.map_builder(|b| paths.into_iter().fold(b, |b, path| b.bytes(path)))
    }

    pub fn extern_path(self, proto_path: impl AsRef<str>, rust_path: impl AsRef<str>) -> Self {
        self.map_builder(|b| b.extern_path(proto_path, rust_path))
    }

    pub fn file_descriptor_set_path(mut self, path: impl AsRef<Path>) -> Self {
        self.file_descriptor_set_path = Some(path.as_ref().to_path_buf());
        self.map_builder(|b| b.file_descriptor_set_path(path))
    }

    pub fn gen_pbjson(mut self) -> Self {
        self.gen_pbjson = true;
        self
    }

    pub fn pbjson_ignore_unknown_fields(mut self) -> Self {
        self.pbjson_ignore_unknown_fields = true;
        self
    }

    /// Serialize JSON using the original proto field names (snake_case) instead
    /// of the proto3-JSON default (camelCase). Deserialization still accepts
    /// both casings, so this is backward-compatible with already-stored
    /// camelCase documents.
    pub fn pbjson_preserve_proto_field_names(mut self) -> Self {
        self.pbjson_preserve_proto_field_names = true;
        self
    }

    pub fn generate_default_stubs(self, enable: bool) -> Self {
        self.map_builder(|b| b.generate_default_stubs(enable))
    }

    pub fn type_attribute(self, path: impl AsRef<str>, attr: impl AsRef<str>) -> Self {
        self.map_builder(|b| b.type_attribute(path, attr))
    }

    pub fn field_attribute(self, path: impl AsRef<str>, attr: impl AsRef<str>) -> Self {
        self.map_builder(|b| b.field_attribute(path, attr))
    }

    // tonic-build generation of `rerun-if-changed` is lazy and incorrect.
    // - everything is invalidated when anything inside include directories is changed
    // - also they compute paths incorrectly: assuming paths are relative to current directory
    //   rather than
    fn emit_rerun_if_changed<'a>(
        protoc: Option<&Path>,
        protoc_include_dir: Option<&Path>,
        protos: impl IntoIterator<Item = &'a Path>,
        includes: impl IntoIterator<Item = &'a Path>,
    ) -> anyhow::Result<()> {
        let includes = Vec::from_iter(includes);

        if let Some(path) = protoc.and_then(rerun_if_changed_for_protoc) {
            println!("cargo:rerun-if-changed={path}");
        }

        // Can only process one input file when using --dependency_out=FILE.
        for proto in protos {
            let mut command = Command::new(protoc.unwrap_or(Path::new("protoc")));
            command
                .arg("--dependency_out=/dev/stdout")
                .arg("--descriptor_set_out=/dev/null");

            // Add protoc's well-known types include directory first (if found).
            // This is needed for Bazel sandboxed builds where protoc and its
            // include files are in different locations.
            if let Some(include_dir) = protoc_include_dir {
                let mut arg = std::ffi::OsString::from("-I");
                arg.push(include_dir.as_os_str());
                command.arg(arg);
            }

            for include in &includes {
                let mut arg = std::ffi::OsString::from("-I");
                arg.push(include.as_os_str());
                command.arg(arg);
            }

            command.arg(proto);

            command.stdin(Stdio::null());
            command.stderr(Stdio::inherit());

            let output = command.output().context("protoc command failed")?;
            if !output.status.success() {
                return Err(anyhow::anyhow!("protoc command failed"));
            }

            let mut lines = output.stdout.split(|&b| b == b'\n');
            let first_line = lines.next().context("protoc command output is empty")?;
            let prefix = b"/dev/null:";
            let rem = if first_line.starts_with(prefix) {
                &first_line[prefix.len()..]
            } else {
                return Err(anyhow::anyhow!(
                    "protoc command output must start with /dev/null: {:?}",
                    String::from_utf8_lossy(first_line)
                ));
            };
            for line in iter::once(rem).chain(lines) {
                let mut line = trim_ascii(line);
                if line.is_empty() {
                    continue;
                }
                if line.ends_with(b"\\") {
                    line = &line[..line.len() - 1];
                }
                let line = trim_ascii(line);
                if line.is_empty() {
                    continue;
                }
                // Depending on absolute paths like
                // /Users/user/homebrew/Cellar/protobuf/29.1/include/google/protobuf/timestamp.proto
                // is valid, but we want to have output more deterministic.
                let pattern = b"/include/google/protobuf/";
                if line.windows(pattern.len()).any(|w| w == pattern) {
                    continue;
                }

                let line_str = std::str::from_utf8(line)
                    .context("dependency path is not valid UTF-8")?;

                if !fs::exists(line_str)? {
                    return Err(anyhow::anyhow!("dependency file not found: {line_str}"));
                }

                if line_str.contains('\n') || line_str.contains('\r') {
                    return Err(anyhow::anyhow!("dependency path contains newline: {line_str}"));
                }

                println!("cargo:rerun-if-changed={line_str}");
            }
        }

        Ok(())
    }

    pub fn compile_protos(
        self,
        protos: &[impl AsRef<Path>],
        includes: &[impl AsRef<Path>],
    ) -> anyhow::Result<()> {
        for proto in protos {
            let proto = proto.as_ref();
            if proto.is_absolute() {
                return Err(anyhow::anyhow!(
                    "Absolute paths are not allowed: {}",
                    proto.display()
                ));
            }
        }

        let XaiProtoBuilder {
            builder,
            gen_pbjson,
            file_descriptor_set_path,
            pbjson_ignore_unknown_fields,
            pbjson_preserve_proto_field_names,
        } = self;
        let mut config = prost_build::Config::new();
        config.enable_type_names();

        let protoc = find_protoc::find_protoc()?;

        // Use fixed version of `protoc` binary.
        if let Some(protoc) = &protoc {
            config.protoc_executable(protoc);
        }

        // Find the protoc's well-known types include directory.
        // This is needed for Bazel sandboxed builds where protoc and its
        // include files are placed in different sandbox locations.
        let protoc_include_dir = find_protoc_include_dir(protoc.as_deref());

        let mut builder = builder.emit_rerun_if_changed(false);
        Self::emit_rerun_if_changed(
            protoc.as_deref(),
            protoc_include_dir.as_deref(),
            protos.iter().map(|p| p.as_ref()),
            includes.iter().map(|i| i.as_ref()),
        )?;

        let tempfile;

        let file_descriptor_set_path: Option<PathBuf> =
            if let Some(file_descriptor_set_path) = file_descriptor_set_path {
                Some(file_descriptor_set_path)
            } else if gen_pbjson {
                tempfile = tempfile::TempDir::new()?;
                let file_descriptor_set_path = tempfile.path().join("xai-proto-build.pbbin");
                builder = builder.file_descriptor_set_path(&file_descriptor_set_path);
                Some(file_descriptor_set_path)
            } else {
                None
            };

        // Build the full includes list, prepending the protoc include directory
        // if found (for well-known types like google/protobuf/timestamp.proto).
        let all_includes: Vec<&Path> = protoc_include_dir
            .as_deref()
            .into_iter()
            .chain(includes.iter().map(|i| i.as_ref()))
            .collect();

        let protos: Vec<&Path> = protos.iter().map(|p| p.as_ref()).collect();

        builder
            .compile_with_config(config, &protos, &all_includes)
            .context("tonic_build failed")?;

        if gen_pbjson {
            let file_descriptor_set_path =
                file_descriptor_set_path.context("fds must be set at this moment")?;
            let descriptor_set = fs::read(&file_descriptor_set_path).with_context(|| {
                format!(
                    "Failed to read file descriptor set {}",
                    file_descriptor_set_path.display()
                )
            })?;
            let mut builder = pbjson_build::Builder::new();
            builder
                .register_descriptors(&descriptor_set)
                .context("Failed to register descriptors in pbjson_build")?;
            if pbjson_ignore_unknown_fields {
                builder.ignore_unknown_fields();
            }
            if pbjson_preserve_proto_field_names {
                builder.preserve_proto_field_names();
            }
            builder
                .build(&["."])
                .context("Failed to build descriptor set")?;
        }

        Ok(())
    }
}

pub fn configure() -> XaiProtoBuilder {
    let builder = tonic_prost_build::configure()
        .compile_well_known_types(true)
        .extern_path(".google.protobuf", "::pbjson_types")
        .extern_path(".google.protobuf.Empty", "()")
        .protoc_arg("--experimental_allow_proto3_optional");
    XaiProtoBuilder {
        builder,
        gen_pbjson: false,
        pbjson_ignore_unknown_fields: false,
        pbjson_preserve_proto_field_names: false,
        file_descriptor_set_path: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir_named(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "xai-proto-build-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    /// The bug this guards: emitting a path Cargo cannot resolve makes the
    /// build script permanently dirty, so every downstream crate recompiles on
    /// every cargo invocation.
    #[test]
    fn an_unresolvable_protoc_emits_nothing() {
        assert_eq!(
            rerun_if_changed_for_protoc(Path::new("/nonexistent-aXbYcZ/protoc")),
            None
        );
        assert_eq!(
            rerun_if_changed_for_protoc(Path::new("protoc")),
            None,
            "the bare name resolves against the package root, where no such \
             file exists"
        );
    }

    #[test]
    fn a_resolvable_protoc_is_emitted() {
        let dir = temp_dir_named("emit");
        let protoc = dir.join("protoc");
        fs::write(&protoc, b"").expect("write protoc");

        assert_eq!(
            rerun_if_changed_for_protoc(&protoc),
            Some(protoc.to_str().expect("temp path is UTF-8"))
        );

        fs::remove_dir_all(&dir).ok();
    }

    /// `PATH` entries are `OsStr`, so a `protoc` under a directory with
    /// non-UTF-8 bytes runs perfectly well — `Command` never needs to decode
    /// it. Only `cargo:rerun-if-changed` does, and treating that as fatal
    /// would fail a build that had already located a working protoc, purely to
    /// protect a rebuild trigger.
    ///
    /// No file is created: APFS and HFS+ refuse these names with `Illegal byte
    /// sequence`, so a filesystem-backed version of this test could only ever
    /// run on Linux. Because the encoding gate precedes the existence gate,
    /// this reaches the branch under test on every platform.
    #[cfg(unix)]
    #[test]
    fn a_non_utf8_protoc_is_skipped_rather_than_failing_the_build() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        // 0xFF can never appear in UTF-8, but is a legal byte in a Linux
        // filename.
        let protoc = Path::new(OsStr::from_bytes(b"/usr/local/b\xffn/protoc"));

        assert_eq!(
            rerun_if_changed_for_protoc(protoc),
            None,
            "an undecodable path must be skipped, not raised — the previous \
             code turned this into a build failure via `to_str()?`"
        );
    }

    /// Why this function had no reachable non-UTF-8 case until recently: the
    /// `PATH` branch used to return the bare name, and the walk to a sibling
    /// `include` dies on it. Resolving the real path is what made the rest of
    /// this behaviour reachable at all.
    #[test]
    fn a_bare_protoc_name_derives_no_include_dir() {
        assert_eq!(find_protoc_include_dir(Some(Path::new("protoc"))), None);
        assert_eq!(find_protoc_include_dir(None), None);
    }

    #[test]
    fn a_sibling_include_dir_is_found() {
        let root = temp_dir_named("layout");
        fs::create_dir_all(root.join("bin")).expect("bin");
        fs::create_dir_all(root.join("include")).expect("include");
        let protoc = root.join("bin").join("protoc");
        fs::write(&protoc, b"").expect("write protoc");

        assert_eq!(
            find_protoc_include_dir(Some(&protoc)),
            Some(root.join("include"))
        );

        fs::remove_dir_all(&root).ok();
    }

    /// A contract assertion, not a regression catch — and worth labelling as
    /// such, because it looks like one. The path does not exist, so `is_dir`
    /// rejects it whether or not the decode gate is there; removing the gate
    /// leaves this test green. Proving the gate needs a directory that exists
    /// *and* cannot be decoded, which is the Linux-only test below.
    #[cfg(unix)]
    #[test]
    fn a_non_utf8_include_dir_is_declined() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let protoc = Path::new(OsStr::from_bytes(b"/opt/we\xffird/bin/protoc"));

        assert_eq!(find_protoc_include_dir(Some(protoc)), None);
    }

    /// The half that actually pins the decode gate, and it can only run where
    /// the filesystem accepts the name: APFS and HFS+ reject non-UTF-8
    /// filenames with `Illegal byte sequence`, so this is unconstructible on
    /// macOS. Without the gate the directory exists, `is_dir` accepts it, and
    /// the build fails later in `emit_rerun_if_changed` — for a protoc that
    /// runs perfectly well and that, before #87, derived no include directory
    /// at all.
    #[cfg(target_os = "linux")]
    #[test]
    fn an_existing_non_utf8_include_dir_is_declined() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let parent = temp_dir_named("nonutf8-layout");
        let root = parent.join(OsStr::from_bytes(b"we\xffird"));
        fs::create_dir_all(root.join("bin")).expect("bin");
        fs::create_dir_all(root.join("include")).expect("include");
        let protoc = root.join("bin").join("protoc");
        fs::write(&protoc, b"").expect("write protoc");

        assert!(
            root.join("include").is_dir(),
            "the point of this test is an include directory that exists and \
             cannot be decoded; without the first half `is_dir` would reject \
             it and the decode gate would go unexercised"
        );
        assert_eq!(find_protoc_include_dir(Some(&protoc)), None);

        fs::remove_dir_all(&parent).ok();
    }

    #[cfg(unix)]
    #[test]
    fn emit_rerun_if_changed_handles_non_utf8_well_known_types_dependency() {
        use std::os::unix::fs::PermissionsExt;
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let dir = temp_dir_named("nonutf8-dep");
        let proto_file = dir.join("test.proto");
        fs::write(&proto_file, b"").expect("write proto");

        let protoc_script = dir.join("protoc");
        let mut script_content = Vec::new();
        script_content.extend_from_slice(b"#!/bin/sh\n");
        script_content.extend_from_slice(b"printf '/dev/null: ");
        script_content.extend_from_slice(proto_file.to_str().expect("temp path is utf8").as_bytes());
        script_content.extend_from_slice(b" \\\\\\n  /opt/we\\xffird/include/google/protobuf/timestamp.proto\\n'\n");
        script_content.extend_from_slice(b"exit 0\n");

        fs::write(&protoc_script, script_content).expect("write stub protoc");
        fs::set_permissions(&protoc_script, fs::Permissions::from_mode(0o755)).expect("chmod");

        let include_dir_bytes = b"/opt/we\xffird/include";
        let include_dir = Path::new(OsStr::from_bytes(include_dir_bytes));

        let res = XaiProtoBuilder::emit_rerun_if_changed(
            Some(&protoc_script),
            Some(include_dir),
            [proto_file.as_path()],
            [] as [&Path; 0]
        );

        assert!(res.is_ok(), "should succeed despite non-UTF-8 paths, err: {:?}", res.err());

        fs::remove_dir_all(&dir).ok();
    }
}

fn trim_ascii(mut bytes: &[u8]) -> &[u8] {
    while let Some((first, rest)) = bytes.split_first() {
        if first.is_ascii_whitespace() {
            bytes = rest;
        } else {
            break;
        }
    }
    while let Some((last, rest)) = bytes.split_last() {
        if last.is_ascii_whitespace() {
            bytes = rest;
        } else {
            break;
        }
    }
    bytes
}
