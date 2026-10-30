//! Neutralizes statement-like macro invocations before parsing.
//!
//! Lines such as `NS_DECL_ISUPPORTS` or `NS_IMPL_ISUPPORTS(Foo, nsIBar)` have no semicolon, so
//! tree-sitter's error recovery can swallow a following `}` and mis-scope the rest of the file.
//! We overwrite them with spaces (newlines kept), so byte offsets and line numbers are unchanged.

/// A blanked invocation: macro name and 1-based line.
pub(super) struct Blanked {
    pub name: String,
    pub line: u32,
}

/// Returns the rewritten source (or `None` if nothing changed) and the blanked invocations.
pub(super) fn neutralize(src: &[u8]) -> (Option<Vec<u8>>, Vec<Blanked>) {
    let (out, blanked) = neutralize_statements(src);
    let base = out.as_deref().unwrap_or(src);
    match blank_class_attributes(base) {
        Some(rewritten) => (Some(rewritten), blanked),
        None => (out, blanked),
    }
}

/// Blanks attribute macros between a class key and the class name, e.g. `class NET_EXPORT Foo`,
/// `struct COMPONENT_EXPORT(X) Bar`, `class MOZ_STACK_CLASS Baz`. Such a macro must be `ALL_CAPS` with
/// an underscore and be followed by another identifier, which plain C++ never allows there.
fn blank_class_attributes(src: &[u8]) -> Option<Vec<u8>> {
    let mut out: Option<Vec<u8>> = None;
    let mut pos = 0;
    while pos < src.len() {
        let word_len = ident_len(&src[pos..]);
        if word_len == 0 {
            pos += 1;
            continue;
        }
        let word = &src[pos..pos + word_len];
        let at_word_start =
            pos == 0 || !(src[pos - 1].is_ascii_alphanumeric() || src[pos - 1] == b'_');
        let mut cursor = pos + word_len;
        if at_word_start && matches!(word, b"class" | b"struct" | b"union") {
            loop {
                let start = cursor + ws_len(&src[cursor..]);
                let len = ident_len(&src[start..]);
                let name = &src[start..start + len];
                if len == 0 || !is_attribute_macro(name) {
                    break;
                }
                let mut end = start + len;
                let after = end + ws_len(&src[end..]);
                if src.get(after) == Some(&b'(') {
                    match matching_paren(src, after) {
                        Some(e) => end = e,
                        None => break,
                    }
                }
                let next = end + ws_len(&src[end..]);
                if ident_len(&src[next..]) == 0 {
                    break; // `class FOO_BAR {` or `class FOO_BAR;`: the macro may be the name
                }
                let buf = out.get_or_insert_with(|| src.to_vec());
                for b in &mut buf[start..end] {
                    if *b != b'\n' {
                        *b = b' ';
                    }
                }
                cursor = end;
            }
        }
        pos = cursor;
    }
    out
}

