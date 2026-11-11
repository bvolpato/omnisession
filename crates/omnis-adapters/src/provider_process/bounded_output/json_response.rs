/// Finds the end of a top-level JSON object or array across reads. The caller still
/// validates the entire response with its JSON parser.
pub(super) fn complete_json_response() -> impl FnMut(&[u8]) -> bool + Send + 'static {
    #[derive(Clone, Copy, PartialEq)]
    enum Position {
        Leading,
        Body,
        Quoted,
        Escaped,
        Trailing,
        Invalid,
    }

    let mut position = Position::Leading;
    let mut depth = 0usize;
    let mut root_end = 0u8;
    move |chunk| {
        for &byte in chunk {
            match (position, byte) {
                (Position::Leading | Position::Trailing, b' ' | b'\n' | b'\r' | b'\t') => {}
                (Position::Leading, b'{' | b'[') => {
                    depth = 1;
                    root_end = if byte == b'{' { b'}' } else { b']' };
                    position = Position::Body;
                }
                (Position::Body, b'{' | b'[') => depth += 1,
                (Position::Body, b'}' | b']') if depth == 1 && byte == root_end => {
                    position = Position::Trailing;
                }
                (Position::Body, b'}' | b']') if depth > 1 => depth -= 1,
                (Position::Body, b'}' | b']')
                | (Position::Leading | Position::Trailing | Position::Invalid, _) => {
                    position = Position::Invalid;
                    return false;
                }
                (Position::Body, b'"') | (Position::Escaped, _) => position = Position::Quoted,
                (Position::Quoted, b'\\') => position = Position::Escaped,
                (Position::Quoted, b'"') => position = Position::Body,
                (Position::Body | Position::Quoted, _) => {}
            }
        }
        position == Position::Trailing
    }
}

#[cfg(test)]
mod tests {
    use super::complete_json_response;

    #[test]
    fn recognizes_a_nested_response_at_every_chunk_boundary() {
        let response =
            br#" [{ "text": "quote \" and a false end }] and a slash \\", "rows": [{}] }]"#;
        for split in 0..response.len() {
            let mut complete = complete_json_response();
            assert!(!complete(&response[..split]));
            assert!(complete(&response[split..]));
            assert!(complete(b"\r\n\t "));
        }
        let mut complete = complete_json_response();
        let last = response.len() - 1;
        for byte in &response[..last] {
            assert!(!complete(std::slice::from_ref(byte)));
        }
        assert!(complete(&response[last..]));
    }

    #[test]
    fn ignores_empty_output_and_rejects_extra_or_mismatched_roots() {
        for response in [b" \n" as &[u8], br"42", br"{]", br"[]{}"] {
            let mut complete = complete_json_response();
            assert!(!complete(response));
        }
        let mut complete = complete_json_response();
        assert!(!complete(br"noise"));
        assert!(!complete(br"{}"));
    }
}
