//! Lexer für LSMQL.
//!
//! Wandelt LSMQL-Quelltext in eine Token-Liste. Position (line/col) wird
//! mitgeführt, damit Parse-Fehler präzise gemeldet werden können (Gate 4).

use crate::lsmql::error::{LsmqlError, LsmqlResult};

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    /// Schlüsselwörter (case-insensitive, aber wir akzeptieren nur UPPER).
    KwSelect,
    KwFrom,
    KwWhere,
    KwOrder,
    KwBy,
    KwAsc,
    KwDesc,
    KwLimit,
    KwOffset,
    KwGroup,
    KwExplain,
    KwAnd,
    KwOr,
    KwNot,
    KwIs,
    KwNull,
    KwAbsent,
    KwIn,
    KwCount,
    KwSum,
    KwAvg,
    KwMin,
    KwMax,

    /// Symbole.
    Star,
    Comma,
    LParen,
    RParen,
    /// Vergleichsoperatoren.
    Eq, // =
    Ne, // !=
    Lt, // <
    Le, // <=
    Gt, // >
    Ge, // >=

    /// Identifier (Feldname, Collection, Parametername ohne $).
    Ident(String),
    /// Parameter: `$name`.
    Param(String),
    /// String-Literal (inkl. Anführungszeichen entfernt).
    Str(String),
    /// Numerisches Literal.
    Num(f64),
    /// `true` / `false`.
    Bool(bool),

    Eof,
}

/// Position im Quelltext (1-basiert).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pos {
    pub line: usize,
    pub col: usize,
}

/// Ein Token mit Position.
#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub tok: Tok,
    pub pos: Pos,
}

struct Lexer<'a> {
    chars: &'a [u8],
    i: usize,
    line: usize,
    col: usize,
}

/// Tokenisiert `src`. Liefert den Token-Vektor (ohne trailing `Eof` separat —
/// `Eof` ist das letzte Element).
pub fn tokenize(src: &str) -> LsmqlResult<Vec<Token>> {
    let mut lx = Lexer {
        chars: src.as_bytes(),
        i: 0,
        line: 1,
        col: 1,
    };
    let mut out = Vec::new();
    loop {
        lx.skip_ws();
        if lx.i >= lx.chars.len() {
            out.push(Token {
                tok: Tok::Eof,
                pos: Pos {
                    line: lx.line,
                    col: lx.col,
                },
            });
            break;
        }
        let start = lx.pos();
        let c = lx.peek();
        let tok = match c {
            b'*' => {
                lx.bump();
                Tok::Star
            }
            b',' => {
                lx.bump();
                Tok::Comma
            }
            b'(' => {
                lx.bump();
                Tok::LParen
            }
            b')' => {
                lx.bump();
                Tok::RParen
            }
            b'=' => {
                lx.bump();
                Tok::Eq
            }
            b'<' => {
                lx.bump();
                if lx.peek() == b'=' {
                    lx.bump();
                    Tok::Le
                } else {
                    Tok::Lt
                }
            }
            b'>' => {
                lx.bump();
                if lx.peek() == b'=' {
                    lx.bump();
                    Tok::Ge
                } else {
                    Tok::Gt
                }
            }
            b'!' => {
                lx.bump();
                if lx.peek() == b'=' {
                    lx.bump();
                    Tok::Ne
                } else {
                    return Err(LsmqlError::Parse {
                        message: "unexpected '!' (expected '!=')".into(),
                        line: start.line,
                        col: start.col,
                    });
                }
            }
            b'"' | b'\'' => {
                let quote = lx.bump();
                let mut s = String::new();
                while lx.i < lx.chars.len() && lx.peek() != quote {
                    s.push(lx.bump() as char);
                }
                if lx.i >= lx.chars.len() {
                    return Err(LsmqlError::Parse {
                        message: "unterminated string literal".into(),
                        line: start.line,
                        col: start.col,
                    });
                }
                lx.bump(); // schließendes Quote
                Tok::Str(s)
            }
            b'$' => {
                lx.bump();
                let name = lx.take_while(|b| b.is_ascii_alphanumeric() || b == b'_');
                if name.is_empty() {
                    return Err(LsmqlError::Parse {
                        message: "expected parameter name after '$'".into(),
                        line: start.line,
                        col: start.col,
                    });
                }
                Tok::Param(name)
            }
            _ if c.is_ascii_digit() || (c == b'-' && lx.peek_at(1).is_ascii_digit()) => {
                let neg = if c == b'-' {
                    lx.bump();
                    "-".to_string()
                } else {
                    String::new()
                };
                let int = lx.take_while(|b| b.is_ascii_digit());
                let mut num = neg + &int;
                if lx.peek() == b'.' {
                    lx.bump();
                    let frac = lx.take_while(|b| b.is_ascii_digit());
                    num.push('.');
                    num.push_str(&frac);
                }
                match num.parse::<f64>() {
                    Ok(n) => Tok::Num(n),
                    Err(_) => {
                        return Err(LsmqlError::Parse {
                            message: format!("invalid number: {num}"),
                            line: start.line,
                            col: start.col,
                        });
                    }
                }
            }
            _ if c.is_ascii_alphabetic() || c == b'_' => {
                let word = lx.take_while(|b| b.is_ascii_alphanumeric() || b == b'_');
                kw_or_ident(&word, &start)?
            }
            other => {
                return Err(LsmqlError::Parse {
                    message: format!("unexpected character '{}'", other as char),
                    line: start.line,
                    col: start.col,
                });
            }
        };
        out.push(Token { tok, pos: start });
    }
    Ok(out)
}

