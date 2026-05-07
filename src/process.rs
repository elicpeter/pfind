//! Filter primitives used by the inline walker.

/// Tiny glob matcher supporting `*` (any run of chars) and `?` (one char).
///
/// No bracket classes, no escapes, no `**`. Quadratic worst case on
/// pathological inputs, which is fine for typical CLI patterns.
pub fn glob_match(pat: &[u8], s: &[u8]) -> bool {
    let (mut pi, mut si) = (0usize, 0usize);
    let (mut star, mut match_si): (Option<usize>, usize) = (None, 0);

    while si < s.len() {
        if pi < pat.len() && (pat[pi] == b'?' || pat[pi] == s[si]) {
            pi += 1;
            si += 1;
        } else if pi < pat.len() && pat[pi] == b'*' {
            star = Some(pi);
            match_si = si;
            pi += 1;
        } else if let Some(sp) = star {
            pi = sp + 1;
            match_si += 1;
            si = match_si;
        } else {
            return false;
        }
    }

    while pi < pat.len() && pat[pi] == b'*' {
        pi += 1;
    }
    pi == pat.len()
}

/// Case-insensitive ASCII extension match. Returns true if `name` ends with
/// `.ext`. `ext` must be lowercase ASCII (CLI normalises it).
#[inline]
pub fn name_matches_extension(name: &[u8], ext: &[u8]) -> bool {
    if name.len() <= ext.len() {
        return false;
    }
    let dot_idx = name.len() - ext.len() - 1;
    if name[dot_idx] != b'.' {
        return false;
    }
    let suffix = &name[dot_idx + 1..];
    // ext is already lowercase; case-fold suffix on the fly.
    for (a, b) in suffix.iter().zip(ext.iter()) {
        if a.to_ascii_lowercase() != *b {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_basic() {
        assert!(glob_match(b"*.txt", b"foo.txt"));
        assert!(glob_match(b"file_00*", b"file_001.dat"));
        assert!(!glob_match(b"file_00*", b"file_010.dat"));
        assert!(glob_match(b"*", b"anything"));
        assert!(glob_match(b"foo?bar", b"foo_bar"));
        assert!(!glob_match(b"foo?bar", b"foo__bar"));
        assert!(glob_match(b"a*b*c", b"axxbyyc"));
        assert!(!glob_match(b"abc", b"abcd"));
    }

    #[test]
    fn ext_match() {
        assert!(name_matches_extension(b"foo.jpg", b"jpg"));
        assert!(name_matches_extension(b"foo.JPG", b"jpg"));
        assert!(!name_matches_extension(b"foo.jpeg", b"jpg"));
        assert!(!name_matches_extension(b"foojpg", b"jpg"));
        assert!(!name_matches_extension(b"jpg", b"jpg"));
        assert!(name_matches_extension(b"a.b.c", b"c"));
    }
}
