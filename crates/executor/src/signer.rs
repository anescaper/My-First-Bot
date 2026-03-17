//! ECDSA signing for Polymarket CTF Exchange orders.
//!
//! Implements EIP-712 typed data signing for order submission and HMAC-SHA256
//! for API request authentication. Two signer implementations:
//!
//! - **DevSigner**: returns dummy signatures for paper trading (no private key needed)
//! - **LiveSigner**: real ECDSA signing via the `ethers` crate (requires `live-signing` feature)
//!
//! The EIP-712 domain includes the Polymarket CTF Exchange contract address on Polygon,
//! with separate contracts for standard markets and negative-risk markets.

use sha3::{Keccak256, Digest};
use crate::order::PolymarketOrder;

/// Polymarket CTF Exchange contract address on Polygon mainnet.
/// Hardcoded because this is a deployed immutable contract.
const CTF_EXCHANGE: &str = "4bFb41d5B3570DeFd03C39a9A4D8dE6Bd8B8982E";

/// Polymarket Neg Risk CTF Exchange contract address on Polygon mainnet.
/// Used for markets with neg_risk=true (most political/event markets).
/// Crypto Up/Down markets use the standard CTF Exchange (neg_risk=false).
const NEG_RISK_CTF_EXCHANGE: &str = "C5d563A36AE78145C45a50134d48A1215220f80a";

/// Polygon mainnet chain ID. Hardcoded because all Polymarket orders are on Polygon.
const POLYGON_CHAIN_ID: u64 = 137;

/// Trait for signing Polymarket orders and API requests.
///
/// Two concerns: (1) EIP-712 order signing for on-chain validity, and
/// (2) HMAC authentication for CLOB API access. Both use the same
/// cryptographic identity but serve different purposes.
pub trait OrderSigner: Send + Sync {
    /// Sign a Polymarket CTF Exchange order via EIP-712 typed data.
    /// Returns a 65-byte signature: 32 bytes r + 32 bytes s + 1 byte v (27 or 28).
    fn sign_order_eip712(&self, order: &PolymarketOrder) -> Vec<u8>;
    /// Compute HMAC-SHA256 signature for CLOB API request authentication.
    /// Message format: "{timestamp}{method}{path}{body}" (no separators).
    fn hmac_signature(&self, timestamp: &str, method: &str, path: &str, body: &str) -> String;
    /// Return the Ethereum address (0x-prefixed hex) associated with this signer.
    fn address(&self) -> String;
}

/// Dummy signer for paper trading — no real private key needed.
///
/// Returns zero-filled 65-byte signatures and a fake address.
/// Orders signed by DevSigner will fail on-chain but work for paper simulation
/// where the PaperExecutor never actually submits to the CLOB.
pub struct DevSigner {
    /// Fake address (recognizable as non-real for debugging)
    address: String,
}

impl DevSigner {
    /// Create a DevSigner with a hardcoded dummy address.
    pub fn new() -> Self {
        Self { address: "0xDEV0000000000000000000000000000000000".to_string() }
    }
}

impl OrderSigner for DevSigner {
    fn sign_order_eip712(&self, _order: &PolymarketOrder) -> Vec<u8> {
        vec![0u8; 65] // 65 zero bytes — invalid but correct length
    }
    fn hmac_signature(&self, _timestamp: &str, _method: &str, _path: &str, _body: &str) -> String {
        "dev-signature".to_string()
    }
    fn address(&self) -> String { self.address.clone() }
}

// --- ABI encoding helpers ---
// These functions encode Solidity types into 32-byte ABI words for EIP-712 hashing.

/// Left-pad data with zeros to fill a 32-byte ABI word.
/// Used for encoding addresses (20 bytes) and smaller integers into uint256.
fn pad_left_32(data: &[u8]) -> [u8; 32] {
    let mut padded = [0u8; 32];
    let start = 32usize.saturating_sub(data.len());
    let copy_len = data.len().min(32);
    padded[start..start + copy_len].copy_from_slice(&data[..copy_len]);
    padded
}

fn encode_u256(val: u128) -> [u8; 32] {
    let bytes = val.to_be_bytes();
    pad_left_32(&bytes)
}

fn encode_u8_as_u256(val: u8) -> [u8; 32] {
    encode_u256(val as u128)
}

