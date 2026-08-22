//! Rekursiver Abstiegs-Parser: Token → [`Query`] AST.
//!
//! Grammatik (EBNF in `docs/design-lsmql.md` §2). `GROUP BY` wird geparst
//! (AST-Feld `group_by` gefüllt), aber die Ausführung lehnt es in v1 ab
//! (siehe `engine.rs` / `translate.rs`).

use crate::lsmql::ast::*;
use crate::lsmql::error::{LsmqlError, LsmqlResult};
use crate::lsmql::lexer::{Tok, Token};

pub fn parse(tokens: &[Token]) -> LsmqlResult<Query> {
    let mut p = Parser { toks: tokens, i: 0 };
    p.parse_query()
}

struct Parser<'a> {
    toks: &'a [Token],
    i: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Tok {
        self.toks
            .get(self.i)
            .map(|t| t.tok.clone())
            .unwrap_or(Tok::Eof)
    }
    fn pos(&self) -> crate::lsmql::lexer::Pos {
        self.toks
            .get(self.i)
            .map(|t| t.pos)
            .unwrap_or(crate::lsmql::lexer::Pos { line: 0, col: 0 })
    }
    fn bump(&mut self) -> Tok {
        let t = self.peek();
        if !matches!(t, Tok::Eof) {
            self.i += 1;
        }
        t
    }
    fn expect(&mut self, want: &Tok) -> LsmqlResult<()> {
        if self.peek() == *want {
            self.bump();
            Ok(())
        } else {
            Err(LsmqlError::Parse {
                message: format!("expected {:?}, found {:?}", want, self.peek()),
                line: self.pos().line,
                col: self.pos().col,
            })
        }
    }
    fn expect_ident(&mut self) -> LsmqlResult<String> {
        match self.bump() {
            Tok::Ident(s) => Ok(s),
            other => Err(LsmqlError::Parse {
                message: format!("expected identifier, found {:?}", other),
                line: self.pos().line,
                col: self.pos().col,
            }),
        }
    }

    fn parse_query(&mut self) -> LsmqlResult<Query> {
        let explain = if matches!(self.peek(), Tok::KwExplain) {
            self.bump();
            true
        } else {
            false
        };
        self.expect(&Tok::KwSelect)?;
        let projection = self.parse_projection()?;
        self.expect(&Tok::KwFrom)?;
        let source = self.expect_ident()?;

        let mut predicate = None;
        if matches!(self.peek(), Tok::KwWhere) {
            self.bump();
            predicate = Some(self.parse_expr()?);
        }

        let mut order_by = Vec::new();
        if matches!(self.peek(), Tok::KwOrder) {
            self.bump();
            self.expect(&Tok::KwBy)?;
            loop {
                let field = self.expect_ident()?;
                let dir = match self.peek() {
                    Tok::KwAsc => {
                        self.bump();
                        SortDir::Asc
                    }
                    Tok::KwDesc => {
                        self.bump();
                        SortDir::Desc
                    }
                    _ => SortDir::Asc,
                };
                order_by.push(OrderItem { field, dir });
                if matches!(self.peek(), Tok::Comma) {
                    self.bump();
                } else {
                    break;
                }
            }
        }

        let mut group_by = Vec::new();
        if matches!(self.peek(), Tok::KwGroup) {
            self.bump();
            self.expect(&Tok::KwBy)?;
            loop {
                group_by.push(self.expect_ident()?);
                if matches!(self.peek(), Tok::Comma) {
                    self.bump();
                } else {
                    break;
                }
            }
        }

        let mut limit = None;
        if matches!(self.peek(), Tok::KwLimit) {
            self.bump();
            limit = Some(self.parse_usize()?);
        }

        let mut offset = 0;
        if matches!(self.peek(), Tok::KwOffset) {
            self.bump();
            offset = self.parse_usize()?;
        }

        Ok(Query {
            explain,
            projection,
            source,
            predicate,
            group_by,
            order_by,
            limit,
            offset,
        })
    }

    fn parse_usize(&mut self) -> LsmqlResult<usize> {
        match self.bump() {
            Tok::Num(n) if n.fract() == 0.0 && n >= 0.0 => Ok(n as usize),
            other => Err(LsmqlError::Parse {
                message: format!("expected non-negative integer, found {:?}", other),
                line: self.pos().line,
                col: self.pos().col,
            }),
        }
    }

    fn parse_projection(&mut self) -> LsmqlResult<Projection> {
        if matches!(self.peek(), Tok::Star) {
            self.bump();
            return Ok(Projection::Star);
        }
        let mut items = Vec::new();
        loop {
            items.push(self.parse_proj_item()?);
            if matches!(self.peek(), Tok::Comma) {
                self.bump();
            } else {
                break;
            }
        }
        Ok(Projection::Items(items))
    }

    fn parse_proj_item(&mut self) -> LsmqlResult<ProjItem> {
        // Aggregat?
        match self.peek() {
            Tok::KwCount | Tok::KwSum | Tok::KwAvg | Tok::KwMin | Tok::KwMax => {
                let kind = self.parse_agg_kind()?;
                self.expect(&Tok::LParen)?;
                let field = if matches!(self.peek(), Tok::Star) {
                    self.bump();
                    None
                } else {
                    Some(self.expect_ident()?)
                };
                self.expect(&Tok::RParen)?;
                Ok(ProjItem::Agg { kind, field })
            }
            _ => {
                let field = self.expect_ident()?;
                Ok(ProjItem::Field(field))
            }
        }
    }

    fn parse_agg_kind(&mut self) -> LsmqlResult<AggKind> {
        let kind = match self.bump() {
            Tok::KwCount => AggKind::Count,
            Tok::KwSum => AggKind::Sum,
            Tok::KwAvg => AggKind::Avg,
            Tok::KwMin => AggKind::Min,
            Tok::KwMax => AggKind::Max,
            other => {
                return Err(LsmqlError::Parse {
                    message: format!("expected aggregate function, found {:?}", other),
                    line: self.pos().line,
                    col: self.pos().col,
                });
            }
        };
        Ok(kind)
    }

    // Bool-Präzedenz: OR < AND < NOT < Pred
    fn parse_expr(&mut self) -> LsmqlResult<Expr> {
        self.parse_or()
    }

    fn parse_or(&mut self) -> LsmqlResult<Expr> {
        let mut left = self.parse_and()?;
        while matches!(self.peek(), Tok::KwOr) {
            self.bump();
            let right = self.parse_and()?;
            left = match left {
                Expr::Or(mut v) => {
                    v.push(right);
                    Expr::Or(v)
                }
                other => Expr::Or(vec![other, right]),
            };
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> LsmqlResult<Expr> {
        let mut left = self.parse_not()?;
        while matches!(self.peek(), Tok::KwAnd) {
            self.bump();
            let right = self.parse_not()?;
            left = match left {
                Expr::And(mut v) => {
                    v.push(right);
                    Expr::And(v)
                }
                other => Expr::And(vec![other, right]),
            };
        }
        Ok(left)
    }

    fn parse_not(&mut self) -> LsmqlResult<Expr> {
        if matches!(self.peek(), Tok::KwNot) {
            self.bump();
            let inner = self.parse_not()?;
            Ok(Expr::Not(Box::new(inner)))
        } else {
            self.parse_pred_expr()
        }
    }

    fn parse_pred_expr(&mut self) -> LsmqlResult<Expr> {
        // Klammer?
        if matches!(self.peek(), Tok::LParen) {
            self.bump();
            let e = self.parse_expr()?;
            self.expect(&Tok::RParen)?;
            return Ok(e);
        }
        let field = self.expect_ident()?;
        // IS NULL / IS NOT NULL / IS ABSENT
        if matches!(self.peek(), Tok::KwIs) {
            self.bump();
            if matches!(self.peek(), Tok::KwNot) {
                self.bump();
                self.expect(&Tok::KwNull)?;
                return Ok(Expr::pred(Predicate::Ne(
                    field,
                    crate::lsmql::ast::Value::Null,
                )));
            }
            let tok = self.bump();
            return match tok {
                Tok::KwNull => Ok(Expr::pred(Predicate::IsNull(field))),
                Tok::KwAbsent => Ok(Expr::pred(Predicate::IsAbsent(field))),
                other => Err(LsmqlError::Parse {
                    message: format!("expected NULL/ABSENT after IS, found {:?}", other),
                    line: self.pos().line,
                    col: self.pos().col,
                }),
            };
        }
        // IN
        if matches!(self.peek(), Tok::KwIn) {
            self.bump();
            self.expect(&Tok::LParen)?;
            let mut vals = Vec::new();
            loop {
                vals.push(self.parse_value()?);
                if matches!(self.peek(), Tok::Comma) {
                    self.bump();
                } else {
                    break;
                }
            }
            self.expect(&Tok::RParen)?;
            return Ok(Expr::pred(Predicate::In(field, vals)));
        }
        // Vergleichsoperatoren
        let cmp = match self.bump() {
            Tok::Eq => Predicate::Eq,
            Tok::Ne => Predicate::Ne,
            Tok::Lt => Predicate::Lt,
            Tok::Le => Predicate::Le,
            Tok::Gt => Predicate::Gt,
            Tok::Ge => Predicate::Ge,
            other => {
                return Err(LsmqlError::Parse {
                    message: format!("expected comparison operator, found {:?}", other),
                    line: self.pos().line,
                    col: self.pos().col,
                });
            }
        };
        let v = self.parse_value()?;
        Ok(Expr::pred(cmp(field, v)))
    }

    fn parse_value(&mut self) -> LsmqlResult<crate::lsmql::ast::Value> {
        let _pos = self.pos();
        match self.bump() {
            Tok::Str(s) => Ok(crate::lsmql::ast::Value::String(s)),
            Tok::Num(n) => Ok(crate::lsmql::ast::Value::Number(n)),
            Tok::Bool(b) => Ok(crate::lsmql::ast::Value::Bool(b)),
            Tok::Param(p) => Ok(crate::lsmql::ast::Value::Param(p)),
            Tok::KwNull => Ok(crate::lsmql::ast::Value::Null),
            other => Err(LsmqlError::Parse {
                message: format!("expected value, found {:?}", other),
                line: self.pos().line,
                col: self.pos().col,
            }),
        }
    }
}
