//! CLI entrypoint for apcore-a2a.

use clap::Parser;
#[cfg(feature = "openapi")]
use serde_json::Value;

#[derive(Parser, Debug)]
#[command(name = "apcore-a2a", version = crate::VERSION, about = "A2A protocol adapter for apcore")]
pub struct Cli {
    /// Path to an apcore extensions directory.
    ///
    /// Optional since the OpenAPI backend gave it an alternative; a backend
    /// source has to come from `--extensions-dir`, `--from-openapi` or an
    /// `apcore-a2a.openapi.spec` in the apcore config, and combining an
    /// extensions directory with an OpenAPI document also requires a prefix.
    #[arg(short, long)]
    pub extensions_dir: Option<String>,

    #[arg(short, long, default_value = "apcore-a2a")]
    pub name: String,

    /// Public base URL published in the Agent Card. Defaults to
    /// `http://localhost:<port>` — it follows `--port` rather than pinning 8000,
    /// so the card cannot advertise a socket the server does not bind.
    #[arg(long)]
    pub url: Option<String>,

    #[arg(short, long, default_value_t = 8000)]
    pub port: u16,

    /// OpenAPI 3.0/3.1 spec URL or path; every operation becomes an A2A Skill.
    #[cfg(feature = "openapi")]
    #[arg(long, value_name = "URL_OR_PATH")]
    pub from_openapi: Option<String>,

    /// Base URL for proxied requests. Defaults to the document's `servers[0].url`.
    #[cfg(feature = "openapi")]
    #[arg(long, value_name = "URL")]
    pub openapi_base_url: Option<String>,

    /// `base_path_prefix` prepended to every derived module ID.
    #[cfg(feature = "openapi")]
    #[arg(long, value_name = "PREFIX")]
    pub openapi_prefix: Option<String>,

    /// Scanner include filter.
    #[cfg(feature = "openapi")]
    #[arg(long, value_name = "GLOB")]
    pub openapi_include: Option<String>,

    /// Scanner exclude filter.
    #[cfg(feature = "openapi")]
    #[arg(long, value_name = "GLOB")]
    pub openapi_exclude: Option<String>,

    /// Header for the spec fetch only; repeatable. Never sent on proxied calls.
    #[cfg(feature = "openapi")]
    #[arg(long, value_name = "KEY:VALUE")]
    pub openapi_header: Vec<String>,

    /// Skip operations marked `deprecated: true`.
    ///
    /// `Option<bool>` rather than a plain clap `bool`, so that **absent** stays
    /// distinguishable from **passed**. As a `bool` the two collapse into
    /// `false`, and [`merge_openapi_settings`] would then write
    /// `include_deprecated: true` on every run — an absent flag silently
    /// reversing an `include_deprecated: false` the operator wrote in the
    /// config file. `num_args = 0` keeps it a bare flag (it never consumes the
    /// next token) and `default_missing_value` is what makes the bare form
    /// `Some(true)`; note that `action = SetTrue` would NOT work here, because
    /// clap re-applies that action's own `default_value("false")` during
    /// `Arg::_build` whenever the default list is empty, yielding `Some(false)`
    /// for an absent flag.
    #[cfg(feature = "openapi")]
    #[arg(long, num_args = 0, default_missing_value = "true")]
    pub openapi_no_deprecated: Option<bool>,
}

/// Parse repeated `--openapi-header KEY:VALUE` flags into a header map.
///
/// The values are credentials for the spec fetch, so a malformed one is named
/// by its **key half only** — echoing the whole argument back would put the
/// secret in the terminal scrollback and in any CI log capturing stderr.
#[cfg(feature = "openapi")]
fn parse_headers(
    raw: &[String],
) -> Result<Option<std::collections::HashMap<String, String>>, String> {
    if raw.is_empty() {
        return Ok(None);
    }
    let mut headers = std::collections::HashMap::new();
    for item in raw {
        let Some((key, value)) = item.split_once(':') else {
            return Err(format!(
                "--openapi-header expects KEY:VALUE, got {} value(s) of which one has no ':'",
                raw.len()
            ));
        };
        if key.trim().is_empty() {
            return Err("--openapi-header expects a non-empty KEY before ':'".to_string());
        }
        headers.insert(key.trim().to_string(), value.trim().to_string());
    }
    Ok(Some(headers))
}

