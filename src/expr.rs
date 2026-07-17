// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 TerraOps <https://terraops.org>

//! Safe band-math expression engine.
//!
//! Parse a small arithmetic expression over named bands (e.g. `(B08 - B04) / (B08 + B04)`)
//! into a flat RPN program, then evaluate it over whole band planes. The grammar is a
//! **closed whitelist** — numeric literals, band variables, `+ - * /`, unary `-`, parens,
//! and a fixed function set (`min`, `max`, `abs`, `sqrt`, `clamp`). There is **no `eval`
//! and no code execution**: an unknown name or malformed input fails at compile time.
//!
//! Evaluation is **slice-based (SoA)** for vectorization: each binary op is a tight
//! `for i { out[i] = a[i] OP b[i] }` loop the compiler auto-vectorizes (the resample SIMD
//! study showed this arithmetic is memory-bound and auto-vectorizes well). Only the bands
//! the expression references are needed at eval time, which drives band-selective decode.

/// A single RPN instruction. `LoadBand(k)` pushes the k-th **referenced** band plane (index
/// into `Program::bands_used`, not the source band number) — so eval only needs the bands
/// the expression actually uses.
#[derive(Debug, Clone, PartialEq)]
enum Op {
    LoadBand(u16),
    LoadConst(f32),
    Add,
    Sub,
    Mul,
    Div,
    Neg,
    Abs,
    Sqrt,
    Min,
    Max,
    Clamp, // clamp(x, lo, hi)
}

/// A compiled band-math expression: a flat RPN program plus the source bands it references.
pub struct Program {
    ops: Vec<Op>,
    /// Source-band indices (positions in the `band_names` passed to `compile`) that the
    /// expression references, in the order eval expects their planes. Drives band-selective
    /// decode: decode only these bands, pass their planes to `eval` in this order.
    bands_used: Vec<usize>,
}

impl Program {
    /// Compile `expr` against the available band names (case-insensitive). Band tokens must
    /// be names from `band_names`; anything else is a compile error.
    pub fn compile(expr: &str, band_names: &[&str]) -> Result<Program, String> {
        let toks = tokenize(expr)?;
        let mut p = Parser {
            toks: &toks,
            pos: 0,
            band_names,
            ops: Vec::new(),
            bands_used: Vec::new(),
        };
        p.parse_expr()?;
        if p.pos != p.toks.len() {
            return Err(format!("unexpected trailing token at {}", p.pos));
        }
        if p.ops.is_empty() {
            return Err("empty expression".into());
        }
        Ok(Program {
            ops: p.ops,
            bands_used: p.bands_used,
        })
    }

    /// Source-band indices this expression references, in the order `eval` expects planes.
    pub fn bands_used(&self) -> &[usize] {
        &self.bands_used
    }

