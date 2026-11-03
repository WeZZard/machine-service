//! The dispatcher's token, from the VM host's login keychain (LR-4,
//! DC-1xx-EPH-3). It is read into memory for each round and never printed,
//! logged, written or sent anywhere but GitHub's API.

use std::process::{Command, Stdio};

/// The keychain item's service.
pub const SERVICE: &str = "com.wezzard.vm-service.lab.github-token";
/// The keychain item's account: the repository the token is limited to.
pub const ACCOUNT: &str = crate::REPOSITORY;
/// macOS's only `security`, never one found on `PATH` (DC-239, DC-242).
pub const SECURITY: &str = "/usr/bin/security";

/// The command the owner runs once per VM host, at the Mac, to store or
/// replace the token; `security` prompts for it, so it is in no argument,
/// history or file.
pub fn store_command() -> String {
    format!("security add-generic-password -U -a {ACCOUNT} -s {SERVICE} -w")
}

/// The message when the item is missing.
pub fn missing_message() -> String {
    format!(
        "no GitHub token in the login keychain (service {SERVICE}); the owner stores it at this Mac \
with: {}",
        store_command()
    )
}

/// A token, which never shows in `Debug` or `Display`.
#[derive(Clone)]
pub struct Token(String);

impl Token {
    /// A token from its text, trimmed.
    pub fn new(text: &str) -> Option<Token> {
        let text = text.trim();
        (!text.is_empty()).then(|| Token(text.to_string()))
    }

    /// The token's text, for the one header that carries it.
    pub fn reveal(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Token {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Token([redacted])")
    }
}

/// Read the token: in a test build only from the seams' token file
/// ([`crate::seams`]), never the keychain; in a release from the login
/// keychain with `security`, whose output goes to memory only.
pub fn read() -> Result<Token, String> {
    if service_layout::test_build::is_test_build() {
        let seams = crate::seams::Seams::load()?;
        let path = seams.token.ok_or_else(missing_message)?;
        let text = std::fs::read_to_string(&path).map_err(|_| missing_message())?;
        return Token::new(&text).ok_or_else(missing_message);
    }
    let output = Command::new(SECURITY)
        .args(["find-generic-password", "-a", ACCOUNT, "-s", SERVICE, "-w"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(|error| format!("{SECURITY}: {error}"))?;
    if !output.status.success() {
        return Err(missing_message());
    }
    let text = String::from_utf8(output.stdout)
        .map_err(|_| "the keychain item is not text".to_string())?;
    Token::new(&text).ok_or_else(missing_message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_owner_is_told_the_exact_prompting_command() {
        assert_eq!(
            store_command(),
            "security add-generic-password -U -a WeZZard/vm-service -s com.wezzard.vm-service.lab.github-token -w"
        );
        assert!(store_command().ends_with(" -w"), "a trailing -w prompts");
        assert!(missing_message().contains(&store_command()));
    }

    #[test]
    fn a_token_never_shows() {
        let token = Token::new(" github_pat_x \n").unwrap();
        assert_eq!(token.reveal(), "github_pat_x");
        assert_eq!(format!("{token:?}"), "Token([redacted])");
        assert!(Token::new("  \n").is_none());
    }
}