fn is_attribute_macro(name: &[u8]) -> bool {
    name.len() > 2
        && name[0].is_ascii_uppercase()
        && name.contains(&b'_')
        && name
            .iter()
            .all(|&b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
}

fn ws_len(s: &[u8]) -> usize {
    s.iter().take_while(|b| b.is_ascii_whitespace()).count()
}

fn neutralize_statements(src: &[u8]) -> (Option<Vec<u8>>, Vec<Blanked>) {
    let mut out: Option<Vec<u8>> = None;
    let mut blanked = Vec::new();
    let mut line_no: u32 = 1;
    let mut i = 0;
    while i < src.len() {
        let line_end = memchr(b'\n', &src[i..]).map_or(src.len(), |n| i + n);
        if let Some(end) = statement_macro_at(src, i, line_end) {
            let start = i + leading_ws(&src[i..line_end]);
            let name_end = start + ident_len(&src[start..]);
            blanked.push(Blanked {
                name: String::from_utf8_lossy(&src[start..name_end]).into_owned(),
                line: line_no,
            });
            let buf = out.get_or_insert_with(|| src.to_vec());
            for b in &mut buf[start..end] {
                if *b != b'\n' {
                    *b = b' ';
                }
            }
            line_no += count_newlines(&src[i..end]);
            i = end;
            continue;
        }
        line_no += 1;
        i = line_end + 1;
    }
    (out, blanked)
}

/// If the line starting at `start` is a lone statement-like macro, returns the end offset of the
/// invocation (after its closing parenthesis, possibly on a later line).
fn statement_macro_at(src: &[u8], start: usize, line_end: usize) -> Option<usize> {
    if inside_list(src, start) {
        return None;
    }
    let name_start = start + leading_ws(&src[start..line_end]);
    let name_len = ident_len(&src[name_start..line_end]);
    let name = &src[name_start..name_start + name_len];
    if name_len < 2
        || !name[0].is_ascii_uppercase()
        || !name
            .iter()
            .all(|&b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
    {
        return None;
    }
    let mut end = name_start + name_len;
    end += leading_ws(&src[end..line_end]);
    if src.get(end) == Some(&b'(') {
        end = matching_paren(src, end)?;
    }
    // Nothing else may follow on the line but a comment.
    let rest_end = memchr(b'\n', &src[end..]).map_or(src.len(), |n| end + n);
    let rest = trim_ascii(&src[end..rest_end]);
    if !(rest.is_empty() || rest.starts_with(b"//")) {
        return None;
    }
    // A return-type macro (`NS_IMETHODIMP\nFoo::Bar(`) continues a declaration: keep it.
    if continues_declaration(&src[rest_end..]) {
        return None;
    }
    Some(end)
}

/// Whether the line starting at `start` continues a list rather than starting a statement: the
/// previous code ends with `,` (`RED,\n BLUE`), or with the `{` of an `enum` (`enum E {\n ONLY`).
fn inside_list(src: &[u8], start: usize) -> bool {
    // The previous non-blank line, without a trailing `//` comment.
    let mut end = start;
    let prev = loop {
        if end == 0 {
            return false;
        }
        let line_start = src[..end - 1]
            .iter()
            .rposition(|&b| b == b'\n')
            .map_or(0, |i| i + 1);
        let line = &src[line_start..end - 1];
        let code = line
            .windows(2)
            .position(|w| w == b"//")
            .map_or(line, |i| &line[..i]);
        let code = trim_ascii(code);
        if !code.is_empty() {
            break code;
        }
        end = line_start;
    };
    match prev.last() {
        Some(b',' | b'(' | b'=' | b'|' | b'&' | b'+' | b'-' | b'?' | b':') => true,
        Some(b'{') => {
            // Text since the previous statement or block boundary: does it open an enum?
            let before = &prev[..prev.len() - 1];
            let head = before
                .iter()
                .rposition(|&b| matches!(b, b';' | b'{' | b'}'))
                .map_or(before, |i| &before[i + 1..]);
            head.split(|b| !(b.is_ascii_alphanumeric() || *b == b'_'))
                .any(|w| w == b"enum")
        }
        _ => false,
    }
}

/// Whether the next token starts the rest of a declaration (`Name(`, `Name::`, `{`, `:` ...).
fn continues_declaration(after: &[u8]) -> bool {
    let mut i = 0;
    loop {
        i += after[i..]
            .iter()
            .take_while(|b| b.is_ascii_whitespace())
            .count();
        if after[i..].starts_with(b"//") {
            i += memchr(b'\n', &after[i..]).unwrap_or(after.len() - i);
        } else {
            break;
        }
    }
    let rest = &after[i..];
    match rest.first() {
        None => false,
        Some(b'{' | b':' | b';' | b',' | b')' | b'=' | b'(') => true,
        Some(_) => {
            let n = ident_len(rest);
            if n == 0 {
                return false;
            }
            let tail = &rest[n..];
            let tail = &tail[tail
                .iter()
                .take_while(|b| **b == b' ' || **b == b'\t')
                .count()..];
            tail.starts_with(b"(") || tail.starts_with(b"::")
        }
    }
}

/// Offset just past the `)` matching the `(` at `open`, skipping string and char literals.
fn matching_paren(src: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut i = open;
    while i < src.len() {
        match src[i] {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            q @ (b'"' | b'\'') => {
                i += 1;
                while i < src.len() && src[i] != q {
                    if src[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
            }
            // A statement or block inside means this is not a macro invocation line.
            b';' | b'{' | b'}' => return None,
            _ => {}
        }
        i += 1;
    }
    None
}

fn ident_len(s: &[u8]) -> usize {
    s.iter()
        .take_while(|b| b.is_ascii_alphanumeric() || **b == b'_')
        .count()
}

fn leading_ws(s: &[u8]) -> usize {
    s.iter().take_while(|b| **b == b' ' || **b == b'\t').count()
}

fn trim_ascii(s: &[u8]) -> &[u8] {
    let start = s.iter().take_while(|b| b.is_ascii_whitespace()).count();
    let end = s.len()
        - s.iter()
            .rev()
            .take_while(|b| b.is_ascii_whitespace())
            .count();
    if start >= end { &[] } else { &s[start..end] }
}

fn memchr(needle: u8, hay: &[u8]) -> Option<usize> {
    hay.iter().position(|&b| b == needle)
}

#[allow(clippy::naive_bytecount)] // lines are short; not worth a dependency
fn count_newlines(s: &[u8]) -> u32 {
    u32::try_from(s.iter().filter(|&&b| b == b'\n').count()).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(src: &str) -> (String, Vec<(String, u32)>) {
        let (out, b) = neutralize(src.as_bytes());
        let text = out.map_or_else(|| src.to_owned(), |o| String::from_utf8(o).unwrap());
        (text, b.into_iter().map(|b| (b.name, b.line)).collect())
    }

    #[test]
    fn blanks_statement_macros_keeping_layout() {
        let src = "class A {\n  NS_DECL_ISUPPORTS\n};\nNS_IMPL_ISUPPORTS(A,\n  nsIFoo)\n}  // ns\n";
        let (text, b) = run(src);
        assert_eq!(text.len(), src.len());
        assert_eq!(text.lines().count(), src.lines().count());
        assert!(!text.contains("NS_DECL") && !text.contains("NS_IMPL"));
        assert_eq!(
            b,
            [
                ("NS_DECL_ISUPPORTS".into(), 2),
                ("NS_IMPL_ISUPPORTS".into(), 4)
            ]
        );
    }

    #[test]
    fn blanks_class_attribute_macros() {
        let src = "class NET_EXPORT Foo {};\nclass COMPONENT_EXPORT(NET) Bar\n : public X {};\nstruct MOZ_RAII MOZ_STACK_CLASS Baz;\n";
        let (text, _) = run(src);
        assert_eq!(text.len(), src.len());
        let blank = |s: &str| " ".repeat(s.len());
        assert!(text.starts_with(&format!("class {} Foo {{}};", blank("NET_EXPORT"))));
        assert!(text.contains(&format!("class {} Bar", blank("COMPONENT_EXPORT(NET)"))));
        assert!(text.contains(&format!(
            "struct {} Baz;",
            blank("MOZ_RAII MOZ_STACK_CLASS")
        )));
        for keep in [
            "class FOO_BAR {};\n",
            "class Foo_Bar Baz;\n",
            "myclass NET_EXPORT x;\n",
        ] {
            assert_eq!(run(keep).0, keep);
        }
    }

    #[test]
    fn keeps_enumerators_and_list_items() {
        for src in [
            "enum Color {\n  RED,\n  GREEN, // c\n  BLUE\n};\n",
            "enum E {\n  ONLY\n};\n",
            "enum class E : int {\n  ONLY_ONE\n};\n",
            "int x = f(a,\n          MAX_SIZE\n);\n",
        ] {
            let (text, b) = run(src);
            assert_eq!(text, src);
            assert!(b.is_empty(), "{src}");
        }
        // A class body holding only a macro is still a macro line.
        let (_, b) = run("class A {\n  NS_DECL_ISUPPORTS\n};\n");
        assert_eq!(b.len(), 1);
    }

    #[test]
    fn keeps_return_type_macros_and_statements() {
        for src in [
            "NS_IMETHODIMP\nFoo::Bar(int a) {\n}\n",
            "  NS_IMETHOD\n  VisitHeader(const nsACString& h) override {}\n",
            "  MOZ_ASSERT(x);\n",
            "TEST(Foo, Bar) {\n}\n",
            "NS_IMETHOD_(MozExternalRefCountType) AddRef() override;\n",
            "TEST_F(Foo, Bar)\n{\n}\n",
        ] {
            let (text, b) = run(src);
            assert_eq!(text, src);
            assert!(b.is_empty(), "{src}");
        }
    }
}