    /// Evaluate the program over `n` pixels. `bands[k]` is the plane for `bands_used()[k]`
    /// (each length `n`). Returns one `f32` per pixel. Division by zero yields inf/NaN — the
    /// caller's nodata/mask step decides transparency; eval itself never panics.
    pub fn eval(&self, bands: &[&[f32]], n: usize) -> Vec<f32> {
        // A stack value: a borrowed band plane, an owned intermediate, or a broadcast scalar.
        enum Val<'a> {
            Band(&'a [f32]),
            Owned(Vec<f32>),
            Scalar(f32),
        }
        #[inline]
        fn as_slice<'a>(v: &'a Val<'a>, scratch: &'a mut Vec<f32>, n: usize) -> &'a [f32] {
            match v {
                Val::Band(s) => s,
                Val::Owned(o) => o,
                Val::Scalar(c) => {
                    scratch.clear();
                    scratch.resize(n, *c);
                    scratch
                }
            }
        }

        let mut stack: Vec<Val> = Vec::with_capacity(8);
        // Binary op with scalar fast-paths so constant operands don't allocate a full plane.
        macro_rules! binop {
            ($f:expr) => {{
                let b = stack.pop().unwrap();
                let a = stack.pop().unwrap();
                let out = match (a, b) {
                    (Val::Scalar(x), Val::Scalar(y)) => Val::Scalar($f(x, y)),
                    (a, b) => {
                        let mut sa = Vec::new();
                        let mut sb = Vec::new();
                        let av = as_slice(&a, &mut sa, n);
                        let bv = as_slice(&b, &mut sb, n);
                        let mut o = vec![0f32; n];
                        for i in 0..n {
                            o[i] = $f(av[i], bv[i]);
                        }
                        Val::Owned(o)
                    }
                };
                stack.push(out);
            }};
        }
        macro_rules! unop {
            ($f:expr) => {{
                let a = stack.pop().unwrap();
                let out = match a {
                    Val::Scalar(x) => Val::Scalar($f(x)),
                    a => {
                        let mut sa = Vec::new();
                        let av = as_slice(&a, &mut sa, n);
                        let mut o = vec![0f32; n];
                        for i in 0..n {
                            o[i] = $f(av[i]);
                        }
                        Val::Owned(o)
                    }
                };
                stack.push(out);
            }};
        }

        for op in &self.ops {
            match op {
                Op::LoadBand(k) => stack.push(Val::Band(bands[*k as usize])),
                Op::LoadConst(c) => stack.push(Val::Scalar(*c)),
                Op::Add => binop!(|x: f32, y: f32| x + y),
                Op::Sub => binop!(|x: f32, y: f32| x - y),
                Op::Mul => binop!(|x: f32, y: f32| x * y),
                Op::Div => binop!(|x: f32, y: f32| x / y),
                Op::Min => binop!(|x: f32, y: f32| x.min(y)),
                Op::Max => binop!(|x: f32, y: f32| x.max(y)),
                Op::Neg => unop!(|x: f32| -x),
                Op::Abs => unop!(|x: f32| x.abs()),
                Op::Sqrt => unop!(|x: f32| x.sqrt()),
                Op::Clamp => {
                    let hi = stack.pop().unwrap();
                    let lo = stack.pop().unwrap();
                    let x = stack.pop().unwrap();
                    let mut sx = Vec::new();
                    let mut sl = Vec::new();
                    let mut sh = Vec::new();
                    let xv = as_slice(&x, &mut sx, n);
                    let lv = as_slice(&lo, &mut sl, n);
                    let hv = as_slice(&hi, &mut sh, n);
                    let mut o = vec![0f32; n];
                    for i in 0..n {
                        o[i] = xv[i].max(lv[i]).min(hv[i]);
                    }
                    stack.push(Val::Owned(o));
                }
            }
        }

        match stack.pop() {
            Some(Val::Owned(o)) => o,
            Some(Val::Band(s)) => s.to_vec(),
            Some(Val::Scalar(c)) => vec![c; n],
            None => vec![0.0; n],
        }
    }
}

// --- lexer -----------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Num(f32),
    Ident(String),
    LParen,
    RParen,
    Comma,
    Plus,
    Minus,
    Star,
    Slash,
}

fn tokenize(s: &str) -> Result<Vec<Tok>, String> {
    let b = s.as_bytes();
    let mut i = 0;
    let mut out = Vec::new();
    while i < b.len() {
        let c = b[i];
        match c {
            _ if c.is_ascii_whitespace() => i += 1,
            b'(' => {
                out.push(Tok::LParen);
                i += 1;
            }
            b')' => {
                out.push(Tok::RParen);
                i += 1;
            }
            b',' => {
                out.push(Tok::Comma);
                i += 1;
            }
            b'+' => {
                out.push(Tok::Plus);
                i += 1;
            }
            b'-' => {
                out.push(Tok::Minus);
                i += 1;
            }
            b'*' => {
                out.push(Tok::Star);
                i += 1;
            }
            b'/' => {
                out.push(Tok::Slash);
                i += 1;
            }
            _ if c.is_ascii_digit() || c == b'.' => {
                let start = i;
                while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'.') {
                    i += 1;
                }
                // allow scientific notation: 1e-3, 2.5E6
                if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
                    i += 1;
                    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
                        i += 1;
                    }
                    while i < b.len() && b[i].is_ascii_digit() {
                        i += 1;
                    }
                }
                let text = &s[start..i];
                let v: f32 = text
                    .parse()
                    .map_err(|_| format!("invalid number '{text}'"))?;
                out.push(Tok::Num(v));
            }
            _ if c.is_ascii_alphabetic() || c == b'_' => {
                let start = i;
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                    i += 1;
                }
                out.push(Tok::Ident(s[start..i].to_string()));
            }
            _ => return Err(format!("unexpected character '{}' at {i}", c as char)),
        }
    }
    Ok(out)
}