fn encode_address(hex_addr: &str) -> [u8; 32] {
    let clean = hex_addr.trim_start_matches("0x");
    let bytes = hex::decode(clean).unwrap_or_else(|_| vec![0u8; 20]);
    pad_left_32(&bytes)
}

fn encode_string_hash(s: &str) -> [u8; 32] {
    Keccak256::digest(s.as_bytes()).into()
}

/// Encode a decimal string as a 256-bit big-endian value.
///
/// Polymarket token IDs are keccak256-derived and can be 77+ digit decimals,
/// far exceeding u128::MAX. This function performs manual base-10 to base-256
/// conversion using big integer arithmetic (multiply-and-add in a byte array).
fn encode_decimal_u256(decimal_str: &str) -> [u8; 32] {
    // Manual base-10 → base-256 conversion using big integer arithmetic
    let mut result = [0u8; 32];
    for ch in decimal_str.bytes() {
        if !ch.is_ascii_digit() { continue; }
        let digit = (ch - b'0') as u16;
        // Multiply result by 10 and add digit
        let mut carry = digit;
        for byte in result.iter_mut().rev() {
            let val = (*byte as u16) * 10 + carry;
            *byte = (val & 0xFF) as u8;
            carry = val >> 8;
        }
    }
    result
}

/// Compute the EIP-712 domain separator for the Polymarket CTF Exchange.
///
/// Domain separator uniquely identifies this contract on this chain, preventing
/// signature replay across different contracts or chains. Components:
/// - name: "Polymarket CTF Exchange"
/// - version: "1"
/// - chainId: 137 (Polygon)
/// - verifyingContract: CTF_EXCHANGE or NEG_RISK_CTF_EXCHANGE
fn compute_domain_separator(neg_risk: bool) -> [u8; 32] {
    let type_hash = Keccak256::digest(
        b"EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)"
    );

    let exchange = if neg_risk { NEG_RISK_CTF_EXCHANGE } else { CTF_EXCHANGE };

    let mut buf = Vec::with_capacity(5 * 32);
    buf.extend_from_slice(&type_hash);
    buf.extend_from_slice(&encode_string_hash("Polymarket CTF Exchange"));
    buf.extend_from_slice(&encode_string_hash("1"));
    buf.extend_from_slice(&encode_u256(POLYGON_CHAIN_ID as u128));
    buf.extend_from_slice(&encode_address(exchange));

    Keccak256::digest(&buf).into()
}

/// EIP-712 type string for the Order struct.
/// Must match the Solidity struct definition in the CTF Exchange contract exactly.
/// The keccak256 of this string is the type hash used in struct encoding.
const ORDER_TYPE_STR: &str = "Order(uint256 salt,address maker,address signer,address taker,uint256 tokenId,uint256 makerAmount,uint256 takerAmount,uint256 expiration,uint256 nonce,uint256 feeRateBps,uint8 side,uint8 signatureType)";

/// Compute the EIP-712 struct hash for a Polymarket order.
///
/// ABI-encodes all 12 order fields in order, prefixed by the type hash,
/// then keccak256 hashes the result. The field order must match ORDER_TYPE_STR.
fn compute_struct_hash(order: &PolymarketOrder) -> [u8; 32] {
    let type_hash: [u8; 32] = Keccak256::digest(ORDER_TYPE_STR.as_bytes()).into();

    // Parse token_id as decimal string → 256-bit big integer
    // Token IDs are keccak256-derived and exceed u128::MAX (77+ digits)
    let token_id_bytes = encode_decimal_u256(&order.token_id);

    let mut buf = Vec::with_capacity(13 * 32);
    buf.extend_from_slice(&type_hash);
    buf.extend_from_slice(&encode_u256(order.salt));
    buf.extend_from_slice(&encode_address(&order.maker));
    buf.extend_from_slice(&encode_address(&order.signer));
    buf.extend_from_slice(&encode_address(&order.taker));
    buf.extend_from_slice(&token_id_bytes);
    buf.extend_from_slice(&encode_u256(order.maker_amount));
    buf.extend_from_slice(&encode_u256(order.taker_amount));
    buf.extend_from_slice(&encode_u256(order.expiration as u128));
    buf.extend_from_slice(&encode_u256(order.nonce));
    buf.extend_from_slice(&encode_u256(order.fee_rate_bps as u128));
    buf.extend_from_slice(&encode_u8_as_u256(order.side));
    buf.extend_from_slice(&encode_u8_as_u256(order.signature_type));

    Keccak256::digest(&buf).into()
}

