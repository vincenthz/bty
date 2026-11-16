//! The part of the `simdutf` API rio-vt calls, in plain Rust.
//!
//! The crates.io `simdutf` builds a C++ library. rio-vt only needs UTF-8
//! validation and decoding plus a forgiving base64 decoder, which std and a
//! table do well enough for terminal output, so the workspace patches it with
//! this crate and bty stays pure Rust. Signatures match `simdutf` 0.7.

/// What a call did: `count` is units written on success, the error's input offset otherwise.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub struct Result {
    pub error: ErrorCode,
    pub count: usize,
}

#[repr(u32)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum ErrorCode {
    Success = 0,
    HeaderBits = 1,
    TooShort = 2,
    TooLong = 3,
    OverLong = 4,
    TooLarge = 5,
    Surrogate = 6,
    InvalidBase64Character = 7,
    Base64InputRemainder = 8,
    Base64ExtraBits = 9,
    OutputBufferTooSmall = 10,
    Other = 11,
}

#[repr(u64)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum Base64Options {
    Default = 0,
    Url = 1,
    DefaultNoPadding = 2,
    UrlWithPadding = 3,
    DefaultAcceptGarbage = 4,
    UrlAcceptGarbage = 5,
    DefaultOrUrl = 8,
    DefaultOrUrlAcceptGarbage = 12,
}

#[repr(u64)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum LastChunkHandlingOptions {
    Loose = 0,
    Strict = 1,
    StopBeforePartial = 2,
    OnlyFullChunks = 3,
}

const OK: ErrorCode = ErrorCode::Success;

fn utf8_error(src: &[u8], e: std::str::Utf8Error) -> Result {
    let at = e.valid_up_to();
    let error = match (src[at], e.error_len()) {
        (0x80..=0xbf, _) => ErrorCode::TooLong,
        (0xf8..=0xff, _) => ErrorCode::HeaderBits,
        (_, None) => ErrorCode::TooShort,
        (0xc0 | 0xc1, _) => ErrorCode::OverLong,
        (0xe0, _) if src.get(at + 1).is_some_and(|&b| b < 0xa0) => ErrorCode::OverLong,
        (0xf0, _) if src.get(at + 1).is_some_and(|&b| b < 0x90) => ErrorCode::OverLong,
        (0xed, _) if src.get(at + 1).is_some_and(|&b| b >= 0xa0) => ErrorCode::Surrogate,
        (0xf4..=0xf7, _) => ErrorCode::TooLarge,
        _ => ErrorCode::TooShort,
    };
    Result { error, count: at }
}

pub fn validate_utf8_with_errors(src: &[u8]) -> Result {
    match std::str::from_utf8(src) {
        Ok(_) => Result { error: OK, count: src.len() },
        Err(e) => utf8_error(src, e),
    }
}

/// Code points in valid UTF-8: every byte that doesn't continue a character.
pub fn count_utf8(src: &[u8]) -> usize {
    src.iter().filter(|&&b| (b as i8) >= -0x40).count()
}

/// # Safety
/// `src` is readable for `len` bytes and `dst` writable for as many code
/// points as the valid prefix holds (at most `len`).
pub unsafe fn convert_utf8_to_utf32_with_errors(src: *const u8, len: usize, dst: *mut u32) -> Result {
    let src = unsafe { std::slice::from_raw_parts(src, len) };
    let (text, result) = match std::str::from_utf8(src) {
        Ok(text) => (text, Result { error: OK, count: 0 }),
        Err(e) => {
            // The valid prefix is written too, as simdutf does.
            let text = unsafe { std::str::from_utf8_unchecked(&src[..e.valid_up_to()]) };
            (text, utf8_error(src, e))
        }
    };
    let mut n = 0;
    for c in text.chars() {
        unsafe { dst.add(n).write(c as u32) };
        n += 1;
    }
    if result.error == OK { Result { error: OK, count: n } } else { result }
}

/// An upper bound on what `base64_to_binary` writes for `len` input bytes.
///
/// # Safety
/// Never reads `input`; unsafe only to match simdutf.
pub unsafe fn maximal_binary_length_from_base64(_input: *const u8, len: usize) -> usize {
    len.div_ceil(4) * 3
}

fn sextet(b: u8, url: bool, standard: bool) -> Option<u32> {
    Some(match b {
        b'A'..=b'Z' => b - b'A',
        b'a'..=b'z' => b - b'a' + 26,
        b'0'..=b'9' => b - b'0' + 52,
        b'+' if standard => 62,
        b'/' if standard => 63,
        b'-' if url => 62,
        b'_' if url => 63,
        _ => return None,
    } as u32)
}