/// Which backend source(s) the flags select, after validation.
#[derive(Debug)]
enum Selected {
    Extensions(String),
    /// The merged `apcore-a2a.openapi` settings — see
    /// [`merge_openapi_settings`]. A settings **mapping** rather than a bare
    /// spec string, because the spec is only one of nine keys and the other
    /// eight have to survive the trip.
    #[cfg(feature = "openapi")]
    OpenApi(Value),
    #[cfg(feature = "openapi")]
    Both {
        extensions_dir: String,
        openapi: Value,
    },
}

/// Whether a merged settings mapping actually names a spec.
///
/// The same three-part filter [`crate::openapi_backend`]'s config route applies:
/// absent, `null` and set-but-empty all mean "no spec here" (FR-OAS-004 AC 2).
/// An inline document (a mapping) is a legitimate spec value.
#[cfg(feature = "openapi")]
fn names_a_spec(merged: &serde_json::Map<String, Value>) -> bool {
    match merged.get("spec") {
        None | Some(Value::Null) => false,
        Some(Value::String(s)) => !s.trim().is_empty(),
        Some(_) => true,
    }
}

/// The `apcore-a2a.openapi` Config Bus section, or `None` when it is unset.
///
/// The namespace is registered first: `A2AServerFactory::new` does it too, but
/// that runs long after this read, and without the registration the
/// `APCORE_A2A_OPENAPI_*` environment overrides and the namespace's own defaults
/// are not in play.
#[cfg(feature = "openapi")]
fn openapi_config_section() -> Option<Value> {
    crate::server::factory::register_a2a_namespace();
    let config = match apcore::config::Config::discover() {
        Ok(config) => config,
        Err(error) => {
            // Not fatal: with no readable config there is simply no Config Bus
            // section, and the CLI flags alone still describe a backend.
            tracing::debug!(%error, "could not read the apcore config; ignoring apcore-a2a.openapi");
            return None;
        }
    };
    // The namespace registers `openapi: null` as its default, so an unset
    // section arrives as `Some(Value::Null)` rather than as `None`.
    config
        .get("apcore-a2a.openapi")
        .filter(|value| !value.is_null())
}

/// Resolve the OpenAPI backend settings, CLI flags over Config Bus.
///
/// Implements the precedence the feature spec states: **an explicit CLI flag
/// beats the `apcore-a2a.openapi` Config Bus section, which beats the default.**
/// Per key, not per source — a `--openapi-prefix` alongside a config-declared
/// `spec` has to take effect, or the flag is a silent no-op, which is the shape
/// of the apcore-mcp `--openapi-header` defect this binding filed upstream.
///
/// Returns `Ok(None)` when neither route names a `spec`: the ordinary "no
/// OpenAPI configured" outcome rather than an error.
///
/// # Errors
///
/// Returns the `parse_headers` message when a `--openapi-header` is malformed.
/// (The Python reference reports the same condition by exiting the process from
/// inside its merge; a `Result` is the same contract without the `sys.exit`.)
#[cfg(feature = "openapi")]
pub fn merge_openapi_settings(cli: &Cli) -> Result<Option<Value>, String> {
    merge_openapi_settings_with(openapi_config_section(), cli)
}