/// Compute the final EIP-712 digest that gets signed.
///
/// Format: keccak256(0x1901 || domain_separator || struct_hash)
/// The 0x19 0x01 prefix is the EIP-712 standard marker that prevents
/// collision with other signing schemes (e.g., eth_sign uses 0x19 0x00).
pub fn compute_eip712_digest(order: &PolymarketOrder) -> [u8; 32] {
    let domain_sep = compute_domain_separator(order.neg_risk);
    let struct_hash = compute_struct_hash(order);

    let mut buf = Vec::with_capacity(2 + 32 + 32);
    buf.push(0x19);
    buf.push(0x01);
    buf.extend_from_slice(&domain_sep);
    buf.extend_from_slice(&struct_hash);

    Keccak256::digest(&buf).into()
}

// --- Live signer ---

/// Real ECDSA signer for live trading on Polymarket.
///
/// Only compiled when the `live-signing` feature is enabled (adds ethers dependency).
/// Reads the private key from `POLYMARKET_PRIVATE_KEY` env var or Docker secret.
/// Derives the Ethereum address from the public key via keccak256 hash.
///
/// HMAC signatures use the API secret (separate from the private key) for
/// REST API authentication. The API secret may be base64-encoded (with or without padding).
#[cfg(feature = "live-signing")]
pub struct LiveSigner {
    key: ethers::core::k256::ecdsa::SigningKey,
    address: String,
    api_secret: String,
}

#[cfg(feature = "live-signing")]
impl LiveSigner {
    pub fn from_env() -> anyhow::Result<Self> {
        let key_hex = std::env::var("POLYMARKET_PRIVATE_KEY")
            .or_else(|_| {
                std::fs::read_to_string("/run/secrets/polymarket_private_key")
                    .map(|s| s.trim().to_string())
            })?;
        let key_bytes = hex::decode(key_hex.trim_start_matches("0x"))?;
        let key = ethers::core::k256::ecdsa::SigningKey::from_bytes((&key_bytes[..]).into())?;

        let pubkey = key.verifying_key();
        let pubkey_bytes = pubkey.to_encoded_point(false);
        let hash = Keccak256::digest(&pubkey_bytes.as_bytes()[1..]);
        let address = format!("0x{}", hex::encode(&hash[12..]));

        let api_secret = std::env::var("POLYMARKET_API_SECRET")
            .or_else(|_| {
                std::fs::read_to_string("/run/secrets/polymarket_api_secret")
                    .map(|s| s.trim().to_string())
            })
            .unwrap_or_default();

        tracing::info!(address = %address, "LiveSigner initialized");
        Ok(Self { key, address, api_secret })
    }
}

#[cfg(feature = "live-signing")]
impl OrderSigner for LiveSigner {
    fn sign_order_eip712(&self, order: &PolymarketOrder) -> Vec<u8> {
        use ethers::core::k256::ecdsa::signature::hazmat::PrehashSigner;

        let digest = compute_eip712_digest(order);

        let (sig, recid) = self.key.sign_prehash(&digest)
            .expect("EIP-712 signing failed");

        let mut result = sig.to_bytes().to_vec(); // 64 bytes (r + s)
        result.push(recid.to_byte() + 27); // recovery ID: 27 or 28
        result
    }

    fn hmac_signature(&self, timestamp: &str, method: &str, path: &str, body: &str) -> String {
        use hmac::{Hmac, Mac};
        use sha2::Sha256;

        // Polymarket L2 auth: direct concatenation, NO separators
        let message = format!("{}{}{}{}", timestamp, method, path, body);
        use base64::Engine;
        // Polymarket API secrets may or may not have base64 padding
        use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};
        let secret_bytes = URL_SAFE.decode(&self.api_secret)
            .or_else(|_| URL_SAFE_NO_PAD.decode(&self.api_secret))
            .unwrap_or_else(|_| self.api_secret.as_bytes().to_vec());

        let mut mac = Hmac::<Sha256>::new_from_slice(&secret_bytes)
            .expect("HMAC key creation failed");
        mac.update(message.as_bytes());
        let result = mac.finalize();
        URL_SAFE.encode(result.into_bytes())
    }

    fn address(&self) -> String { self.address.clone() }
}