// --- parser (recursive descent, emits RPN directly) ------------------------

struct Parser<'a> {
    toks: &'a [Tok],
    pos: usize,
    band_names: &'a [&'a str],
    ops: Vec<Op>,
    bands_used: Vec<usize>,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }
    fn bump(&mut self) -> Option<&Tok> {
        let t = self.toks.get(self.pos);
        self.pos += 1;
        t
    }

    // expr := term (('+'|'-') term)*
    fn parse_expr(&mut self) -> Result<(), String> {
        self.parse_term()?;
        loop {
            match self.peek() {
                Some(Tok::Plus) => {
                    self.pos += 1;
                    self.parse_term()?;
                    self.ops.push(Op::Add);
                }
                Some(Tok::Minus) => {
                    self.pos += 1;
                    self.parse_term()?;
                    self.ops.push(Op::Sub);
                }
                _ => break,
            }
        }
        Ok(())
    }

    // term := factor (('*'|'/') factor)*
    fn parse_term(&mut self) -> Result<(), String> {
        self.parse_factor()?;
        loop {
            match self.peek() {
                Some(Tok::Star) => {
                    self.pos += 1;
                    self.parse_factor()?;
                    self.ops.push(Op::Mul);
                }
                Some(Tok::Slash) => {
                    self.pos += 1;
                    self.parse_factor()?;
                    self.ops.push(Op::Div);
                }
                _ => break,
            }
        }
        Ok(())
    }

    // factor := '-' factor | primary
    fn parse_factor(&mut self) -> Result<(), String> {
        if let Some(Tok::Minus) = self.peek() {
            self.pos += 1;
            self.parse_factor()?;
            self.ops.push(Op::Neg);
            return Ok(());
        }
        self.parse_primary()
    }

    // primary := num | ident | func '(' args ')' | '(' expr ')'
    fn parse_primary(&mut self) -> Result<(), String> {
        match self.bump().cloned() {
            Some(Tok::Num(v)) => {
                self.ops.push(Op::LoadConst(v));
                Ok(())
            }
            Some(Tok::LParen) => {
                self.parse_expr()?;
                self.expect(Tok::RParen)
            }
            Some(Tok::Ident(name)) => {
                if let Some(Tok::LParen) = self.peek() {
                    self.parse_call(&name)
                } else {
                    self.emit_band(&name)
                }
            }
            other => Err(format!("expected value, found {other:?}")),
        }
    }

    fn parse_call(&mut self, name: &str) -> Result<(), String> {
        self.expect(Tok::LParen)?;
        // parse comma-separated args (each emits its own ops)
        let mut argc = 0;
        if !matches!(self.peek(), Some(Tok::RParen)) {
            loop {
                self.parse_expr()?;
                argc += 1;
                match self.peek() {
                    Some(Tok::Comma) => {
                        self.pos += 1;
                    }
                    _ => break,
                }
            }
        }
        self.expect(Tok::RParen)?;
        let f = name.to_ascii_lowercase();
        let (op, want) = match f.as_str() {
            "abs" => (Op::Abs, 1),
            "sqrt" => (Op::Sqrt, 1),
            "min" => (Op::Min, 2),
            "max" => (Op::Max, 2),
            "clamp" => (Op::Clamp, 3),
            _ => return Err(format!("unknown function '{name}'")),
        };
        if argc != want {
            return Err(format!("{name}() takes {want} args, got {argc}"));
        }
        self.ops.push(op);
        Ok(())
    }

    /// Resolve a band name to a referenced-band slot and emit a LoadBand.
    fn emit_band(&mut self, name: &str) -> Result<(), String> {
        let src_idx = self
            .band_names
            .iter()
            .position(|b| b.eq_ignore_ascii_case(name))
            .ok_or_else(|| format!("unknown band '{name}'"))?;
        // Map source index -> referenced-band slot (dedup, first-seen order).
        let slot = match self.bands_used.iter().position(|&u| u == src_idx) {
            Some(k) => k,
            None => {
                self.bands_used.push(src_idx);
                self.bands_used.len() - 1
            }
        };
        self.ops.push(Op::LoadBand(slot as u16));
        Ok(())
    }

    fn expect(&mut self, t: Tok) -> Result<(), String> {
        match self.bump() {
            Some(x) if *x == t => Ok(()),
            other => Err(format!("expected {t:?}, found {other:?}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BANDS: &[&str] = &["B02", "B03", "B04", "B08"];

    fn eval1(expr: &str, vals: &[(&str, f32)]) -> f32 {
        // one-pixel eval: build planes for referenced bands from `vals`
        let p = Program::compile(expr, BANDS).unwrap();
        let planes: Vec<Vec<f32>> = p
            .bands_used()
            .iter()
            .map(|&i| {
                let name = BANDS[i];
                vec![vals.iter().find(|(n, _)| *n == name).unwrap().1]
            })
            .collect();
        let refs: Vec<&[f32]> = planes.iter().map(|v| v.as_slice()).collect();
        p.eval(&refs, 1)[0]
    }

    #[test]
    fn ndvi_ratio() {
        // Red=2000, NIR=6000 -> (6000-2000)/(6000+2000) = 0.5
        let v = eval1(
            "(B08 - B04) / (B08 + B04)",
            &[("B04", 2000.0), ("B08", 6000.0)],
        );
        assert!((v - 0.5).abs() < 1e-6, "ndvi={v}");
    }

    #[test]
    fn precedence_and_parens() {
        assert!((eval1("1 + 2 * 3", &[]) - 7.0).abs() < 1e-6);
        assert!((eval1("(1 + 2) * 3", &[]) - 9.0).abs() < 1e-6);
    }

    #[test]
    fn unary_minus_and_functions() {
        assert!((eval1("-B04 + B08", &[("B04", 10.0), ("B08", 30.0)]) - 20.0).abs() < 1e-6);
        assert!((eval1("abs(B04 - B08)", &[("B04", 30.0), ("B08", 10.0)]) - 20.0).abs() < 1e-6);
        assert!((eval1("max(B04, B08)", &[("B04", 3.0), ("B08", 8.0)]) - 8.0).abs() < 1e-6);
        assert!((eval1("min(B04, B08)", &[("B04", 3.0), ("B08", 8.0)]) - 3.0).abs() < 1e-6);
        assert!((eval1("clamp(B08, 0, 1)", &[("B08", 5.0)]) - 1.0).abs() < 1e-6);
        assert!((eval1("sqrt(B08)", &[("B08", 9.0)]) - 3.0).abs() < 1e-6);
    }

    #[test]
    fn bands_used_dedup_and_order() {
        // references B08 then B04 (each twice) -> referenced order [B08(3), B04(2)]
        let p = Program::compile("(B08 - B04) / (B08 + B04)", BANDS).unwrap();
        assert_eq!(p.bands_used(), &[3, 2]);
    }

    #[test]
    fn vectorized_eval_over_planes() {
        // Multi-pixel: NDVI over 3 pixels at once (proves the slice path).
        let p = Program::compile("(B08 - B04) / (B08 + B04)", BANDS).unwrap();
        // bands_used = [B08, B04]
        let b08 = [6000.0f32, 1000.0, 3000.0];
        let b04 = [2000.0f32, 1000.0, 1000.0];
        let out = p.eval(&[&b08, &b04], 3);
        assert!((out[0] - 0.5).abs() < 1e-6);
        assert!((out[1] - 0.0).abs() < 1e-6);
        assert!((out[2] - 0.5).abs() < 1e-6);
    }

    #[test]
    fn rejects_bad_input() {
        assert!(Program::compile("(B08 - B99)", BANDS).is_err()); // unknown band
        assert!(Program::compile("B08 +", BANDS).is_err()); // dangling operator
        assert!(Program::compile("frobnicate(B08)", BANDS).is_err()); // unknown function
        assert!(Program::compile("min(B08)", BANDS).is_err()); // wrong arg count
        assert!(Program::compile("", BANDS).is_err()); // empty
        assert!(Program::compile("B08 B04", BANDS).is_err()); // trailing token
    }
}