/// [`merge_openapi_settings`] with the Config Bus section injected.
///
/// Split out so the merge is unit-testable without a config file on disk —
/// the Rust equivalent of the Python reference's `monkeypatch` of
/// `get_a2a_setting`.
#[cfg(feature = "openapi")]
fn merge_openapi_settings_with(section: Option<Value>, cli: &Cli) -> Result<Option<Value>, String> {
    let mut merged = match section {
        Some(Value::Object(map)) => map,
        // A section that is not a mapping is passed through UNCHANGED so that
        // `build_openapi_backend_from_config` reports it by name
        // ("apcore-a2a.openapi must be a mapping, got string"). The Python
        // reference silently discards it instead, which turns a typo like
        // `openapi: ./spec.json` into "a backend source is required".
        Some(other) => return Ok(Some(other)),
        None => serde_json::Map::new(),
    };

    // Only overlay a flag the operator actually passed. Every `--openapi-*`
    // flag is an `Option`, so "absent" stays distinguishable from "set to a
    // falsy value" — see `Cli::openapi_no_deprecated` for why that matters.
    for (key, value) in [
        ("spec", cli.from_openapi.as_deref()),
        ("base_url", cli.openapi_base_url.as_deref()),
        ("prefix", cli.openapi_prefix.as_deref()),
        ("include", cli.openapi_include.as_deref()),
        ("exclude", cli.openapi_exclude.as_deref()),
    ] {
        if let Some(value) = value {
            merged.insert(key.to_string(), Value::String(value.to_string()));
        }
    }

    // `headers` is one key, so the flags replace the configured map rather than
    // merging into it — the same granularity every other key gets.
    if let Some(headers) = parse_headers(&cli.openapi_header)? {
        merged.insert(
            "headers".to_string(),
            Value::Object(
                headers
                    .into_iter()
                    .map(|(k, v)| (k, Value::String(v)))
                    .collect(),
            ),
        );
    }

    if let Some(no_deprecated) = cli.openapi_no_deprecated {
        merged.insert(
            "include_deprecated".to_string(),
            Value::Bool(!no_deprecated),
        );
    }

    // Neither route named a `spec`: the ordinary "no OpenAPI configured"
    // outcome, not an error.
    Ok(names_a_spec(&merged).then(|| Value::Object(merged)))
}

/// A fault in the command line itself, as opposed to a fault in the environment
/// it names.
///
/// Carried as its own type so `main` can exit **2** for it, which is what clap
/// already does for the usage errors it catches (an unknown flag, a missing
/// value). Before this existed, `apcore-a2a --bogus` exited 2 while
/// `apcore-a2a` with no source exited 1 — the same class of mistake, two codes,
/// decided by whether clap or this module noticed it first.
///
/// The distinction is worth carrying: a supervisor may retry exit 1, because the
/// command was well-formed and the world might become ready. Exit 2 will never
/// succeed on retry — the fix is in the command line.
#[derive(Debug)]
pub struct UsageError(pub String);

impl std::fmt::Display for UsageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for UsageError {}

/// At least one backend source is required, and a mixed deployment needs a
/// prefix (FR-OAS-006): the scanner deduplicates within one scan only and knows
/// nothing about modules already in the registry.
#[cfg(feature = "openapi")]
fn select_source(cli: &Cli, openapi: Option<Value>) -> Result<Selected, String> {
    match (cli.extensions_dir.as_deref(), openapi) {
        (None, None) => Err("a backend source is required — pass --extensions-dir or \
                             --from-openapi, or set apcore-a2a.openapi.spec in your apcore config"
            .to_string()),
        (Some(dir), None) => Ok(Selected::Extensions(dir.to_string())),
        (None, Some(openapi)) => Ok(Selected::OpenApi(openapi)),
        (Some(dir), Some(openapi)) => {
            // Read from the MERGED settings, not from the flag: an
            // `apcore-a2a.openapi.prefix` in the config file satisfies
            // FR-OAS-006 exactly as `--openapi-prefix` does.
            let prefixed = match openapi.as_object() {
                Some(map) => map
                    .get("prefix")
                    .and_then(Value::as_str)
                    .is_some_and(|prefix| !prefix.trim().is_empty()),
                // Not a mapping: `options_from_config` owns that message, and a
                // prefix complaint here would mask it.
                None => true,
            };
            if !prefixed {
                return Err(
                    "--openapi-prefix is required when --extensions-dir and an OpenAPI document \
                     are combined: without it a derived module ID can collide with a project \
                     module ID. Set --openapi-prefix or apcore-a2a.openapi.prefix"
                        .to_string(),
                );
            }
            Ok(Selected::Both {
                extensions_dir: dir.to_string(),
                openapi,
            })
        }
    }
}

