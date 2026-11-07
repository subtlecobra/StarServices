//! Password hashing and policy for web accounts.
//!
//! Hashes are argon2id PHC strings (`$argon2id$v=19$m=..,t=..,p=..$<salt>$<hash>`), so each carries its own random
//! salt and parameters. A login with a hash made under other parameters than the current ones gets a new hash, which
//! is how the parameters are raised. Hashing is slow on purpose, so it runs off the async threads and only a few at once.

use std::sync::Arc;

use argon2::{
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Algorithm, Argon2, Params, Version,
};
use ring::rand::{SecureRandom, SystemRandom};
use tokio::sync::Semaphore;
use tonic::Status;
use tracing::warn;

pub const MIN_LENGTH: usize = 10;
pub const MAX_LENGTH: usize = 128;
/// Hashes at once. Each takes the configured memory, 19 MiB by default.
const CONCURRENT_HASHES: usize = 4;
const SALT_BYTES: usize = 16;

/// Lower-case, one per line: whole passwords and the stems of common ones. A password is refused when it, or what is
/// left of it without trailing digits and symbols, is listed.
static COMMON: &str = include_str!("common_passwords.txt");

pub struct Hasher {
    params: Params,
    /// A hash of nothing anyone knows, verified for a login with no account, so that it takes as long as a real one.
    dummy: String,
    permits: Arc<Semaphore>,
}

/// The outcome of a verification.
pub struct Verified {
    pub ok: bool,
    /// The hash was made under other parameters, so it should be replaced.
    pub rehash: bool,
}

fn argon2(params: Params) -> Argon2<'static> {
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
}

fn hash_now(params: &Params, password: &[u8]) -> Result<String, String> {
    let mut salt = [0u8; SALT_BYTES];
    SystemRandom::new().fill(&mut salt).map_err(|_| "no system randomness".to_string())?;
    let salt = SaltString::encode_b64(&salt).map_err(|e| e.to_string())?;
    argon2(params.clone()).hash_password(password, &salt).map(|hash| hash.to_string()).map_err(|e| e.to_string())
}

impl Hasher {
    /// OWASP's argon2id minimum is 19 MiB, 2 passes, 1 lane.
    pub fn new(memory_kib: u32, iterations: u32, parallelism: u32) -> Result<Self, String> {
        let params = Params::new(memory_kib, iterations, parallelism, None).map_err(|e| format!("argon2 params: {e}"))?;
        let dummy = hash_now(&params, b"no account has this password")?;
        Ok(Self { params, dummy, permits: Arc::new(Semaphore::new(CONCURRENT_HASHES)) })
    }

    pub async fn hash(&self, password: &str) -> Result<String, Status> {
        let _permit = self.permits.acquire().await.map_err(|_| Status::unavailable("shutting down"))?;
        let params = self.params.clone();
        let password = password.as_bytes().to_vec();
        tokio::task::spawn_blocking(move || hash_now(&params, &password))
            .await
            .map_err(|_| Status::internal("hash failed"))?
            .map_err(|error| {
                warn!(%error, "password hash failed");
                Status::internal("hash failed")
            })
    }

    /// Checks `password` against `stored`, or against the dummy hash when there is none, so that both take as long.
    pub async fn verify(&self, password: &str, stored: Option<&str>) -> Result<Verified, Status> {
        let _permit = self.permits.acquire().await.map_err(|_| Status::unavailable("shutting down"))?;
        let real = stored.is_some();
        let stored = stored.unwrap_or(&self.dummy).to_string();
        let dummy = self.dummy.clone();
        let current = self.params.clone();
        let password = password.as_bytes().to_vec();
        tokio::task::spawn_blocking(move || {
            let (parsed, readable) = match PasswordHash::new(&stored) {
                Ok(parsed) => (parsed, true),
                Err(error) => {
                    warn!(%error, "stored password hash unreadable");
                    (PasswordHash::new(&dummy).expect("the dummy hash parses"), false)
                }
            };
            // The hash's own algorithm and parameters are used, whatever the current ones are.
            let matches = Argon2::default().verify_password(&password, &parsed).is_ok();
            let same_params = Params::try_from(&parsed).is_ok_and(|params| {
                params.m_cost() == current.m_cost()
                    && params.t_cost() == current.t_cost()
                    && params.p_cost() == current.p_cost()
            });
            let rehash = parsed.algorithm.as_str() != "argon2id"
                || parsed.version != Some(Version::V0x13 as u32)
                || !same_params;
            let ok = real && readable && matches;
            Verified { ok, rehash: ok && rehash }
        })
        .await
        .map_err(|_| Status::internal("verify failed"))
    }
}

