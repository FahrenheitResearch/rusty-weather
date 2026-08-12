use crate::error::{ErrorKind, FormulaError, FormulaResult, Span};
use crate::model::ResourceLimits;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum TokenKind {
    Number(f64),
    Identifier(String),
    Text(String),
    Plus,
    Minus,
    Star,
    Slash,
    Caret,
    LeftParen,
    RightParen,
    Comma,
    Semi,
    Assign,
    Eq,
    NotEq,
    Less,
    LessEq,
    Greater,
    GreaterEq,
    And,
    Or,
    Not,
    Eof,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

pub(crate) fn lex(source: &str, limits: &ResourceLimits) -> FormulaResult<Vec<Token>> {
    if source.len() > limits.max_source_bytes {
        return Err(FormulaError::new(
            ErrorKind::Limit,
            format!(
                "formula is {} bytes; limit is {}",
                source.len(),
                limits.max_source_bytes
            ),
        ));
    }
    let bytes = source.as_bytes();
    let mut tokens = Vec::new();
    let mut cursor = 0_usize;
    while cursor < bytes.len() {
        if tokens.len() >= limits.max_tokens {
            return Err(FormulaError::new(
                ErrorKind::Limit,
                format!("formula exceeds token limit {}", limits.max_tokens),
            )
            .at(Span::new(cursor, cursor)));
        }
        let start = cursor;
        match bytes[cursor] {
            b' ' | b'\t' | b'\r' => cursor += 1,
            b'\n' | b';' => {
                cursor += 1;
                tokens.push(Token {
                    kind: TokenKind::Semi,
                    span: Span::new(start, cursor),
                });
            }
            b'#' => skip_comment(bytes, &mut cursor),
            b'/' if bytes.get(cursor + 1) == Some(&b'/') => skip_comment(bytes, &mut cursor),
            b'0'..=b'9' | b'.'
                if bytes[cursor] != b'.'
                    || bytes.get(cursor + 1).is_some_and(u8::is_ascii_digit) =>
            {
                cursor = scan_number(bytes, cursor);
                let spelling = &source[start..cursor];
                let value = spelling.parse::<f64>().map_err(|_| {
                    FormulaError::new(
                        ErrorKind::Lex,
                        format!("invalid numeric literal '{spelling}'"),
                    )
                    .at(Span::new(start, cursor))
                })?;
                if !value.is_finite() {
                    return Err(FormulaError::new(
                        ErrorKind::Lex,
                        "numeric literals must be finite",
                    )
                    .at(Span::new(start, cursor)));
                }
                tokens.push(Token {
                    kind: TokenKind::Number(value),
                    span: Span::new(start, cursor),
                });
            }
            b'a'..=b'z' | b'A'..=b'Z' | b'_' => {
                cursor += 1;
                while cursor < bytes.len()
                    && (bytes[cursor].is_ascii_alphanumeric() || bytes[cursor] == b'_')
                {
                    cursor += 1;
                }
                if cursor - start > limits.max_identifier_bytes {
                    return Err(FormulaError::new(
                        ErrorKind::Limit,
                        format!("identifier exceeds {} bytes", limits.max_identifier_bytes),
                    )
                    .at(Span::new(start, cursor)));
                }
                let spelling = &source[start..cursor];
                let kind = match spelling {
                    "and" => TokenKind::And,
                    "or" => TokenKind::Or,
                    "not" => TokenKind::Not,
                    _ => TokenKind::Identifier(spelling.to_string()),
                };
                tokens.push(Token {
                    kind,
                    span: Span::new(start, cursor),
                });
            }
            b'\'' | b'\"' => {
                let quote = bytes[cursor];
                cursor += 1;
                let mut value = String::new();
                let mut closed = false;
                while cursor < bytes.len() {
                    let byte = bytes[cursor];
                    if byte == quote {
                        cursor += 1;
                        closed = true;
                        break;
                    }
                    if byte == b'\n' || byte == b'\r' {
                        return Err(FormulaError::new(
                            ErrorKind::Lex,
                            "string literals cannot span lines",
                        )
                        .at(Span::new(start, cursor)));
                    }
                    if byte == b'\\' {
                        cursor += 1;
                        let escaped = *bytes.get(cursor).ok_or_else(|| {
                            FormulaError::new(ErrorKind::Lex, "unterminated string escape")
                                .at(Span::new(start, cursor))
                        })?;
                        match escaped {
                            b'\\' => value.push('\\'),
                            b'\'' => value.push('\''),
                            b'\"' => value.push('\"'),
                            b'n' => value.push('\n'),
                            b't' => value.push('\t'),
                            _ => {
                                return Err(FormulaError::new(
                                    ErrorKind::Lex,
                                    format!("unsupported string escape \\{}", escaped as char),
                                )
                                .at(Span::new(cursor - 1, cursor + 1)));
                            }
                        }
                        cursor += 1;
                    } else if byte.is_ascii() {
                        value.push(byte as char);
                        cursor += 1;
                    } else {
                        let ch = source[cursor..].chars().next().ok_or_else(|| {
                            FormulaError::new(ErrorKind::Lex, "invalid UTF-8 boundary")
                        })?;
                        value.push(ch);
                        cursor += ch.len_utf8();
                    }
                    if value.len() > limits.max_source_bytes {
                        return Err(FormulaError::new(
                            ErrorKind::Limit,
                            "string literal is too long",
                        )
                        .at(Span::new(start, cursor)));
                    }
                }
                if !closed {
                    return Err(
                        FormulaError::new(ErrorKind::Lex, "unterminated string literal")
                            .at(Span::new(start, bytes.len())),
                    );
                }
                tokens.push(Token {
                    kind: TokenKind::Text(value),
                    span: Span::new(start, cursor),
                });
            }
            b'+' => push_one(&mut tokens, TokenKind::Plus, &mut cursor),
            b'-' => push_one(&mut tokens, TokenKind::Minus, &mut cursor),
            b'*' => push_one(&mut tokens, TokenKind::Star, &mut cursor),
            b'/' => push_one(&mut tokens, TokenKind::Slash, &mut cursor),
            b'^' => push_one(&mut tokens, TokenKind::Caret, &mut cursor),
            b'(' => push_one(&mut tokens, TokenKind::LeftParen, &mut cursor),
            b')' => push_one(&mut tokens, TokenKind::RightParen, &mut cursor),
            b',' => push_one(&mut tokens, TokenKind::Comma, &mut cursor),
            b'=' => push_two(
                bytes,
                &mut tokens,
                &mut cursor,
                b'=',
                TokenKind::Eq,
                TokenKind::Assign,
            ),
            b'!' => push_two(
                bytes,
                &mut tokens,
                &mut cursor,
                b'=',
                TokenKind::NotEq,
                TokenKind::Not,
            ),
            b'<' => push_two(
                bytes,
                &mut tokens,
                &mut cursor,
                b'=',
                TokenKind::LessEq,
                TokenKind::Less,
            ),
            b'>' => push_two(
                bytes,
                &mut tokens,
                &mut cursor,
                b'=',
                TokenKind::GreaterEq,
                TokenKind::Greater,
            ),
            b'&' if bytes.get(cursor + 1) == Some(&b'&') => {
                cursor += 2;
                tokens.push(Token {
                    kind: TokenKind::And,
                    span: Span::new(start, cursor),
                });
            }
            b'|' if bytes.get(cursor + 1) == Some(&b'|') => {
                cursor += 2;
                tokens.push(Token {
                    kind: TokenKind::Or,
                    span: Span::new(start, cursor),
                });
            }
            _ => {
                let ch = source[cursor..]
                    .chars()
                    .next()
                    .ok_or_else(|| FormulaError::new(ErrorKind::Lex, "invalid UTF-8 boundary"))?;
                return Err(FormulaError::new(
                    ErrorKind::Lex,
                    format!("unexpected character '{ch}'"),
                )
                .at(Span::new(cursor, cursor + ch.len_utf8())));
            }
        }
    }
    tokens.push(Token {
        kind: TokenKind::Eof,
        span: Span::new(source.len(), source.len()),
    });
    Ok(tokens)
}

fn skip_comment(bytes: &[u8], cursor: &mut usize) {
    while *cursor < bytes.len() && bytes[*cursor] != b'\n' {
        *cursor += 1;
    }
}

fn scan_number(bytes: &[u8], mut cursor: usize) -> usize {
    while bytes.get(cursor).is_some_and(u8::is_ascii_digit) {
        cursor += 1;
    }
    if bytes.get(cursor) == Some(&b'.') {
        cursor += 1;
        while bytes.get(cursor).is_some_and(u8::is_ascii_digit) {
            cursor += 1;
        }
    }
    if matches!(bytes.get(cursor), Some(b'e' | b'E')) {
        cursor += 1;
        if matches!(bytes.get(cursor), Some(b'+' | b'-')) {
            cursor += 1;
        }
        while bytes.get(cursor).is_some_and(u8::is_ascii_digit) {
            cursor += 1;
        }
    }
    cursor
}

fn push_one(tokens: &mut Vec<Token>, kind: TokenKind, cursor: &mut usize) {
    let start = *cursor;
    *cursor += 1;
    tokens.push(Token {
        kind,
        span: Span::new(start, *cursor),
    });
}
fn push_two(
    bytes: &[u8],
    tokens: &mut Vec<Token>,
    cursor: &mut usize,
    second: u8,
    double: TokenKind,
    single: TokenKind,
) {
    let start = *cursor;
    *cursor += 1;
    let kind = if bytes.get(*cursor) == Some(&second) {
        *cursor += 1;
        double
    } else {
        single
    };
    tokens.push(Token {
        kind,
        span: Span::new(start, *cursor),
    });
}
