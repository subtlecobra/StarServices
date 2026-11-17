//! Lobby passwords. A hash is `iterations || salt || PBKDF2-HMAC-SHA256`. Rows from before this are a
//! bare SHA-256 of the group id and password, told apart by their length, and are replaced at the next
//! join that gets them right. The work takes a large part of a second, so it stays off the async threads.

use std::num::NonZeroU32;

use ring::{
    digest::{digest, SHA256, SHA256_OUTPUT_LEN},
    pbkdf2::{self, PBKDF2_HMAC_SHA256},
    rand::{SecureRandom, SystemRandom},
};
use tonic::Status;

const ITERATIONS: NonZeroU32 = NonZeroU32::new(600_000).unwrap();
const ITERATIONS_LEN: usize = size_of::<u32>();
const SALT_LEN: usize = 16;
const HASHED_LEN: usize = ITERATIONS_LEN + SALT_LEN + SHA256_OUTPUT_LEN;

fn derive(password: &str, iterations: NonZeroU32) -> Result<Vec<u8>, Status> {
    let mut stored = vec![0; HASHED_LEN];
    let (head, hash) = stored.split_at_mut(ITERATIONS_LEN + SALT_LEN);
    let (count, salt) = head.split_at_mut(ITERATIONS_LEN);
    count.copy_from_slice(&iterations.get().to_be_bytes());
    SystemRandom::new().fill(salt).map_err(|_| Status::internal("no system randomness"))?;
    pbkdf2::derive(PBKDF2_HMAC_SHA256, iterations, salt, password.as_bytes(), hash);
    Ok(stored)
}

fn matches(group_id: &str, password: &str, stored: &[u8]) -> bool {
    if is_outdated(stored) {
        return digest(&SHA256, format!("{group_id}{password}").as_bytes()).as_ref() == stored;
    }
    if stored.len() != HASHED_LEN {
        return false;
    }
    let (head, hash) = stored.split_at(ITERATIONS_LEN + SALT_LEN);
    let (count, salt) = head.split_at(ITERATIONS_LEN);
    let iterations = count.try_into().ok().map(u32::from_be_bytes).and_then(NonZeroU32::new);
    iterations.is_some_and(|n| pbkdf2::verify(PBKDF2_HMAC_SHA256, n, salt, password.as_bytes(), hash).is_ok())
}

pub fn is_outdated(stored: &[u8]) -> bool {
    stored.len() == SHA256_OUTPUT_LEN
}

async fn blocking<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> Result<T, Status> {
    tokio::task::spawn_blocking(work).await.map_err(|_| Status::internal("password check failed"))
}

pub async fn hash(password: &str) -> Result<Vec<u8>, Status> {
    let password = password.to_string();
    blocking(move || derive(&password, ITERATIONS)).await?
}

pub async fn verify(group_id: &str, password: &str, stored: &[u8]) -> Result<bool, Status> {
    let (group_id, password, stored) = (group_id.to_string(), password.to_string(), stored.to_vec());
    blocking(move || matches(&group_id, &password, &stored)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    const FEW: NonZeroU32 = NonZeroU32::new(10).unwrap();

    #[tokio::test]
    async fn a_hash_takes_the_right_password_only() {
        let stored = hash("hunter2").await.unwrap();
        assert_eq!(stored.len(), HASHED_LEN);
        assert_eq!(&stored[..ITERATIONS_LEN], ITERATIONS.get().to_be_bytes());
        assert!(!is_outdated(&stored));
        assert!(verify("g", "hunter2", &stored).await.unwrap());
        assert!(!verify("g", "hunter3", &stored).await.unwrap());
    }

    #[test]
    fn the_salt_is_random() {
        let (one, other) = (derive("same", FEW).unwrap(), derive("same", FEW).unwrap());
        assert_ne!(one[ITERATIONS_LEN..ITERATIONS_LEN + SALT_LEN], other[ITERATIONS_LEN..ITERATIONS_LEN + SALT_LEN]);
        assert_ne!(one, other);
    }

    #[test]
    fn the_stored_iterations_decide_so_they_can_be_raised() {
        assert!(matches("g", "pass", &derive("pass", FEW).unwrap()));
    }

    #[test]
    fn hashes_from_before_still_work() {
        let stored = digest(&SHA256, b"group-1secret").as_ref().to_vec();
        assert!(is_outdated(&stored));
        assert!(matches("group-1", "secret", &stored));
        assert!(!matches("group-1", "wrong", &stored));
    }

    #[test]
    fn a_damaged_hash_matches_nothing() {
        assert!(!matches("g", "", &[]));
        assert!(!matches("g", "pass", &[0; HASHED_LEN]));
    }
}
