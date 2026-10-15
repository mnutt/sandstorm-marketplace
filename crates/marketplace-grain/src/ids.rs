use crate::{appid_replacements_capnp::APP_ID_REPLACEMENT_LIST, package_capnp as p};
use anyhow::{Result, bail, ensure};
const ALPHABET: &[u8] = b"0123456789acdefghjkmnpqrstuvwxyz";
pub fn app_id(bytes: &[u8]) -> String {
    let mut out = String::new();
    let mut buffer = 0u32;
    let mut bits = 0;
    for byte in bytes {
        buffer = (buffer << 8) | u32::from(*byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ALPHABET[((buffer >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(ALPHABET[((buffer << (5 - bits)) & 31) as usize] as char);
    }
    out
}
pub fn parse_app_id(text: &str) -> Result<[u8; 32]> {
    ensure!(text.len() == 52, "invalid app ID");
    let mut out = Vec::new();
    let mut buffer = 0u32;
    let mut bits = 0;
    for byte in text.bytes() {
        let value = ALPHABET
            .iter()
            .position(|&b| b == byte)
            .ok_or_else(|| anyhow::anyhow!("invalid app ID"))?;
        buffer = (buffer << 5) | value as u32;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    ensure!(buffer & ((1 << bits) - 1) == 0, "invalid app ID padding");
    out.try_into()
        .map_err(|_| anyhow::anyhow!("invalid app ID length"))
}
pub fn app_bytes(id: p::app_id::Reader<'_>) -> [u8; 32] {
    let mut out = [0; 32];
    for (chunk, n) in
        out.chunks_mut(8)
            .zip([id.get_id0(), id.get_id1(), id.get_id2(), id.get_id3()])
    {
        chunk.copy_from_slice(&n.to_le_bytes());
    }
    out
}
pub fn package_bytes(id: p::package_id::Reader<'_>) -> [u8; 16] {
    let mut out = [0; 16];
    out[..8].copy_from_slice(&id.get_id0().to_le_bytes());
    out[8..].copy_from_slice(&id.get_id1().to_le_bytes());
    out
}
pub fn set_app(mut id: p::app_id::Builder<'_>, bytes: &[u8; 32]) {
    let words: Vec<_> = bytes
        .as_chunks::<8>()
        .0
        .iter()
        .map(|v| u64::from_le_bytes(*v))
        .collect();
    id.set_id0(words[0]);
    id.set_id1(words[1]);
    id.set_id2(words[2]);
    id.set_id3(words[3]);
}
pub fn set_package(mut id: p::package_id::Builder<'_>, bytes: &[u8; 16]) {
    id.set_id0(u64::from_le_bytes(bytes[..8].try_into().unwrap()));
    id.set_id1(u64::from_le_bytes(bytes[8..].try_into().unwrap()));
}
pub fn canonical_app(key: &[u8; 32], package: &[u8; 16]) -> Result<[u8; 32]> {
    let rules = APP_ID_REPLACEMENT_LIST.get()?;
    let mut id = app_id(key);
    for rule in rules {
        if rule.get_original()?.to_str()? == id && rule.has_revoke_except_package_ids() {
            ensure!(
                rule.get_revoke_except_package_ids()?
                    .iter()
                    .any(|v| v.ok().and_then(|s| s.to_str().ok())
                        == Some(hex::encode(package).as_str())),
                "package signed with a revoked key"
            );
        }
    }
    for _ in 0..=rules.len() {
        let mut next = None;
        for rule in rules {
            if rule.get_replacement()?.to_str()? == id {
                next = Some(rule.get_original()?.to_str()?.to_string());
                break;
            }
        }
        match next {
            Some(value) => id = value,
            None => return parse_app_id(&id),
        }
    }
    bail!("cyclic app ID replacement table")
}
pub fn current_key(canonical: &[u8; 32]) -> Result<[u8; 32]> {
    let rules = APP_ID_REPLACEMENT_LIST.get()?;
    let mut id = app_id(canonical);
    for _ in 0..=rules.len() {
        let mut next = None;
        for rule in rules {
            if rule.get_original()?.to_str()? == id {
                next = Some(rule.get_replacement()?.to_str()?.to_string());
                break;
            }
        }
        match next {
            Some(value) => id = value,
            None => return parse_app_id(&id),
        }
    }
    bail!("cyclic app ID replacement table")
}
