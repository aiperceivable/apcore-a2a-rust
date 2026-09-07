use std::process::ExitCode;

/// Exit codes follow the convention clap already applies to the usage errors it
/// catches, so that one class of mistake carries one code whoever noticed it:
///
/// * `0` — clean shutdown
/// * `1` — configuration or runtime error: the command line was well-formed, the
///   environment it named was not. A supervisor may retry these.
/// * `2` — usage error: the command line itself is wrong. Retrying it unchanged
///   will always fail.
///
/// Returning `Result` from `main` would collapse 1 and 2, because Rust's runtime
/// exits 1 for any `Err` — which is how `apcore-a2a --bogus` came to exit 2
/// (clap) while `apcore-a2a` with no backend source exited 1 (this crate).
fn main() -> ExitCode {
    match apcore_a2a::cli::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Error: {error}");
            if error
                .downcast_ref::<apcore_a2a::cli::UsageError>()
                .is_some()
            {
                ExitCode::from(2)
            } else {
                ExitCode::FAILURE
            }
        }
    }
}