/// WHATWG forgiving base64: ASCII whitespace is skipped, padding optional.
///
/// # Safety
/// `input` is readable for `len` bytes and `output` writable for
/// `maximal_binary_length_from_base64(input, len)` bytes.
pub unsafe fn base64_to_binary(
    input: *const u8,
    len: usize,
    output: *mut u8,
    options: Base64Options,
    last_chunk_options: LastChunkHandlingOptions,
) -> Result {
    use Base64Options::*;
    let input = unsafe { std::slice::from_raw_parts(input, len) };
    let url = matches!(options, Url | UrlWithPadding | UrlAcceptGarbage | DefaultOrUrl | DefaultOrUrlAcceptGarbage);
    let standard = !matches!(options, Url | UrlWithPadding | UrlAcceptGarbage);
    let garbage = matches!(options, DefaultAcceptGarbage | UrlAcceptGarbage | DefaultOrUrlAcceptGarbage);
    let fail = |error, count| Result { error, count };
    let (mut acc, mut bits, mut written, mut chars, mut padding) = (0u32, 0u32, 0usize, 0usize, 0usize);
    for (i, &b) in input.iter().enumerate() {
        if matches!(b, b' ' | b'\t' | b'\n' | b'\x0c' | b'\r') {
            continue;
        }
        if b == b'=' {
            padding += 1;
            continue;
        }
        let Some(v) = sextet(b, url, standard) else {
            if garbage {
                continue;
            }
            return fail(ErrorCode::InvalidBase64Character, i);
        };
        if padding > 0 {
            return fail(ErrorCode::InvalidBase64Character, i);
        }
        chars += 1;
        acc = acc << 6 | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            unsafe { output.add(written).write((acc >> bits) as u8) };
            written += 1;
            acc &= (1 << bits) - 1;
        }
    }
    let partial = chars % 4;
    if partial == 1 || padding > 2 || (padding > 0 && (chars + padding) % 4 != 0) {
        return fail(ErrorCode::Base64InputRemainder, input.len());
    }
    match last_chunk_options {
        LastChunkHandlingOptions::Strict if partial != 0 && (padding == 0 || acc != 0) => {
            fail(ErrorCode::Base64ExtraBits, input.len())
        }
        LastChunkHandlingOptions::StopBeforePartial | LastChunkHandlingOptions::OnlyFullChunks if partial != 0 => {
            Result { error: OK, count: written - (partial - 1) }
        }
        _ => Result { error: OK, count: written },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(s: &str) -> Option<Vec<u8>> {
        let mut out = vec![0u8; unsafe { maximal_binary_length_from_base64(s.as_ptr(), s.len()) }];
        let r = unsafe {
            base64_to_binary(
                s.as_ptr(),
                s.len(),
                out.as_mut_ptr(),
                Base64Options::Default,
                LastChunkHandlingOptions::Loose,
            )
        };
        (r.error == OK).then(|| out[..r.count].to_vec())
    }

    #[test]
    fn base64_decodes_padded_unpadded_and_spaced_input_and_rejects_bad_characters() {
        assert_eq!(decode("aGVsbG8=").unwrap(), b"hello");
        assert_eq!(decode("aGVsbG8").unwrap(), b"hello");
        assert_eq!(decode("aGVs\nbG8gd29y bGQ=").unwrap(), b"hello world");
        assert_eq!(decode("").unwrap(), b"");
        assert!(decode("aGV*").is_none());
        assert!(decode("a").is_none());
    }

    #[test]
    fn utf8_decodes_the_valid_prefix_and_reports_where_it_broke() {
        let src = "aé上🦀".as_bytes();
        let mut out = vec![0u32; src.len()];
        let r = unsafe { convert_utf8_to_utf32_with_errors(src.as_ptr(), src.len(), out.as_mut_ptr()) };
        assert_eq!(r, Result { error: OK, count: 4 });
        assert_eq!(&out[..4], &['a' as u32, 'é' as u32, '上' as u32, '🦀' as u32]);
        assert_eq!(count_utf8(src), 4);

        let bad = b"ab\xe4\xb8x";
        let r = unsafe { convert_utf8_to_utf32_with_errors(bad.as_ptr(), bad.len(), out.as_mut_ptr()) };
        assert_eq!((r.error, r.count), (ErrorCode::TooShort, 2));
        assert_eq!(&out[..2], &['a' as u32, 'b' as u32]);
        assert_eq!(validate_utf8_with_errors(b"ok\x80").count, 2);
        assert_eq!(validate_utf8_with_errors(b"ok").error, OK);
    }
}
