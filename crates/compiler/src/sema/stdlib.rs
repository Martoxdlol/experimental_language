//! Toolchain standard-library catalog.
//!
//! The compiler needs a small amount of built-in knowledge to bootstrap
//! `core:*`/`std:*`: which module paths exist, which symbols each module
//! exports, and whether those symbols are pure Otter definitions, Rust-backed
//! runtime intrinsics, or a mix of both. Otter-authored definitions live under
//! `sema/stdlib_src/{core,std}/` and are collected into hidden module-local
//! owners under the private `__builtins__` root before this catalog creates
//! public import views.
//! Keeping the catalog explicit gives us the extension point for target-specific
//! std providers later (`std:fs` on WASI vs POSIX, embedded `no_std`, vendor
//! toolchains, etc.) without scattering module policy across name resolution.

/// A standard-library tier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StdTier {
    /// `core:*`: compiler/runtime substrate with language-privileged semantics.
    Core,
    /// `std:*`: official toolchain library, portable or target-backed, but not
    /// language-privileged unless separately documented.
    Std,
}

/// How a toolchain module is implemented today.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StdImplementation {
    /// Surface and behavior are authored in Otter Fusion source collected into
    /// hidden module-local toolchain owners.
    Otter,
    /// Surface is represented by marker definitions, but behavior is emitted by
    /// compiler/runtime intrinsics written in Rust.
    RustBacked,
    /// Public API is ordinary Otter Fusion definitions layered over Rust-backed
    /// runtime intrinsics or compiler intrinsics.
    Mixed,
}

/// One importable `core:*` or `std:*` module provided by the current toolchain.
#[derive(Clone, Copy, Debug)]
pub struct StdModuleSpec {
    pub path: &'static [&'static str],
    pub tier: StdTier,
    pub implementation: StdImplementation,
    pub exports: &'static [&'static str],
}

/// One Otter Fusion source file bundled into the toolchain stdlib.
#[derive(Clone, Copy, Debug)]
pub struct ToolchainSourceSpec {
    /// Public module path represented by this source file.
    pub path: &'static [&'static str],
    /// Source text compiled into a hidden module-local owner under the private
    /// `__builtins__` root.
    pub source: &'static str,
}

/// A provider of toolchain `core:*` / `std:*` modules.
///
/// Today there is exactly one built-in provider, but the resolver is structured
/// around this trait so a future target can select another provider catalog
/// without changing import semantics. A WASI provider, for example, can expose a
/// different `std:fs` implementation while still sharing compiler substrate such
/// as `core:prelude` and `core:collections`.
pub trait StdProvider {
    /// Stable provider identity, used in diagnostics, lockfiles, and future
    /// target metadata. Provider names must be non-empty ASCII identifiers
    /// using letters, digits, `.`, `_`, or `-`.
    fn name(&self) -> &'static str;

    /// The modules exported by this provider.
    fn modules(&self) -> &'static [StdModuleSpec];

    /// Find an exact module path such as `["std", "io"]`.
    fn module(&self, path: &[String]) -> Option<&'static StdModuleSpec> {
        self.modules().iter().find(|spec| {
            spec.path
                .iter()
                .copied()
                .eq(path.iter().map(String::as_str))
        })
    }
}

/// Whether a provider name is stable enough for diagnostics and future
/// lockfile/sysroot metadata.
pub fn valid_provider_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Render a catalog path with the public import spelling: one scheme separator
/// (`core:` / `std:`), then slash-separated submodule segments.
pub fn display_module_path(path: &[&str]) -> String {
    match path {
        [] => "<empty>".to_string(),
        [root] => (*root).to_string(),
        [root, rest @ ..] => format!("{root}:{}", rest.join("/")),
    }
}

/// The host/native standard-library provider bundled with this compiler.
#[derive(Clone, Copy, Debug, Default)]
pub struct BuiltinStdProvider;

impl StdProvider for BuiltinStdProvider {
    fn name(&self) -> &'static str {
        "builtin"
    }

    fn modules(&self) -> &'static [StdModuleSpec] {
        TOOLCHAIN_MODULES
    }
}

/// The active provider for the current compiler. This is deliberately a function
/// rather than a global constant so target selection can later thread through
/// project configuration without rewriting every call site.
pub fn active_provider() -> BuiltinStdProvider {
    BuiltinStdProvider
}