/// At least one backend source is required. Without the `openapi` feature there
/// is only one to choose from.
#[cfg(not(feature = "openapi"))]
fn select_source(cli: &Cli) -> Result<Selected, String> {
    cli.extensions_dir
        .as_deref()
        .map(|dir| Selected::Extensions(dir.to_string()))
        .ok_or_else(|| {
            "a backend source is required — pass --extensions-dir (rebuild with \
             `--features openapi` for --from-openapi)"
                .to_string()
        })
}

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    // The backend source may come from the Config Bus, so the usage check has
    // to consult it — otherwise a valid `apcore-a2a.openapi.spec` in the config
    // file would be rejected for naming no flag.
    // `select_source` faults are usage faults — the command line named no source,
    // or combined two without a prefix — so they carry `UsageError` and reach the
    // operator as exit 2, the same code clap gives for its own usage errors.
    #[cfg(feature = "openapi")]
    let selected = select_source(&cli, merge_openapi_settings(&cli)?).map_err(UsageError)?;
    #[cfg(not(feature = "openapi"))]
    let selected = select_source(&cli).map_err(UsageError)?;

    let config = crate::APCoreA2AConfig {
        name: cli.name.clone(),
        // `--port` was parsed and then dropped by `..Default::default()`, so it
        // was inert and the server always bound 8000 whatever the operator
        // typed. The published URL follows it, so the two cannot disagree.
        port: cli.port,
        url: cli
            .url
            .clone()
            .unwrap_or_else(|| format!("http://localhost:{}", cli.port)),
        ..Default::default()
    };

    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async move {
        let source = match selected {
            Selected::Extensions(dir) => crate::BackendSource::from(dir.as_str()),
            #[cfg(feature = "openapi")]
            Selected::OpenApi(openapi) => {
                // Routed through the Config-Bus builder rather than through
                // `openapi_backend_from_spec` so the whole documented key set —
                // including `timeout` and `acknowledge_unapproved_writes`, which
                // have no flag in any SDK — reaches the fetch and the scan. It
                // resolves `Config::project_root` itself (FR-OAS-004 AC 5), so
                // the CLI must not duplicate that.
                //
                // `governance_state` is `None`: the executor does not exist yet
                // — this call builds the registry it is constructed from — so
                // FR-OAS-005's escalated tier is answered at serve time by
                // FR-AGC-007 instead. Threading the parameter is what makes it
                // reachable for an embedder that *does* hold one.
                let registry = crate::openapi_backend::build_openapi_backend_from_config(
                    &openapi,
                    std::sync::Arc::new(apcore::registry::registry::Registry::new()),
                    false,
                    None,
                )
                .await?
                // `select_source` only yields this arm when the merged settings
                // name a `spec`, so the absent case cannot reach here — but it
                // is a message, not an `unwrap`, if that ever stops holding.
                .ok_or(
                    "apcore-a2a.openapi named a spec but resolved to no backend; \
                     set apcore-a2a.openapi.spec",
                )?;
                crate::BackendSource::Registry(registry)
            }
            #[cfg(feature = "openapi")]
            Selected::Both {
                extensions_dir,
                openapi,
            } => {
                // One registry, both sources: discovery first, then the scan
                // writes into the same registry so the collision preflight can
                // see the project modules it must not shadow.
                let registry =
                    crate::apcore_a2a::discover_extensions(std::path::Path::new(&extensions_dir))
                        .await?;
                // `governance_state` is `None` for the same reason as the
                // OpenApi-only arm above: the executor does not exist yet.
                let registry = crate::openapi_backend::build_openapi_backend_from_config(
                    &openapi, registry, true, None,
                )
                .await?
                // `select_source` only yields this arm when the merged settings
                // name a `spec`, so the absent case cannot reach here — but it
                // is a message, not an `unwrap`, if that ever stops holding.
                .ok_or(
                    "apcore-a2a.openapi named a spec but resolved to no backend; \
                     set apcore-a2a.openapi.spec",
                )?;
                crate::BackendSource::Registry(registry)
            }
        };
        crate::async_serve(source, config).await?;
        Ok::<(), Box<dyn std::error::Error>>(())
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "openapi")]
    use serde_json::json;

    fn parse(args: &[&str]) -> Cli {
        Cli::parse_from(args)
    }

    /// The merged settings for `args`, with `section` standing in for the
    /// `apcore-a2a.openapi` Config Bus section. Never touches a config file, so
    /// the result does not depend on where the suite happens to run.
    #[cfg(feature = "openapi")]
    fn merge(section: Option<Value>, args: &[&str]) -> Option<Value> {
        merge_openapi_settings_with(section, &parse(args)).expect("merge")
    }

    #[cfg(feature = "openapi")]
    fn selected(section: Option<Value>, args: &[&str]) -> Result<Selected, String> {
        select_source(&parse(args), merge(section, args))
    }

    /// The selected source for `args` with an empty Config Bus, in both feature
    /// configurations.
    #[cfg(feature = "openapi")]
    fn source_of(args: &[&str]) -> Result<Selected, String> {
        selected(None, args)
    }

    #[cfg(not(feature = "openapi"))]
    fn source_of(args: &[&str]) -> Result<Selected, String> {
        select_source(&parse(args))
    }

    #[test]
    fn port_reaches_the_server_config() {
        // `--port` used to be parsed and then discarded by `..Default::default()`,
        // so the server bound 0.0.0.0:8000 whatever the operator typed and
        // nothing said so.
        let cli = parse(&["apcore-a2a", "--extensions-dir", "./ext", "--port", "9123"]);
        assert_eq!(cli.port, 9123);
        let config = crate::APCoreA2AConfig {
            port: cli.port,
            url: cli
                .url
                .clone()
                .unwrap_or_else(|| format!("http://localhost:{}", cli.port)),
            ..Default::default()
        };
        assert_eq!(crate::bind_addr(&config).unwrap(), "0.0.0.0:9123");
        // ... and the Agent Card cannot advertise a socket the server does not bind.
        assert_eq!(config.url, "http://localhost:9123");
    }

    #[test]
    fn explicit_url_still_wins_over_the_derived_one() {
        let cli = parse(&[
            "apcore-a2a",
            "--extensions-dir",
            "./ext",
            "--port",
            "9123",
            "--url",
            "https://agents.example.com/a2a",
        ]);
        assert_eq!(cli.url.as_deref(), Some("https://agents.example.com/a2a"));
    }

    #[test]
    fn a_backend_source_is_required() {
        let err = source_of(&["apcore-a2a"]).expect_err("no source");
        assert!(err.contains("backend source is required"), "{err}");
    }

    #[test]
    fn a_missing_backend_source_is_a_usage_fault_not_a_runtime_one() {
        // This crate used to return a plain `Box<dyn Error>` here, and `main`
        // returning `Result` made Rust's runtime exit 1 for it — while clap
        // exited 2 for *its* usage errors, so `apcore-a2a --bogus` and
        // `apcore-a2a` with no source reported the same class of mistake with two
        // different codes depending on which layer noticed first.
        //
        // The marker type is what `main` downcasts to choose exit 2, so the type
        // is the contract; asserting the message alone would not have caught the
        // divergence.
        let err = source_of(&["apcore-a2a"]).expect_err("no source");
        let boxed: Box<dyn std::error::Error> = Box::new(UsageError(err));
        assert!(
            boxed.downcast_ref::<UsageError>().is_some(),
            "select_source faults must be downcastable to UsageError, or main \
             cannot tell a usage fault from a runtime one"
        );
    }

    #[test]
    fn a_usage_error_renders_its_message_unchanged() {
        // `main` prints this straight to stderr, so a Display impl that added a
        // prefix would double up with main's own "Error: ".
        let err = UsageError("no backend source".to_string());
        assert_eq!(format!("{err}"), "no backend source");
    }

    #[test]
    fn extensions_dir_alone_is_enough() {
        assert!(matches!(
            source_of(&["apcore-a2a", "--extensions-dir", "./ext"]),
            Ok(Selected::Extensions(_))
        ));
    }

    #[cfg(feature = "openapi")]
    #[test]
    fn from_openapi_alone_is_enough() {
        assert!(matches!(
            source_of(&[
                "apcore-a2a",
                "--from-openapi",
                "https://api.example.com/openapi.json",
            ]),
            Ok(Selected::OpenApi(_))
        ));
    }

    #[cfg(feature = "openapi")]
    #[test]
    fn mixing_sources_requires_a_prefix() {
        let err = source_of(&[
            "apcore-a2a",
            "--extensions-dir",
            "./ext",
            "--from-openapi",
            "https://api.example.com/openapi.json",
        ])
        .expect_err("prefix required");
        assert!(err.contains("--openapi-prefix is required"), "{err}");

        assert!(matches!(
            source_of(&[
                "apcore-a2a",
                "--extensions-dir",
                "./ext",
                "--from-openapi",
                "https://api.example.com/openapi.json",
                "--openapi-prefix",
                "petstore",
            ]),
            Ok(Selected::Both { .. })
        ));

        // ... and a prefix the CONFIG declares satisfies FR-OAS-006 just as the
        // flag does: the preflight reads the merged settings, not the flag.
        assert!(matches!(
            selected(
                Some(json!({ "prefix": "petstore" })),
                &[
                    "apcore-a2a",
                    "--extensions-dir",
                    "./ext",
                    "--from-openapi",
                    "https://api.example.com/openapi.json",
                ],
            ),
            Ok(Selected::Both { .. })
        ));
    }

    #[cfg(feature = "openapi")]
    #[test]
    fn headers_reach_the_backend_options() {
        // The flag is not a no-op: apcore-mcp's Rust CLI parses the identical
        // flag into a map it never passes to `load_spec_with_options`.
        let merged = merge(
            None,
            &[
                "apcore-a2a",
                "--from-openapi",
                "https://api.example.com/openapi.json",
                "--openapi-header",
                "X-Api-Key: secret",
                "--openapi-header",
                "X-Tenant:acme",
            ],
        )
        .expect("a spec was given");
        assert_eq!(merged["headers"]["X-Api-Key"], json!("secret"));
        assert_eq!(merged["headers"]["X-Tenant"], json!("acme"));
    }

    #[cfg(feature = "openapi")]
    #[test]
    fn malformed_header_is_rejected_without_echoing_the_value() {
        let cli = parse(&[
            "apcore-a2a",
            "--from-openapi",
            "https://api.example.com/openapi.json",
            "--openapi-header",
            "supersecrettoken",
        ]);
        let err = merge_openapi_settings_with(None, &cli).expect_err("malformed header");
        assert!(err.contains("KEY:VALUE"), "{err}");
        assert!(!err.contains("supersecrettoken"), "{err}");
    }

    // ----------------------------------------------------------------------
    // Config Bus wiring (FR-OAS-004 AC 5) — the route must be reachable at all
    // ----------------------------------------------------------------------

    /// The `apcore-a2a.openapi` section must be readable with no CLI flag at all.
    ///
    /// Before this wiring `build_openapi_backend_from_config` had no caller
    /// anywhere in `src/`, so `timeout`, `include`, `exclude` and
    /// `acknowledge_unapproved_writes` — none of which has a CLI flag in any
    /// SDK — were unreachable through every live path.
    #[cfg(feature = "openapi")]
    #[test]
    fn the_config_bus_alone_supplies_a_backend_source() {
        let section = json!({ "spec": "./from-config.json", "timeout": 5.0, "prefix": "cfg" });
        let merged = merge(Some(section.clone()), &["apcore-a2a"])
            .expect("the Config Bus section was not consulted");
        assert_eq!(merged["spec"], json!("./from-config.json"));
        assert_eq!(
            merged["timeout"],
            json!(5.0),
            "a key with no CLI flag must survive the merge"
        );
        assert_eq!(merged["prefix"], json!("cfg"));

        // ... and it counts as a backend source, so `apcore-a2a` with no flag at
        // all is no longer a usage error.
        assert!(matches!(
            selected(Some(section), &["apcore-a2a"]),
            Ok(Selected::OpenApi(_))
        ));
    }

    /// Per key, not per source.
    ///
    /// Choosing the whole source by whoever named `spec` would make
    /// `--openapi-prefix` a silent no-op alongside a config-declared spec — the
    /// same shape as the apcore-mcp `--openapi-header` defect this binding filed
    /// upstream (apcore-mcp-rust#8).
    #[cfg(feature = "openapi")]
    #[test]
    fn an_explicit_flag_beats_the_config_bus_per_key() {
        let merged = merge(
            Some(json!({ "spec": "./from-config.json", "prefix": "cfg", "timeout": 5.0 })),
            &["apcore-a2a", "--openapi-prefix", "from-flag"],
        )
        .expect("a spec was configured");
        assert_eq!(merged["prefix"], json!("from-flag"), "the flag must win");
        assert_eq!(
            merged["spec"],
            json!("./from-config.json"),
            "config keys with no flag survive"
        );
        assert_eq!(merged["timeout"], json!(5.0));
    }

    /// `--openapi-no-deprecated` is an `Option<bool>`, not a clap `bool`.
    ///
    /// With a plain `bool`, simply *not passing* the flag would overwrite a
    /// config `include_deprecated: false` with `true` — an absent flag silently
    /// reversing a setting the operator wrote.
    #[cfg(feature = "openapi")]
    #[test]
    fn an_absent_no_deprecated_flag_does_not_override_the_config() {
        let section = json!({ "spec": "./s.json", "include_deprecated": false });

        let absent = merge(Some(section.clone()), &["apcore-a2a"]).expect("configured");
        assert_eq!(
            absent["include_deprecated"],
            json!(false),
            "an absent flag must not override"
        );

        let given =
            merge(Some(section), &["apcore-a2a", "--openapi-no-deprecated"]).expect("configured");
        assert_eq!(given["include_deprecated"], json!(false));

        // The flag still does its job where the config is silent, and absence
        // still leaves the key unset so the scanner default (`true`) applies.
        let flipped = merge(
            Some(json!({ "spec": "./s.json" })),
            &["apcore-a2a", "--openapi-no-deprecated"],
        )
        .expect("configured");
        assert_eq!(flipped["include_deprecated"], json!(false));
        let untouched =
            merge(Some(json!({ "spec": "./s.json" })), &["apcore-a2a"]).expect("configured");
        assert!(untouched.get("include_deprecated").is_none());
    }

    #[cfg(feature = "openapi")]
    #[test]
    fn no_spec_anywhere_is_not_an_openapi_source() {
        assert!(merge(None, &["apcore-a2a"]).is_none());
        // A section with no `spec` is equally not a source.
        assert!(merge(Some(json!({ "prefix": "p" })), &["apcore-a2a"]).is_none());
        // ... and neither is a set-but-empty one (FR-OAS-004 AC 2).
        assert!(merge(Some(json!({ "spec": "   " })), &["apcore-a2a"]).is_none());
        assert!(merge(Some(json!({ "spec": null })), &["apcore-a2a"]).is_none());
    }

    #[cfg(feature = "openapi")]
    #[test]
    fn no_deprecated_flag_flips_the_scanner_default() {
        // Absent, the key is left unset and the scanner default (`true`) stands;
        // `options_from_config` is what turns "unset" into `true`.
        assert!(parse(&["apcore-a2a", "--from-openapi", "s.json"])
            .openapi_no_deprecated
            .is_none());

        let cli = parse(&[
            "apcore-a2a",
            "--from-openapi",
            "https://api.example.com/openapi.json",
            "--openapi-no-deprecated",
        ]);
        assert_eq!(cli.openapi_no_deprecated, Some(true));
        let merged = merge_openapi_settings_with(None, &cli)
            .expect("merge")
            .expect("a spec was given");
        assert_eq!(merged["include_deprecated"], json!(false));
    }
}