fn kw_or_ident(word: &str, _pos: &Pos) -> LsmqlResult<Tok> {
    let tok = match word.to_ascii_uppercase().as_str() {
        "SELECT" => Tok::KwSelect,
        "FROM" => Tok::KwFrom,
        "WHERE" => Tok::KwWhere,
        "ORDER" => Tok::KwOrder,
        "BY" => Tok::KwBy,
        "ASC" => Tok::KwAsc,
        "DESC" => Tok::KwDesc,
        "LIMIT" => Tok::KwLimit,
        "OFFSET" => Tok::KwOffset,
        "GROUP" => Tok::KwGroup,
        "EXPLAIN" => Tok::KwExplain,
        "AND" => Tok::KwAnd,
        "OR" => Tok::KwOr,
        "NOT" => Tok::KwNot,
        "IS" => Tok::KwIs,
        "NULL" => Tok::KwNull,
        "ABSENT" => Tok::KwAbsent,
        "IN" => Tok::KwIn,
        "COUNT" => Tok::KwCount,
        "SUM" => Tok::KwSum,
        "AVG" => Tok::KwAvg,
        "MIN" => Tok::KwMin,
        "MAX" => Tok::KwMax,
        "TRUE" => return Ok(Tok::Bool(true)),
        "FALSE" => return Ok(Tok::Bool(false)),
        _ => Tok::Ident(word.to_string()),
    };
    Ok(tok)
}

impl<'a> Lexer<'a> {
    fn pos(&self) -> Pos {
        Pos {
            line: self.line,
            col: self.col,
        }
    }
    fn peek(&self) -> u8 {
        self.peek_at(0)
    }
    fn peek_at(&self, n: usize) -> u8 {
        self.chars.get(self.i + n).copied().unwrap_or(0)
    }
    fn bump(&mut self) -> u8 {
        let b = self.chars[self.i];
        self.i += 1;
        if b == b'\n' {
            self.line += 1;
            self.col = 1;
        } else {
            self.col += 1;
        }
        b
    }
    fn skip_ws(&mut self) {
        while self.i < self.chars.len() {
            let b = self.chars[self.i];
            if b == b' ' || b == b'\t' || b == b'\r' || b == b'\n' {
                self.bump();
            } else {
                break;
            }
        }
    }
    fn take_while<F: Fn(u8) -> bool>(&mut self, f: F) -> String {
        let mut s = String::new();
        while self.i < self.chars.len() && f(self.chars[self.i]) {
            s.push(self.bump() as char);
        }
        s
    }
}
