//! secp128r1 ECDH, the curve of the connect handshake. Keys are `04 || X || Y`, 16 bytes each, as
//! libtomcrypt exports them. No maintained Rust crate has this curve. The client chooses it, so this
//! is not constant time: a 128-bit curve protects nothing that timing would reveal.

use std::sync::LazyLock;

use num_bigint::BigUint;
use num_traits::{One, Zero};
use rand_core::{OsRng, RngCore};

pub const KEY_LEN: usize = 33;
pub const SECRET_LEN: usize = 16;
const COORDINATE_LEN: usize = 16;
const UNCOMPRESSED: u8 = 0x04;

struct Curve {
    p: BigUint,
    a: BigUint,
    b: BigUint,
    generator: Point,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Point {
    x: BigUint,
    y: BigUint,
}

fn hex(text: &str) -> BigUint {
    BigUint::parse_bytes(text.as_bytes(), 16).expect("constant is hex")
}

static CURVE: LazyLock<Curve> = LazyLock::new(|| Curve {
    p: hex("FFFFFFFDFFFFFFFFFFFFFFFFFFFFFFFF"),
    a: hex("FFFFFFFDFFFFFFFFFFFFFFFFFFFFFFFC"),
    b: hex("E87579C11079F43DD824993C2CEE5ED3"),
    generator: Point { x: hex("161FF7528B899B2D0C28607CA52C5B86"), y: hex("CF5AC8395BAFEB13C02DA292DDED7A83") },
});

impl Curve {
    fn sub(&self, left: &BigUint, right: &BigUint) -> BigUint {
        (left + &self.p - right % &self.p) % &self.p
    }

    fn inverse(&self, value: &BigUint) -> BigUint {
        value.modpow(&(&self.p - 2u32), &self.p)
    }

    /// `None` is the point at infinity.
    fn add(&self, left: &Option<Point>, right: &Option<Point>) -> Option<Point> {
        let (first, second) = match (left, right) {
            (None, other) | (other, None) => return other.clone(),
            (Some(first), Some(second)) => (first, second),
        };
        let slope = if first.x == second.x {
            if (&first.y + &second.y) % &self.p == BigUint::zero() {
                return None;
            }
            (&first.x * &first.x * 3u32 + &self.a) * self.inverse(&(&first.y * 2u32)) % &self.p
        } else {
            self.sub(&second.y, &first.y) * self.inverse(&self.sub(&second.x, &first.x)) % &self.p
        };
        let x = self.sub(&self.sub(&(&slope * &slope), &first.x), &second.x);
        let y = self.sub(&(&slope * self.sub(&first.x, &x)), &first.y);
        Some(Point { x, y })
    }

    fn multiply(&self, scalar: &BigUint, point: &Point) -> Option<Point> {
        let mut result = None;
        let mut addend = Some(point.clone());
        for bit in 0..scalar.bits() {
            if scalar.bit(bit) {
                result = self.add(&result, &addend);
            }
            addend = self.add(&addend, &addend);
        }
        result
    }

    fn contains(&self, point: &Point) -> bool {
        let right = (&point.x * &point.x * &point.x + &self.a * &point.x + &self.b) % &self.p;
        point.x < self.p && point.y < self.p && &point.y * &point.y % &self.p == right
    }
}

fn coordinate(value: &BigUint) -> [u8; COORDINATE_LEN] {
    let bytes = value.to_bytes_be();
    let mut out = [0u8; COORDINATE_LEN];
    out[COORDINATE_LEN - bytes.len()..].copy_from_slice(&bytes);
    out
}

impl Point {
    /// Refuses anything off the curve, as a crafted point would leak the secret scalar.
    pub fn parse(key: &[u8]) -> Option<Point> {
        let (&format, coordinates) = key.split_first()?;
        if format != UNCOMPRESSED || coordinates.len() != 2 * COORDINATE_LEN {
            return None;
        }
        let (x, y) = coordinates.split_at(COORDINATE_LEN);
        let point = Point { x: BigUint::from_bytes_be(x), y: BigUint::from_bytes_be(y) };
        CURVE.contains(&point).then_some(point)
    }

    pub fn encode(&self) -> [u8; KEY_LEN] {
        let mut key = [0u8; KEY_LEN];
        key[0] = UNCOMPRESSED;
        key[1..1 + COORDINATE_LEN].copy_from_slice(&coordinate(&self.x));
        key[1 + COORDINATE_LEN..].copy_from_slice(&coordinate(&self.y));
        key
    }
}

pub struct KeyPair {
    secret: BigUint,
    pub public: Point,
}

impl KeyPair {
    pub fn generate() -> KeyPair {
        loop {
            // 127 bits stays below the group order, which is just under 2^128.
            let mut bytes = [0u8; COORDINATE_LEN];
            OsRng.fill_bytes(&mut bytes);
            bytes[0] &= 0x7f;
            let secret = BigUint::from_bytes_be(&bytes);
            if secret < BigUint::one() + BigUint::one() {
                continue;
            }
            if let Some(public) = CURVE.multiply(&secret, &CURVE.generator) {
                return KeyPair { secret, public };
            }
        }
    }

    /// The X coordinate of the shared point, which is what libtomcrypt's `ecc_shared_secret` returns.
    pub fn shared_secret(&self, peer: &Point) -> Option<[u8; SECRET_LEN]> {
        CURVE.multiply(&self.secret, peer).map(|point| coordinate(&point.x))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_sides_reach_the_same_secret() {
        let (left, right) = (KeyPair::generate(), KeyPair::generate());
        assert_eq!(left.shared_secret(&right.public), right.shared_secret(&left.public));
    }

    #[test]
    fn public_keys_survive_encoding_and_lie_on_the_curve() {
        let pair = KeyPair::generate();
        assert_eq!(Point::parse(&pair.public.encode()), Some(pair.public));
    }

    #[test]
    fn points_off_the_curve_are_refused() {
        let mut key = KeyPair::generate().public.encode();
        key[32] ^= 1;
        assert_eq!(Point::parse(&key), None);
        assert_eq!(Point::parse(&[UNCOMPRESSED; 3]), None);
        let mut compressed = KeyPair::generate().public.encode();
        compressed[0] = 2;
        assert_eq!(Point::parse(&compressed), None);
    }
}
