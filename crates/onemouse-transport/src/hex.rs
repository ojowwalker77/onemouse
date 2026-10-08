pub fn encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn decode(text: &str) -> Option<Vec<u8>> {
    let text = text.trim();
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn round_trip() {
        assert_eq!(super::encode(&[0, 0xAB, 0x7f]), "00ab7f");
        assert_eq!(super::decode("00AB7f"), Some(vec![0, 0xAB, 0x7F]));
        assert_eq!(super::decode("abc"), None);
        assert_eq!(super::decode("zz"), None);
        assert_eq!(super::decode("é1"), None);
    }
}
