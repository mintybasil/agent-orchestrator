//! Self-invoking ASKPASS handler for git authentication.
//!
//! When git needs credentials, it re-invokes the binary with a prompt string
//! as `argv[1]`. This module reads the token from `AO_GIT_TOKEN` and prints
//! the appropriate credential to stdout.
//!
//! Git passes prompts like:
//! - `Username for 'https://github.com': `
//! - `Password for 'https://x-access-token@github.com': `
//!
//! We respond with `x-access-token` for username prompts and the token for
//! password prompts.

/// Environment variable sentinel: when set, the binary enters askpass mode
/// instead of its normal execution path.
pub const ASKPASS_MODE_ENV: &str = "AO_ASKPASS_MODE";

/// Environment variable holding the GitHub token for askpass mode.
pub const GIT_TOKEN_ENV: &str = "AO_GIT_TOKEN";

/// Run the askpass handler. Returns the process exit code.
///
/// - If `AO_GIT_TOKEN` is missing or empty, returns 1 (git treats this as
///   auth failure, and `GIT_TERMINAL_PROMPT=0` prevents hanging).
/// - If `argv[1]` contains "Username" (case-insensitive), prints
///   `x-access-token` and returns 0.
/// - Otherwise (password prompt), prints the token and returns 0.
pub fn run(args: &[String]) -> i32 {
    let token = match std::env::var(GIT_TOKEN_ENV) {
        Ok(t) if !t.is_empty() => t,
        _ => return 1,
    };

    let prompt = args.get(1).map(|s| s.as_str()).unwrap_or("");

    if prompt.to_ascii_lowercase().contains("username") {
        print!("x-access-token");
    } else {
        print!("{token}");
    }

    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Tests that modify `AO_GIT_TOKEN` must not run concurrently because
    /// `std::env::set_var` / `remove_var` operate on the process-wide
    /// environment. This mutex serialises all askpass tests without
    /// penalising the rest of the test suite.
    static ENV_MUTEX: Mutex<()> = Mutex::new(());

    fn set_token(val: &str) {
        unsafe {
            std::env::set_var(GIT_TOKEN_ENV, val);
        }
    }

    fn remove_token() {
        unsafe {
            std::env::remove_var(GIT_TOKEN_ENV);
        }
    }

    #[test]
    fn askpass_returns_username_for_username_prompt() {
        let _lock = ENV_MUTEX.lock().unwrap();
        set_token("ghp_test123");
        let exit_code = run(&[
            "agent-orchestrator".into(),
            "Username for 'https://github.com': ".into(),
        ]);
        assert_eq!(exit_code, 0);
        remove_token();
    }

    #[test]
    fn askpass_returns_token_for_password_prompt() {
        let _lock = ENV_MUTEX.lock().unwrap();
        set_token("ghp_test123");
        let exit_code = run(&[
            "agent-orchestrator".into(),
            "Password for 'https://github.com': ".into(),
        ]);
        assert_eq!(exit_code, 0);
        remove_token();
    }

    #[test]
    fn askpass_returns_token_for_generic_prompt() {
        let _lock = ENV_MUTEX.lock().unwrap();
        set_token("ghp_test123");
        let exit_code = run(&[
            "agent-orchestrator".into(),
            "Password for 'https://x-access-token@github.com': ".into(),
        ]);
        assert_eq!(exit_code, 0);
        remove_token();
    }

    #[test]
    fn askpass_fails_without_token() {
        let _lock = ENV_MUTEX.lock().unwrap();
        remove_token();
        let exit_code = run(&[
            "agent-orchestrator".into(),
            "Username for 'https://github.com': ".into(),
        ]);
        assert_eq!(exit_code, 1);
    }

    #[test]
    fn askpass_fails_with_empty_token() {
        let _lock = ENV_MUTEX.lock().unwrap();
        set_token("");
        let exit_code = run(&[
            "agent-orchestrator".into(),
            "Username for 'https://github.com': ".into(),
        ]);
        assert_eq!(exit_code, 1);
        remove_token();
    }
}