/// Otter-authored toolchain modules bundled with this compiler.
///
/// Source files are collected into hidden module-local owners under the private
/// `__builtins__` root in this order, then [`TOOLCHAIN_MODULES`] builds the
/// public `core:*`/`std:*` views. Keeping the source manifest explicit prevents
/// the collector, catalog tests, and future sysroot/provider work from growing
/// separate file lists.
pub const TOOLCHAIN_SOURCES: &[ToolchainSourceSpec] = &[
    ToolchainSourceSpec {
        path: &["core", "prelude"],
        source: include_str!("stdlib_src/core/prelude.otter"),
    },
    ToolchainSourceSpec {
        path: &["core", "async"],
        source: include_str!("stdlib_src/core/async.otter"),
    },
    ToolchainSourceSpec {
        path: &["std", "error"],
        source: include_str!("stdlib_src/std/error.otter"),
    },
    ToolchainSourceSpec {
        path: &["core", "collections"],
        source: include_str!("stdlib_src/core/collections.otter"),
    },
    ToolchainSourceSpec {
        path: &["std", "bytes"],
        source: include_str!("stdlib_src/std/bytes.otter"),
    },
    ToolchainSourceSpec {
        path: &["std", "collections"],
        source: include_str!("stdlib_src/std/collections.otter"),
    },
    ToolchainSourceSpec {
        path: &["std", "fmt"],
        source: include_str!("stdlib_src/std/fmt.otter"),
    },
    ToolchainSourceSpec {
        path: &["std", "time"],
        source: include_str!("stdlib_src/std/time.otter"),
    },
    ToolchainSourceSpec {
        path: &["std", "fs"],
        source: include_str!("stdlib_src/std/fs.otter"),
    },
    ToolchainSourceSpec {
        path: &["std", "hash"],
        source: include_str!("stdlib_src/std/hash.otter"),
    },
    ToolchainSourceSpec {
        path: &["std", "http"],
        source: include_str!("stdlib_src/std/http.otter"),
    },
    ToolchainSourceSpec {
        path: &["std", "json"],
        source: include_str!("stdlib_src/std/json.otter"),
    },
    ToolchainSourceSpec {
        path: &["std", "net", "types"],
        source: include_str!("stdlib_src/std/net_types.otter"),
    },
    ToolchainSourceSpec {
        path: &["std", "net"],
        source: include_str!("stdlib_src/std/net.otter"),
    },
    ToolchainSourceSpec {
        path: &["std", "rand"],
        source: include_str!("stdlib_src/std/rand.otter"),
    },
    ToolchainSourceSpec {
        path: &["std", "log"],
        source: include_str!("stdlib_src/std/log.otter"),
    },
    ToolchainSourceSpec {
        path: &["std", "process"],
        source: include_str!("stdlib_src/std/process.otter"),
    },
    ToolchainSourceSpec {
        path: &["std", "thread"],
        source: include_str!("stdlib_src/std/thread.otter"),
    },
    ToolchainSourceSpec {
        path: &["std", "task"],
        source: include_str!("stdlib_src/std/task.otter"),
    },
    ToolchainSourceSpec {
        path: &["std", "sync"],
        source: include_str!("stdlib_src/std/sync.otter"),
    },
    ToolchainSourceSpec {
        path: &["core", "sync", "atomic"],
        source: include_str!("stdlib_src/core/sync_atomic.otter"),
    },
    ToolchainSourceSpec {
        path: &["std", "async"],
        source: include_str!("stdlib_src/std/async.otter"),
    },
    ToolchainSourceSpec {
        path: &["core", "ffi"],
        source: include_str!("stdlib_src/core/ffi.otter"),
    },
    ToolchainSourceSpec {
        path: &["std", "io"],
        source: include_str!("stdlib_src/std/io.otter"),
    },
    ToolchainSourceSpec {
        path: &["core", "compiler"],
        source: include_str!("stdlib_src/core/compiler.otter"),
    },
];