/// The policy for a new password, as a message to show the user.
pub fn check_policy(password: &str, handle: &str, email: &str) -> Result<(), &'static str> {
    let length = password.chars().count();
    if length < MIN_LENGTH {
        return Err("The password needs at least 10 characters.");
    }
    if length > MAX_LENGTH {
        return Err("The password can have at most 128 characters.");
    }
    if password.chars().any(char::is_control) {
        return Err("The password cannot contain control characters.");
    }
    let lower = password.to_lowercase();
    if is_common(&lower) {
        return Err("That password is too common. Choose another.");
    }
    let handle = handle.to_lowercase();
    if handle.len() >= 3 && lower.contains(&handle) {
        return Err("The password cannot contain your handle.");
    }
    let local = email.split('@').next().unwrap_or("").to_lowercase();
    if local.len() >= 3 && lower.contains(&local) {
        return Err("The password cannot contain your email address.");
    }
    if lower.chars().collect::<std::collections::HashSet<_>>().len() < 4 {
        return Err("The password needs more different characters.");
    }
    Ok(())
}

fn is_common(lower: &str) -> bool {
    let stem = lower.trim_end_matches(|c: char| c.is_ascii_digit() || c.is_ascii_punctuation());
    let stem = stem.trim_start_matches(|c: char| c.is_ascii_digit() || c.is_ascii_punctuation());
    COMMON.lines().map(str::trim).filter(|line| !line.is_empty()).any(|common| common == lower || common == stem)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cheap() -> Hasher {
        Hasher::new(1024, 1, 1).unwrap()
    }

    #[tokio::test]
    async fn a_hash_verifies_its_password_only() {
        let hasher = cheap();
        let hash = hasher.hash("correct horse battery").await.unwrap();
        assert!(hash.starts_with("$argon2id$v=19$m=1024,t=1,p=1$"));
        assert!(hasher.verify("correct horse battery", Some(&hash)).await.unwrap().ok);
        assert!(!hasher.verify("correct horse batterY", Some(&hash)).await.unwrap().ok);
        assert!(!hasher.verify("anything", None).await.unwrap().ok);
        assert!(!hasher.verify("anything", Some("not a hash")).await.unwrap().ok);
    }

    #[tokio::test]
    async fn each_hash_has_its_own_salt() {
        let hasher = cheap();
        assert_ne!(hasher.hash("same password").await.unwrap(), hasher.hash("same password").await.unwrap());
    }

    #[tokio::test]
    async fn a_hash_under_old_parameters_is_marked_for_rehash() {
        let old = cheap().hash("upgrade me please").await.unwrap();
        let stronger = Hasher::new(2048, 2, 1).unwrap();
        let verified = stronger.verify("upgrade me please", Some(&old)).await.unwrap();
        assert!(verified.ok && verified.rehash);
        let new = stronger.hash("upgrade me please").await.unwrap();
        assert!(!stronger.verify("upgrade me please", Some(&new)).await.unwrap().rehash);
    }

    #[test]
    fn the_policy_refuses_short_common_and_personal_passwords() {
        assert!(check_policy("short", "pilot", "a@b.c").is_err());
        assert!(check_policy("password123", "pilot", "a@b.c").is_err());
        assert!(check_policy("Starcitizen2954!", "pilot", "a@b.c").is_err());
        assert!(check_policy("1234567890", "pilot", "a@b.c").is_err());
        assert!(check_policy("aaaaaaaaaaaa", "pilot", "a@b.c").is_err());
        assert!(check_policy("my-pilot-is-great", "Pilot", "a@b.c").is_err());
        assert!(check_policy("jane.doe rocks hard", "pilot", "jane.doe@example.com").is_err());
        assert!(check_policy(&"x1".repeat(65), "pilot", "a@b.c").is_err());
        assert!(check_policy("violet tangerine orbit", "pilot", "a@b.c").is_ok());
    }
}