/// Built-in toolchain modules exposed through explicit import paths.
///
/// This is intentionally a curated view, not "everything under `__builtins__`".
/// Internal adapters (`ListIter`, `MapKeys`, etc.) remain private while the
/// documented module surface is importable by name.
pub const TOOLCHAIN_MODULES: &[StdModuleSpec] = &[
    StdModuleSpec {
        path: &["core", "prelude"],
        tier: StdTier::Core,
        implementation: StdImplementation::Mixed,
        exports: &[
            "Iterator",
            "Item",
            "Done",
            "FromResidual",
            "Try",
            "Clone",
            "Drop",
            "Eq",
            "Ord",
            "ToStr",
            "Hash",
            "Future",
            "Ready",
            "Pending",
            "Context",
            "panic",
            "panic_with",
        ],
    },
    StdModuleSpec {
        path: &["core", "collections"],
        tier: StdTier::Core,
        implementation: StdImplementation::Mixed,
        exports: &["List", "Map", "Set", "Entry"],
    },
    StdModuleSpec {
        path: &["core", "async"],
        tier: StdTier::Core,
        implementation: StdImplementation::Mixed,
        exports: &["Future", "Ready", "Pending", "Context", "AsyncIterator"],
    },
    StdModuleSpec {
        path: &["core", "ffi"],
        tier: StdTier::Core,
        implementation: StdImplementation::Mixed,
        exports: &[
            "c_int",
            "c_uint",
            "c_long",
            "c_ulong",
            "c_longlong",
            "c_ulonglong",
            "c_short",
            "c_ushort",
            "c_char",
            "c_schar",
            "c_uchar",
            "c_float",
            "c_double",
            "c_size_t",
            "c_ptrdiff_t",
            "c_intptr_t",
            "c_uintptr_t",
            "c_void",
            "c_va_list",
            "Foreign",
            "CString",
            "CStr",
            "Buffer",
        ],
    },
    StdModuleSpec {
        path: &["std", "error"],
        tier: StdTier::Std,
        implementation: StdImplementation::Otter,
        exports: &["Error", "Annotated", "with_context"],
    },
    StdModuleSpec {
        path: &["std", "bytes"],
        tier: StdTier::Std,
        implementation: StdImplementation::Otter,
        exports: &["Bytes", "BytesCursor", "Utf8Error"],
    },
    StdModuleSpec {
        path: &["std", "collections"],
        tier: StdTier::Std,
        implementation: StdImplementation::Otter,
        exports: &["Deque", "deque", "deque_from_list"],
    },
    StdModuleSpec {
        path: &["std", "fmt"],
        tier: StdTier::Std,
        implementation: StdImplementation::Otter,
        exports: &["Display", "Debug", "FmtSink", "FmtError"],
    },
    StdModuleSpec {
        path: &["std", "io"],
        tier: StdTier::Std,
        implementation: StdImplementation::Mixed,
        exports: &[
            "Reader",
            "Writer",
            "AsyncReader",
            "AsyncWriter",
            "Seeker",
            "SeekFrom",
            "Start",
            "Current",
            "End",
            "IoError",
            "IoErrorKind",
            "NotFound",
            "PermissionDenied",
            "ConnectionRefused",
            "IoTimedOut",
            "UnexpectedEof",
            "Interrupted",
            "OtherIo",
            "Stdin",
            "Stdout",
            "Stderr",
            "BufReader",
            "BufWriter",
            "buf_reader",
            "buf_writer",
            "stdin",
            "stdout",
            "stderr",
            "print",
            "println",
            "eprint",
            "eprintln",
        ],
    },
    StdModuleSpec {
        path: &["std", "time"],
        tier: StdTier::Std,
        implementation: StdImplementation::Mixed,
        exports: &[
            "Duration",
            "Instant",
            "SystemTime",
            "TimeZone",
            "Utc",
            "FixedOffset",
            "NamedTimeZone",
            "DateTime",
            "TimeError",
            "utc",
            "fixed_offset",
            "named_timezone",
            "time_error",
            "date_time",
            "parse_iso8601",
            "sleep",
        ],
    },
    StdModuleSpec {
        path: &["std", "fs"],
        tier: StdTier::Std,
        implementation: StdImplementation::Mixed,
        exports: &[
            "Path",
            "File",
            "FileKind",
            "RegularFile",
            "Directory",
            "Symlink",
            "OtherKind",
            "DirEntry",
            "DirEntries",
            "Permissions",
            "Metadata",
            "OpenOptions",
            "regular_file",
            "directory",
            "symlink",
            "other_kind",
            "dir_entry",
            "permissions",
            "metadata",
            "open_options",
            "read_to_string",
            "write_string",
            "append_string",
            "read",
            "write",
            "remove",
            "rename",
            "create_dir",
            "create_dir_all",
            "canonicalize",
            "native_separator",
            "path_from_native",
            "read_dir",
        ],
    },
    StdModuleSpec {
        path: &["std", "hash"],
        tier: StdTier::Std,
        implementation: StdImplementation::Mixed,
        exports: &[
            "Hasher",
            "DefaultHasher",
            "KeyedHasher",
            "HashSeedError",
            "hash_value",
            "write_hash",
            "combine_hash",
            "keyed_hasher",
            "os_keyed_hasher",
        ],
    },
    StdModuleSpec {
        path: &["std", "http"],
        tier: StdTier::Std,
        implementation: StdImplementation::Otter,
        exports: &[
            "Method",
            "Get",
            "Head",
            "Post",
            "Put",
            "Delete",
            "Patch",
            "Options",
            "Trace",
            "Connect",
            "Custom",
            "HttpVersion",
            "Http10",
            "Http11",
            "Http2",
            "Http3",
            "Status",
            "Headers",
            "HeaderEntry",
            "HttpRequest",
            "HttpResponse",
            "method_get",
            "method_head",
            "method_post",
            "method_put",
            "method_delete",
            "method_patch",
            "method_options",
            "method_trace",
            "method_connect",
            "method_custom",
            "http_10",
            "http_11",
            "http_2",
            "http_3",
            "status",
            "headers",
            "header_entry",
            "http_request",
            "http_response",
        ],
    },
    StdModuleSpec {
        path: &["std", "json"],
        tier: StdTier::Std,
        implementation: StdImplementation::Otter,
        exports: &[
            "Json",
            "json_null",
            "json_bool",
            "json_number",
            "json_string",
            "json_array",
            "json_object",
        ],
    },
    StdModuleSpec {
        path: &["std", "net", "types"],
        tier: StdTier::Std,
        implementation: StdImplementation::Otter,
        exports: &[
            "IpAddr",
            "SocketAddr",
            "Uri",
            "Url",
            "ParseError",
            "ip_v4",
            "ip_v6",
            "ip_v6_scoped",
            "socket_addr",
            "uri",
            "url",
            "parse_ip_v4",
            "parse_ip_v6",
            "parse_ipv4_socket_addr",
            "parse_socket_addr",
            "parse_uri",
            "parse_url",
            "percent_encode_component",
            "percent_decode_component",
        ],
    },
    StdModuleSpec {
        path: &["std", "net"],
        tier: StdTier::Std,
        implementation: StdImplementation::Mixed,
        exports: &[
            "TcpStream",
            "AsyncTcpStream",
            "AsyncTcpListener",
            "TcpListener",
            "UdpSocket",
            "AsyncUdpSocket",
            "resolve",
        ],
    },
    StdModuleSpec {
        path: &["std", "rand"],
        tier: StdTier::Std,
        implementation: StdImplementation::Mixed,
        exports: &[
            "Rng",
            "RandomError",
            "OsRng",
            "SeededRng",
            "ThreadRng",
            "random_error",
            "os_rng",
            "os_bytes",
            "thread_rng",
            "gen_range_i64",
            "gen_range_u64",
            "gen_f64",
            "gen_range_f64",
            "gen_triangular_f64",
            "gen_bates_f64",
            "gen_irwin_hall_f64",
            "gen_min_f64",
            "gen_max_f64",
            "gen_midrange_f64",
            "gen_median_f64",
            "gen_bool",
            "gen_binomial",
            "gen_index",
            "fill_bytes_n",
            "gen_bytes",
            "choose_index",
            "choose",
            "weighted_index",
            "choose_weighted",
            "sample_indices",
            "sample",
            "shuffle",
            "shuffled",
        ],
    },
    StdModuleSpec {
        path: &["std", "log"],
        tier: StdTier::Std,
        implementation: StdImplementation::Otter,
        exports: &[
            "Level",
            "LogTrace",
            "LogDebug",
            "Info",
            "Warn",
            "LogError",
            "Record",
            "LoggerAlreadySet",
            "Logger",
            "level_trace",
            "level_debug",
            "level_info",
            "level_warn",
            "level_error",
            "record",
            "empty_fields",
            "log_record",
            "trace",
            "debug",
            "info",
            "warn",
            "error",
            "info_with",
            "trace_with",
            "debug_with",
            "warn_with",
            "error_with",
        ],
    },
    StdModuleSpec {
        path: &["std", "process"],
        tier: StdTier::Std,
        implementation: StdImplementation::Mixed,
        exports: &[
            "Command",
            "Child",
            "ExitStatus",
            "Output",
            "command",
            "exit_status",
            "output",
            "args",
            "env",
            "env_all",
            "set_env",
            "exit",
            "abort",
        ],
    },
    StdModuleSpec {
        path: &["std", "thread"],
        tier: StdTier::Std,
        implementation: StdImplementation::Mixed,
        exports: &["Thread", "JoinHandle", "Joined", "Panicked"],
    },
    StdModuleSpec {
        path: &["std", "task"],
        tier: StdTier::Std,
        implementation: StdImplementation::Mixed,
        exports: &["Task", "JoinHandle", "Joined", "Panicked", "Cancelled"],
    },
    StdModuleSpec {
        path: &["std", "sync"],
        tier: StdTier::Std,
        implementation: StdImplementation::Mixed,
        exports: &[
            "Sender",
            "Receiver",
            "ChannelClosed",
            "Shared",
            "LockBusy",
            "MpmcSender",
            "MpmcReceiver",
            "channel",
            "channel_bounded",
            "channel_mpmc",
            "channel_mpmc_bounded",
        ],
    },
    StdModuleSpec {
        path: &["core", "sync", "atomic"],
        tier: StdTier::Core,
        implementation: StdImplementation::Otter,
        exports: &[
            "Ordering",
            "Relaxed",
            "Acquire",
            "Release",
            "AcqRel",
            "SeqCst",
            "AtomicI64",
            "AtomicI32",
            "AtomicU64",
            "AtomicU32",
            "AtomicBool",
            "AtomicPtr",
            "ordering_relaxed",
            "ordering_acquire",
            "ordering_release",
            "ordering_acq_rel",
            "ordering_seq_cst",
        ],
    },
    StdModuleSpec {
        path: &["std", "async"],
        tier: StdTier::Std,
        implementation: StdImplementation::Mixed,
        exports: &["TimedOut", "yield_now", "sleep", "timeout"],
    },
    StdModuleSpec {
        path: &["core", "compiler"],
        tier: StdTier::Core,
        implementation: StdImplementation::Mixed,
        exports: &["MacroContext", "ASTNode", "Span"],
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn display_module_path_uses_scheme_colon_and_slash_submodules() {
        assert_eq!(display_module_path(&[]), "<empty>");
        assert_eq!(display_module_path(&["std"]), "std");
        assert_eq!(display_module_path(&["std", "io"]), "std:io");
        assert_eq!(
            display_module_path(&["core", "sync", "atomic"]),
            "core:sync/atomic"
        );
    }

    #[test]
    fn catalog_paths_match_tiers_and_are_unique() {
        let mut paths = HashSet::new();
        for spec in TOOLCHAIN_MODULES {
            assert!(
                paths.insert(spec.path),
                "duplicate stdlib module path {:?}",
                spec.path
            );
            match spec.tier {
                StdTier::Core => assert_eq!(spec.path.first(), Some(&"core")),
                StdTier::Std => assert_eq!(spec.path.first(), Some(&"std")),
            }
            assert!(
                spec.path.len() >= 2,
                "stdlib modules must be scheme + module path"
            );
            for segment in spec.path.iter().skip(1) {
                assert!(
                    !segment.is_empty()
                        && *segment != "."
                        && *segment != ".."
                        && !segment.contains('/'),
                    "stdlib module path {:?} contains unaddressable segment {:?}",
                    spec.path,
                    segment
                );
            }
        }
    }

    #[test]
    fn toolchain_source_paths_are_addressable() {
        let mut paths = HashSet::new();
        for spec in TOOLCHAIN_SOURCES {
            assert!(
                paths.insert(spec.path),
                "duplicate toolchain source path {:?}",
                spec.path
            );
            assert!(
                spec.path.len() >= 2,
                "toolchain source paths must be scheme + module path"
            );
            assert!(
                matches!(spec.path.first(), Some(&"core" | &"std")),
                "toolchain source path {:?} must start with core or std",
                spec.path
            );
            for segment in spec.path.iter().skip(1) {
                assert!(
                    !segment.is_empty()
                        && *segment != "."
                        && *segment != ".."
                        && !segment.contains('/'),
                    "toolchain source path {:?} contains unaddressable segment {:?}",
                    spec.path,
                    segment
                );
            }
        }
    }

    #[test]
    fn catalog_exports_are_unique_within_each_module() {
        for spec in TOOLCHAIN_MODULES {
            let mut seen = HashSet::new();
            for export in spec.exports {
                assert!(
                    seen.insert(export),
                    "duplicate export `{export}` in {:?}",
                    spec.path
                );
            }
        }
    }

    #[test]
    fn catalog_exports_have_import_gating_diagnostics() {
        const REQUIRE_IMPORT_TESTS: &[&str] = &[
            include_str!("../../../../tests/cases/stdlib/core_async_requires_import.otter"),
            include_str!(
                "../../../../tests/cases/stdlib/core_collections_set_requires_import.otter"
            ),
            include_str!("../../../../tests/cases/stdlib/core_compiler_requires_import.otter"),
            include_str!("../../../../tests/cases/stdlib/core_ffi_requires_import.otter"),
            include_str!("../../../../tests/cases/stdlib/core_prelude_requires_import.otter"),
            include_str!("../../../../tests/cases/stdlib/core_sync_atomic_requires_import.otter"),
            include_str!("../../../../tests/cases/stdlib/std_async_runtime_requires_import.otter"),
            include_str!("../../../../tests/cases/stdlib/std_bytes_requires_import.otter"),
            include_str!(
                "../../../../tests/cases/stdlib/std_collections_deque_requires_import.otter"
            ),
            include_str!("../../../../tests/cases/stdlib/std_error_requires_import.otter"),
            include_str!("../../../../tests/cases/stdlib/std_fmt_requires_import.otter"),
            include_str!("../../../../tests/cases/stdlib/std_fs_dir_entry_requires_import.otter"),
            include_str!("../../../../tests/cases/stdlib/std_fs_helpers_require_import.otter"),
            include_str!("../../../../tests/cases/stdlib/std_fs_metadata_requires_import.otter"),
            include_str!("../../../../tests/cases/stdlib/std_fs_path_requires_import.otter"),
            include_str!("../../../../tests/cases/stdlib/std_hash_requires_import.otter"),
            include_str!("../../../../tests/cases/stdlib/std_http_requires_import.otter"),
            include_str!("../../../../tests/cases/stdlib/std_io_requires_import.otter"),
            include_str!("../../../../tests/cases/stdlib/std_json_requires_import.otter"),
            include_str!("../../../../tests/cases/stdlib/std_log_requires_import.otter"),
            include_str!("../../../../tests/cases/stdlib/std_net_requires_import.otter"),
            include_str!("../../../../tests/cases/stdlib/std_net_types_requires_import.otter"),
            include_str!("../../../../tests/cases/stdlib/std_process_requires_import.otter"),
            include_str!("../../../../tests/cases/stdlib/std_rand_requires_import.otter"),
            include_str!("../../../../tests/cases/stdlib/std_sync_requires_import.otter"),
            include_str!("../../../../tests/cases/stdlib/std_task_requires_import.otter"),
            include_str!("../../../../tests/cases/stdlib/std_thread_requires_import.otter"),
            include_str!("../../../../tests/cases/stdlib/std_time_datetime_requires_import.otter"),
            include_str!("../../../../tests/cases/stdlib/std_time_duration_requires_import.otter"),
            include_str!("../../../../tests/cases/stdlib/std_time_instant_requires_import.otter"),
        ];

        let mut covered = HashSet::new();
        for source in REQUIRE_IMPORT_TESTS {
            for line in source
                .lines()
                .filter(|line| line.starts_with("//@ stderr:"))
            {
                let mut rest = line;
                while let Some(start) = rest.find('`') {
                    let after_start = &rest[start + 1..];
                    if let Some(end) = after_start.find('`') {
                        covered.insert(&after_start[..end]);
                        rest = &after_start[end + 1..];
                    } else {
                        break;
                    }
                }
            }
        }

        for spec in TOOLCHAIN_MODULES {
            for export in spec.exports {
                assert!(
                    covered.contains(export),
                    "stdlib export `{}::{}` lacks require-import diagnostic coverage",
                    display_module_path(spec.path),
                    export
                );
            }
        }
    }

    #[test]
    fn catalog_has_otter_and_mixed_modules() {
        assert!(TOOLCHAIN_MODULES
            .iter()
            .any(|m| m.implementation == StdImplementation::Otter));
        assert!(TOOLCHAIN_MODULES
            .iter()
            .any(|m| m.implementation == StdImplementation::Mixed));
    }

    #[test]
    fn catalog_paths_match_toolchain_source_files() {
        let source_modules: HashSet<&[&str]> =
            TOOLCHAIN_SOURCES.iter().map(|spec| spec.path).collect();
        let catalog_modules: HashSet<&[&str]> =
            TOOLCHAIN_MODULES.iter().map(|spec| spec.path).collect();

        assert_eq!(
            catalog_modules, source_modules,
            "catalog modules must stay in sync with stdlib_src/core and stdlib_src/std"
        );
    }

    #[test]
    fn core_catalog_is_limited_to_language_substrate() {
        let allowed_core_modules: HashSet<&[&str]> = [
            &["core", "prelude"][..],
            &["core", "collections"][..],
            &["core", "async"][..],
            &["core", "ffi"][..],
            &["core", "sync", "atomic"][..],
            &["core", "compiler"][..],
        ]
        .into_iter()
        .collect();

        for spec in TOOLCHAIN_MODULES
            .iter()
            .filter(|spec| spec.tier == StdTier::Core)
        {
            assert!(
                allowed_core_modules.contains(spec.path),
                "{:?} is marked core; add a design justification before growing core:*",
                spec.path
            );
        }
    }

    #[test]
    fn process_control_markers_live_in_std_process_not_core_prelude() {
        let core_prelude = TOOLCHAIN_MODULES
            .iter()
            .find(|spec| spec.path == ["core", "prelude"])
            .expect("core:prelude catalog entry");
        assert!(
            !core_prelude.exports.contains(&"exit"),
            "process exit is std:process API, not core substrate"
        );
        assert!(
            !core_prelude.exports.contains(&"abort"),
            "process abort is std:process API, not core substrate"
        );

        let std_process = TOOLCHAIN_MODULES
            .iter()
            .find(|spec| spec.path == ["std", "process"])
            .expect("std:process catalog entry");
        assert!(
            std_process.exports.contains(&"exit"),
            "std:process must export the exit marker"
        );
        assert!(
            std_process.exports.contains(&"abort"),
            "std:process must export the abort marker"
        );
    }

    #[test]
    fn wait_capable_stdlib_surfaces_stay_future_returning_at_source_level() {
        fn source_for(path: &[&str]) -> &'static str {
            TOOLCHAIN_SOURCES
                .iter()
                .find(|spec| spec.path == path)
                .unwrap_or_else(|| panic!("missing toolchain source for {path:?}"))
                .source
        }

        let cases: &[(&[&str], &[&str])] = &[
            (
                &["std", "io"],
                &[
                    "function print(s: str): Future<null> async",
                    "function println(s: str): Future<null> async",
                    "function eprint(s: str): Future<null> async",
                    "function eprintln(s: str): Future<null> async",
                    "function read(self, buf: Bytes): Future<i64 | IoError> async",
                    "function read_to_end(self, buf: Bytes): Future<i64 | IoError> async",
                    "function read_async(self, buf: Bytes): Future<i64 | IoError> async",
                    "function read_to_end_async(self, buf: Bytes): Future<i64 | IoError> async",
                    "function write(self, buf: Bytes): Future<i64 | IoError> async",
                    "function write_all(self, buf: Bytes): Future<null | IoError> async",
                    "function flush(self): Future<null | IoError> async",
                    "function write_async(self, buf: Bytes): Future<i64 | IoError> async",
                    "function write_all_async(self, buf: Bytes): Future<null | IoError> async",
                    "function flush_async(self): Future<null | IoError> async",
                ],
            ),
            (
                &["std", "fs"],
                &[
                    "function read_to_string(path: Path): Future<str | IoError> async",
                    "function write_string(path: Path, contents: str): Future<null | IoError> async",
                    "function append_string(path: Path, contents: str): Future<null | IoError> async",
                    "function read(path: Path): Future<Bytes | IoError> async",
                    "function write(path: Path, data: Bytes): Future<null | IoError> async",
                    "function remove(path: Path): Future<null | IoError> async",
                    "function rename(from: Path, to: Path): Future<null | IoError> async",
                    "function create_dir(path: Path): Future<null | IoError> async",
                    "function create_dir_all(path: Path): Future<null | IoError> async",
                    "function read_dir(path: Path): Future<DirEntries | IoError> async",
                    "function canonicalize(path: Path): Future<Path | IoError> async",
                    "function exists(self): Future<bool | IoError> async",
                    "function is_file(self): Future<bool | IoError> async",
                    "function is_dir(self): Future<bool | IoError> async",
                    "function file_kind(self): Future<FileKind | IoError> async",
                    "function byte_len(self): Future<u64 | IoError> async",
                    "function permissions(self): Future<Permissions | IoError> async",
                    "function metadata(self): Future<Metadata | IoError> async",
                    "function canonicalize(self): Future<Path | IoError> async",
                    "function open(path: Path): Future<File | IoError> async",
                    "function create(path: Path): Future<File | IoError> async",
                    "function append(path: Path): Future<File | IoError> async",
                    "function open_with(path: Path, options: OpenOptions): Future<File | IoError> async",
                    "function read_to_string(self): Future<str | IoError> async",
                    "function write_string(self, contents: str): Future<null | IoError> async",
                    "function append_string(self, contents: str): Future<null | IoError> async",
                    "function close(self): Future<null | IoError> async",
                    "function read_async(self, buf: Bytes): Future<i64 | IoError> async",
                    "function read_to_end_async(self, buf: Bytes): Future<i64 | IoError> async",
                    "function write_async(self, buf: Bytes): Future<i64 | IoError> async",
                    "function write_all_async(self, buf: Bytes): Future<null | IoError> async",
                    "function flush_async(self): Future<null | IoError> async",
                    "function seek_async(self, pos: SeekFrom): Future<i64 | IoError> async",
                    "function read(self, buf: Bytes): Future<i64 | IoError> async",
                    "function read_to_end(self, buf: Bytes): Future<i64 | IoError> async",
                    "function write(self, buf: Bytes): Future<i64 | IoError> async",
                    "function write_all(self, buf: Bytes): Future<null | IoError> async",
                    "function flush(self): Future<null | IoError> async",
                    "function seek(self, pos: SeekFrom): Future<i64 | IoError> async",
                ],
            ),
            (
                &["std", "net"],
                &[
                    "function resolve(host: str): Future<List<IpAddr> | IoError> async",
                    "function connect(addr: SocketAddr): Future<TcpStream | IoError> async",
                    "function connect_timeout(addr: SocketAddr, timeout: Duration): Future<TcpStream | IoError> async",
                    "function peer_addr(self): Future<SocketAddr | IoError> async",
                    "function local_addr(self): Future<SocketAddr | IoError> async",
                    "function nodelay(self): Future<bool | IoError> async",
                    "function take_error(self): Future<IoError | null> async",
                    "function set_nodelay(self, on: bool): Future<null | IoError> async",
                    "function set_nonblocking(self, on: bool): Future<null | IoError> async",
                    "function read_timeout(self): Future<Duration | null | IoError> async",
                    "function set_read_timeout(self, timeout: Duration | null): Future<null | IoError> async",
                    "function write_timeout(self): Future<Duration | null | IoError> async",
                    "function set_write_timeout(self, timeout: Duration | null): Future<null | IoError> async",
                    "function ttl(self): Future<u32 | IoError> async",
                    "function set_ttl(self, ttl: u32): Future<null | IoError> async",
                    "function close(self): Future<null | IoError> async",
                    "function peek(self, buf: Bytes): Future<i64 | IoError> async",
                    "function read(self, buf: Bytes): Future<i64 | IoError> async",
                    "function read_to_end(self, buf: Bytes): Future<i64 | IoError> async",
                    "function write(self, buf: Bytes): Future<i64 | IoError> async",
                    "function write_all(self, buf: Bytes): Future<null | IoError> async",
                    "function flush(self): Future<null | IoError> async",
                    "function connect(addr: SocketAddr): Future<AsyncTcpStream | IoError> async",
                    "function connect_timeout(addr: SocketAddr, timeout: Duration): Future<AsyncTcpStream | IoError> async",
                    "function peer_addr(self): Future<SocketAddr | IoError> async",
                    "function local_addr(self): Future<SocketAddr | IoError> async",
                    "function close(self): Future<null | IoError> async",
                    "function peek_async(self, buf: Bytes): Future<i64 | IoError> async",
                    "function read_async(self, buf: Bytes): Future<i64 | IoError> async",
                    "function write_async(self, buf: Bytes): Future<i64 | IoError> async",
                    "function bind(addr: SocketAddr): Future<TcpListener | IoError> async",
                    "function accept(self): Future<(TcpStream, SocketAddr) | IoError> async",
                    "function bind(addr: SocketAddr): Future<AsyncTcpListener | IoError> async",
                    "function accept_async(self): Future<(AsyncTcpStream, SocketAddr) | IoError> async",
                    "function bind(addr: SocketAddr): Future<UdpSocket | IoError> async",
                    "function connect(self, addr: SocketAddr): Future<null | IoError> async",
                    "function broadcast(self): Future<bool | IoError> async",
                    "function set_broadcast(self, on: bool): Future<null | IoError> async",
                    "function multicast_loop_v4(self): Future<bool | IoError> async",
                    "function set_multicast_loop_v4(self, on: bool): Future<null | IoError> async",
                    "function multicast_loop_v6(self): Future<bool | IoError> async",
                    "function set_multicast_loop_v6(self, on: bool): Future<null | IoError> async",
                    "function multicast_ttl_v4(self): Future<u32 | IoError> async",
                    "function set_multicast_ttl_v4(self, ttl: u32): Future<null | IoError> async",
                    "function join_multicast_v4(self, group: IpAddr, iface: IpAddr): Future<null | IoError> async",
                    "function leave_multicast_v4(self, group: IpAddr, iface: IpAddr): Future<null | IoError> async",
                    "function join_multicast_v6(self, group: IpAddr, iface: u32): Future<null | IoError> async",
                    "function leave_multicast_v6(self, group: IpAddr, iface: u32): Future<null | IoError> async",
                    "function send_to(self, buf: Bytes, addr: SocketAddr): Future<i64 | IoError> async",
                    "function send(self, buf: Bytes): Future<i64 | IoError> async",
                    "function recv(self, buf: Bytes): Future<i64 | IoError> async",
                    "function peek(self, buf: Bytes): Future<i64 | IoError> async",
                    "function recv_from(self, buf: Bytes): Future<(i64, SocketAddr) | IoError> async",
                    "function peek_from(self, buf: Bytes): Future<(i64, SocketAddr) | IoError> async",
                    "function close(self): Future<null | IoError> async",
                    "function bind(addr: SocketAddr): Future<AsyncUdpSocket | IoError> async",
                    "function send_to_async(self, buf: Bytes, addr: SocketAddr): Future<i64 | IoError> async",
                    "function send_async(self, buf: Bytes): Future<i64 | IoError> async",
                    "function recv_async(self, buf: Bytes): Future<i64 | IoError> async",
                    "function peek_async(self, buf: Bytes): Future<i64 | IoError> async",
                    "function recv_from_async(self, buf: Bytes): Future<(i64, SocketAddr) | IoError> async",
                    "function peek_from_async(self, buf: Bytes): Future<(i64, SocketAddr) | IoError> async",
                ],
            ),
            (
                &["std", "process"],
                &[
                    "function args(): Future<List<str>> async",
                    "function env(name: str): Future<str | null> async",
                    "function env_all(): Future<Map<str, str>> async",
                    "function set_env(name: str, value: str): Future<null> async",
                    "function status(self): Future<ExitStatus | IoError> async",
                    "function output(self): Future<Output | IoError> async",
                    "function spawn(self): Future<Child | IoError> async",
                    "function wait(self): Future<ExitStatus | IoError> async",
                    "function kill(self): Future<null | IoError> async",
                ],
            ),
            (
                &["std", "time"],
                &[
                    "function sleep(duration: Duration): Future<null> async",
                    "function now(): Future<Instant> async",
                    "function elapsed(self): Future<Duration> async",
                    "function now(): Future<SystemTime> async",
                    "function now_utc(): Future<DateTime> async",
                    "function now_local(): Future<DateTime> async",
                    "function from_system_time_local(t: SystemTime): Future<DateTime | TimeError> async",
                ],
            ),
            (
                &["std", "async"],
                &[
                    "function yield_now(): Future<null> async",
                    "function sleep(ms: i64): Future<null> async",
                    "function timeout<T>(future: Future<T>, ms: i64): Future<T | TimedOut> async",
                ],
            ),
            (
                &["std", "rand"],
                &[
                    "function os_bytes(count: i64): Future<Bytes | RandomError> async",
                    "function thread_rng(): Future<ThreadRng | RandomError> async",
                    "function try_next_u32(self): Future<u32 | RandomError> async",
                    "function try_next_u64(self): Future<u64 | RandomError> async",
                    "function try_fill_bytes(self, buf: Bytes): Future<null | RandomError> async",
                    "function try_fill_bytes_n(self, buf: Bytes, count: i64): Future<null | RandomError> async",
                ],
            ),
            (
                &["std", "hash"],
                &["function os_keyed_hasher(): Future<KeyedHasher | HashSeedError> async"],
            ),
            (
                &["std", "log"],
                &[
                    "function log(self, r: Record): Future<null>",
                    "function record(level: Level, message: str, module: str, fields: Map<str, str>): Future<Record> async",
                    "function log_record(r: Record): Future<null> async",
                    "function trace(message: str): Future<null> async",
                    "function debug(message: str): Future<null> async",
                    "function info(message: str): Future<null> async",
                    "function warn(message: str): Future<null> async",
                    "function error(message: str): Future<null> async",
                    "function trace_with(message: str, fields: Map<str, str>): Future<null> async",
                    "function debug_with(message: str, fields: Map<str, str>): Future<null> async",
                    "function info_with(message: str, fields: Map<str, str>): Future<null> async",
                    "function warn_with(message: str, fields: Map<str, str>): Future<null> async",
                    "function error_with(message: str, fields: Map<str, str>): Future<null> async",
                ],
            ),
        ];

        for (path, snippets) in cases {
            let source = source_for(path);
            for snippet in *snippets {
                assert!(
                    source.contains(snippet),
                    "{} must keep wait-capable public signature `{snippet}`",
                    display_module_path(path)
                );
            }
        }

        let forbidden_cases: &[(&[&str], &[&str])] = &[
            (
                &["std", "fs"],
                &[
                    "function read_to_string(path: Path): str | IoError",
                    "function write_string(path: Path, contents: str): null | IoError",
                    "function append_string(path: Path, contents: str): null | IoError",
                    "function read(path: Path): Bytes | IoError",
                    "function write(path: Path, data: Bytes): null | IoError",
                    "function read_dir(path: Path): DirEntries | IoError",
                    "function canonicalize(path: Path): Path | IoError",
                    "function open(path: Path): File | IoError",
                    "function close(self): null | IoError",
                ],
            ),
            (
                &["std", "net"],
                &[
                    "function resolve(host: str): List<IpAddr> | IoError",
                    "function connect(addr: SocketAddr): TcpStream | IoError",
                    "function connect_timeout(addr: SocketAddr, timeout: Duration): TcpStream | IoError",
                    "function bind(addr: SocketAddr): TcpListener | IoError",
                    "function accept(self): (TcpStream, SocketAddr) | IoError",
                    "function bind(addr: SocketAddr): UdpSocket | IoError",
                    "function recv(self, buf: Bytes): i64 | IoError",
                    "function recv_from(self, buf: Bytes): (i64, SocketAddr) | IoError",
                    "function close(self): null | IoError",
                ],
            ),
            (
                &["std", "process"],
                &[
                    "function args(): List<str>",
                    "function env(name: str): str | null",
                    "function env_all(): Map<str, str>",
                    "function status(self): ExitStatus | IoError",
                    "function output(self): Output | IoError",
                    "function spawn(self): Child | IoError",
                    "function wait(self): ExitStatus | IoError",
                    "function kill(self): null | IoError",
                ],
            ),
            (
                &["std", "time"],
                &[
                    "function sleep(duration: Duration): null",
                    "function now(): Instant",
                    "function now(): SystemTime",
                    "function now_utc(): DateTime",
                    "function now_local(): DateTime",
                ],
            ),
            (
                &["std", "async"],
                &[
                    "function yield_now(): null",
                    "function sleep(ms: i64): null",
                    "function timeout<T>(future: Future<T>, ms: i64): T | TimedOut",
                ],
            ),
            (
                &["std", "rand"],
                &[
                    "function os_bytes(count: i64): Bytes | RandomError",
                    "function thread_rng(): ThreadRng | RandomError",
                    "function try_next_u32(self): u32 | RandomError",
                    "function try_next_u64(self): u64 | RandomError",
                ],
            ),
            (
                &["std", "hash"],
                &["function os_keyed_hasher(): KeyedHasher | HashSeedError"],
            ),
        ];

        for (path, snippets) in forbidden_cases {
            let source = source_for(path);
            for snippet in *snippets {
                assert!(
                    !source.contains(snippet),
                    "{} must not expose old direct-result wait signature `{snippet}`",
                    display_module_path(path)
                );
            }
        }
    }

    #[test]
    fn operator_protocol_labels_are_not_catalog_exports() {
        let core_prelude = TOOLCHAIN_MODULES
            .iter()
            .find(|spec| spec.path == ["core", "prelude"])
            .expect("core:prelude catalog entry");
        for label in [
            "Add", "Sub", "Mul", "Div", "Mod", "Neg", "Not", "BitAnd", "BitOr", "BitXor", "Shl",
            "Shr", "Index", "IndexMut",
        ] {
            assert!(
                !core_prelude.exports.contains(&label),
                "{label} is currently a checker-recognized operator protocol label, not a core:prelude export"
            );
        }
    }

    #[test]
    fn builtin_provider_resolves_exact_paths() {
        let provider = active_provider();
        assert_eq!(provider.name(), "builtin");
        let io = provider
            .module(&["std".to_string(), "io".to_string()])
            .unwrap();
        assert_eq!(io.tier, StdTier::Std);
        assert_eq!(io.implementation, StdImplementation::Mixed);
        let bytes = provider
            .module(&["std".to_string(), "bytes".to_string()])
            .unwrap();
        assert_eq!(bytes.tier, StdTier::Std);
        assert_eq!(bytes.implementation, StdImplementation::Otter);
        let time = provider
            .module(&["std".to_string(), "time".to_string()])
            .unwrap();
        assert_eq!(time.tier, StdTier::Std);
        assert_eq!(time.implementation, StdImplementation::Mixed);
        let fs = provider
            .module(&["std".to_string(), "fs".to_string()])
            .unwrap();
        assert_eq!(fs.tier, StdTier::Std);
        assert_eq!(fs.implementation, StdImplementation::Mixed);
        let error = provider
            .module(&["std".to_string(), "error".to_string()])
            .unwrap();
        assert_eq!(error.tier, StdTier::Std);
        assert_eq!(error.implementation, StdImplementation::Otter);
        let atomic = provider
            .module(&["core".to_string(), "sync".to_string(), "atomic".to_string()])
            .unwrap();
        assert_eq!(atomic.tier, StdTier::Core);
        assert_eq!(atomic.implementation, StdImplementation::Otter);
        assert!(atomic.exports.contains(&"Ordering"));
        assert!(provider
            .module(&["std".to_string(), "sync".to_string(), "atomic".to_string()])
            .is_none());
        assert!(provider
            .module(&["core".to_string(), "error".to_string()])
            .is_none());
        assert!(provider
            .module(&["std".to_string(), "missing".to_string()])
            .is_none());
    }
}
