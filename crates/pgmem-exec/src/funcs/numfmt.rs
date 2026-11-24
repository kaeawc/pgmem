//! Numeric `to_char` / `to_number`: a port of the number half of Postgres' `formatting.c`
//! (`NUMDesc_prepare`, `NUM_processor`, `NUM_numpart_to_char`) in the C locale: `9` / `0`
//! digits, `.` / `D` decimal point, `,` / `G` group separator, `S` / `MI` / `PL` / `SG` / `PR`
//! signs, `FM` fill mode, `L` currency, `RN` roman numerals, `TH` ordinals, `V` shifts and
//! `EEEE` scientific notation.

use pgmem_types::{Numeric, SqlError, SqlResult, Value, code};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Key {
    Comma,
    Dec,
    Zero,
    Nine,
    B,
    C,
    D,
    E,
    Fm,
    G,
    L,
    Mi,
    Pl,
    Pr,
    Rn,
    RnLower,
    Sg,
    Sp,
    S,
    Th,
    ThLower,
    V,
}

/// `NUM_keywords`, in table order (the first match at a position wins).
const KEYWORDS: &[(&str, Key)] = &[
    (",", Key::Comma),
    (".", Key::Dec),
    ("0", Key::Zero),
    ("9", Key::Nine),
    ("B", Key::B),
    ("C", Key::C),
    ("D", Key::D),
    ("EEEE", Key::E),
    ("FM", Key::Fm),
    ("G", Key::G),
    ("L", Key::L),
    ("MI", Key::Mi),
    ("PL", Key::Pl),
    ("PR", Key::Pr),
    ("RN", Key::Rn),
    ("SG", Key::Sg),
    ("SP", Key::Sp),
    ("S", Key::S),
    ("TH", Key::Th),
    ("V", Key::V),
    ("b", Key::B),
    ("c", Key::C),
    ("d", Key::D),
    ("eeee", Key::E),
    ("fm", Key::Fm),
    ("g", Key::G),
    ("l", Key::L),
    ("mi", Key::Mi),
    ("pl", Key::Pl),
    ("pr", Key::Pr),
    ("rn", Key::RnLower),
    ("sg", Key::Sg),
    ("sp", Key::Sp),
    ("s", Key::S),
    ("th", Key::ThLower),
    ("v", Key::V),
];

#[derive(Clone, Debug, PartialEq)]
enum Node {
    Action(Key),
    Char(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
enum LSign {
    #[default]
    None,
    Pre,
    Post,
}

/// `NUMDesc`.
#[derive(Clone, Debug, Default)]
struct Desc {
    pre: i32,
    post: i32,
    lsign: LSign,
    decimal: bool,
    zero: bool,
    fill: bool,
    bracket: bool,
    minus: bool,
    plus: bool,
    roman: bool,
    multi_flag: bool,
    eeee: bool,
    pre_lsign_num: i32,
    multi: i32,
    zero_start: i32,
    zero_end: i32,
}

fn syntax(msg: &str) -> SqlError {
    SqlError::new(code::SYNTAX_ERROR, msg.to_owned())
}

/// `parse_format` + `NUMDesc_prepare`.
fn parse(fmt: &str) -> SqlResult<(Vec<Node>, Desc)> {
    let mut nodes = Vec::new();
    let mut d = Desc::default();
    let mut rest = fmt;
    while let Some(c) = rest.chars().next() {
        if let Some((kw, key)) = KEYWORDS.iter().find(|(k, _)| rest.starts_with(k)) {
            prepare(&mut d, *key)?;
            nodes.push(Node::Action(*key));
            rest = &rest[kw.len()..];
            continue;
        }
        if c == '"' {
            // Quoted literal text.
            let mut chars = rest[1..].char_indices();
            let mut text = String::new();
            let mut end = rest.len();
            while let Some((i, ch)) = chars.next() {
                match ch {
                    '"' => {
                        end = i + 2;
                        break;
                    }
                    '\\' => {
                        if let Some((_, n)) = chars.next() {
                            text.push(n);
                        }
                    }
                    ch => text.push(ch),
                }
            }
            for ch in text.chars() {
                nodes.push(Node::Char(ch.to_string()));
            }
            rest = &rest[end.min(rest.len())..];
            continue;
        }
        if c == '\\' && rest[1..].starts_with('"') {
            nodes.push(Node::Char("\"".into()));
            rest = &rest[2..];
            continue;
        }
        nodes.push(Node::Char(c.to_string()));
        rest = &rest[c.len_utf8()..];
    }
    if d.roman && (d.decimal || d.zero || d.bracket || d.minus || d.plus || d.multi_flag || d.eeee || d.lsign != LSign::None) {
        return Err(syntax("\"RN\" is incompatible with other formats").with_detail("\"RN\" may only be used together with \"FM\"."));
    }
    Ok((nodes, d))
}

fn prepare(d: &mut Desc, key: Key) -> SqlResult<()> {
    match key {
        Key::Nine => {
            if d.bracket {
                return Err(syntax("\"9\" must be ahead of \"PR\""));
            }
            if d.multi_flag {
                d.multi += 1;
            } else if d.decimal {
                d.post += 1;
            } else {
                d.pre += 1;
            }
        }
        Key::Zero => {
            if d.bracket {
                return Err(syntax("\"0\" must be ahead of \"PR\""));
            }
            if !d.zero && !d.decimal {
                d.zero = true;
                d.zero_start = d.pre + 1;
            }
            if d.decimal {
                d.post += 1;
            } else {
                d.pre += 1;
            }
            d.zero_end = d.pre + d.post;
        }
        Key::D | Key::Dec => {
            if d.decimal {
                return Err(syntax("multiple decimal points"));
            }
            if d.multi_flag {
                return Err(syntax("cannot use \"V\" and decimal point together"));
            }
            d.decimal = true;
        }
        Key::Fm => d.fill = true,
        Key::S => {
            if d.lsign != LSign::None {
                return Err(syntax("cannot use \"S\" twice"));
            }
            if d.plus || d.minus || d.bracket {
                return Err(syntax("cannot use \"S\" and \"PL\"/\"MI\"/\"SG\"/\"PR\" together"));
            }
            if d.decimal {
                d.lsign = LSign::Post;
            } else {
                d.lsign = LSign::Pre;
                d.pre_lsign_num = d.pre;
            }
        }
        Key::Mi => {
            if d.lsign != LSign::None {
                return Err(syntax("cannot use \"S\" and \"MI\" together"));
            }
            d.minus = true;
        }
        Key::Pl => {
            if d.lsign != LSign::None {
                return Err(syntax("cannot use \"S\" and \"PL\" together"));
            }
            d.plus = true;
        }
        Key::Sg => {
            if d.lsign != LSign::None {
                return Err(syntax("cannot use \"S\" and \"SG\" together"));
            }
            d.minus = true;
            d.plus = true;
        }
        Key::Pr => {
            if d.lsign != LSign::None || d.plus || d.minus {
                return Err(syntax("cannot use \"PR\" and \"S\"/\"PL\"/\"MI\"/\"SG\" together"));
            }
            d.bracket = true;
        }
        Key::Rn | Key::RnLower => {
            if d.roman {
                return Err(syntax("cannot use \"RN\" twice"));
            }
            d.roman = true;
        }
        Key::V => {
            if d.decimal {
                return Err(syntax("cannot use \"V\" and decimal point together"));
            }
            d.multi_flag = true;
        }
        Key::E => {
            if d.eeee {
                return Err(syntax("cannot use \"EEEE\" twice"));
            }
            if d.fill || d.lsign != LSign::None || d.bracket || d.minus || d.plus || d.roman || d.multi_flag {
                return Err(syntax("\"EEEE\" is incompatible with other formats")
                    .with_detail("\"EEEE\" may only be used together with digit and decimal point patterns."));
            }
            d.eeee = true;
        }
        _ => {}
    }
    Ok(())
}

/// `int_to_roman`: 1..=3999, else 15 `#`.
fn roman(n: i64) -> String {
    if !(1..=3999).contains(&n) {
        return "#".repeat(15);
    }
    const TABLE: [(i64, &str); 13] = [
        (1000, "M"),
        (900, "CM"),
        (500, "D"),
        (400, "CD"),
        (100, "C"),
        (90, "XC"),
        (50, "L"),
        (40, "XL"),
        (10, "X"),
        (9, "IX"),
        (5, "V"),
        (4, "IV"),
        (1, "I"),
    ];
    let mut n = n;
    let mut out = String::new();
    for (v, s) in TABLE {
        while n >= v {
            out.push_str(s);
            n -= v;
        }
    }
    out
}

/// C's `%+.*e` with the leading `+` swapped for a space.
fn sci(v: f64, post: i32) -> String {
    let s = format!("{:.*e}", usize::try_from(post.max(0)).unwrap_or(0), v);
    let (m, e) = s.split_once('e').unwrap_or((&s, "0"));
    let exp: i32 = e.parse().unwrap_or(0);
    let body = format!("{m}e{}{:02}", if exp < 0 { '-' } else { '+' }, exp.unsigned_abs());
    if body.starts_with('-') { body } else { format!(" {body}") }
}

/// The number as `NUM_processor` receives it: digits (with the decimal point, or `#` on
/// overflow), the sign, and the count of leading blank positions.
struct Prepared {
    number: String,
    sign: char,
    pre_spaces: i32,
}

fn fit(digits: String, d: &Desc, sign: char) -> Prepared {
    let pre_len = i32::try_from(digits.find('.').unwrap_or(digits.len())).unwrap_or(i32::MAX);
    if pre_len < d.pre {
        return Prepared { number: digits, sign, pre_spaces: d.pre - pre_len };
    }
    if pre_len > d.pre {
        let total = usize::try_from(d.pre + d.post + 1).unwrap_or(0);
        let mut s: Vec<char> = vec!['#'; total];
        if let Some(c) = s.get_mut(usize::try_from(d.pre).unwrap_or(0)) {
            *c = '.';
        }
        return Prepared { number: s.into_iter().collect(), sign, pre_spaces: 0 };
    }
    Prepared { number: digits, sign, pre_spaces: 0 }
}

/// `to_char(number, format)`.
pub fn to_char(v: &Value, fmt: &str) -> SqlResult<String> {
    let (nodes, mut d) = parse(fmt)?;
    let p = match v {
        Value::Int2(_) | Value::Int4(_) | Value::Int8(_) => {
            let i = v.as_i64().unwrap_or(0);
            if d.roman {
                Prepared { number: roman(i), sign: '+', pre_spaces: 0 }
            } else if d.eeee {
                #[allow(clippy::cast_precision_loss)]
                return Ok(eeee_output(&nodes, &sci(i as f64, d.post)));
            } else {
                let mut i = i128::from(i);
                if d.multi_flag {
                    i *= 10i128.pow(u32::try_from(d.multi).unwrap_or(0));
                    d.pre += d.multi;
                }
                let sign = if i < 0 { '-' } else { '+' };
                let mut digits = i.unsigned_abs().to_string();
                if d.post > 0 {
                    digits.push('.');
                    digits.push_str(&"0".repeat(usize::try_from(d.post).unwrap_or(0)));
                }
                fit(digits, &d, sign)
            }
        }
        Value::Float4(_) | Value::Float8(_) => {
            let x = v.as_f64().unwrap_or(0.0);
            let dig = if matches!(v, Value::Float4(_)) { 6 } else { 15 };
            if d.roman {
                #[allow(clippy::cast_possible_truncation)]
                Prepared { number: roman(x.round_ties_even() as i64), sign: '+', pre_spaces: 0 }
            } else if d.eeee {
                if !x.is_finite() {
                    return Ok(eeee_output(&nodes, &eeee_overflow(&d)));
                }
                return Ok(eeee_output(&nodes, &sci(x, d.post)));
            } else {
                let mut x = x;
                if d.multi_flag {
                    x *= 10f64.powi(d.multi);
                    d.pre += d.multi;
                }
                let int_len = i32::try_from(format!("{:.0}", x.abs()).len()).unwrap_or(0);
                if int_len >= dig {
                    d.post = 0;
                } else if int_len + d.post > dig {
                    d.post = dig - int_len;
                }
                let s = format!("{:.*}", usize::try_from(d.post).unwrap_or(0), x);
                let (sign, digits) = match s.strip_prefix('-') {
                    Some(r) => ('-', r.to_owned()),
                    None => ('+', s),
                };
                fit(digits, &d, sign)
            }
        }
        Value::Numeric(n) => {
            if d.roman {
                let r = n.round(0);
                Prepared { number: roman(r.to_i64().unwrap_or(0)), sign: '+', pre_spaces: 0 }
            } else if d.eeee {
                if !n.is_finite() {
                    return Ok(eeee_output(&nodes, &eeee_overflow(&d)));
                }
                return Ok(eeee_output(&nodes, &numeric_sci(n, d.post)));
            } else {
                let mut n = n.clone();
                if d.multi_flag {
                    n = n.mul(&Numeric::from_i64(10i64.pow(u32::try_from(d.multi).unwrap_or(0))));
                    d.pre += d.multi;
                }
                let s = n.round(d.post).to_string();
                let (sign, digits) = match s.strip_prefix('-') {
                    Some(r) => ('-', r.to_owned()),
                    None => ('+', s),
                };
                fit(digits, &d, sign)
            }
        }
        _ => return Err(SqlError::internal("to_char of a non-number")),
    };
    Ok(process(&nodes, &mut d, &p))
}

/// EEEE output for NaN / infinity: `#`s around the decimal point.
fn eeee_overflow(d: &Desc) -> String {
    let mut s: Vec<char> = vec!['#'; usize::try_from(d.pre + d.post + 6).unwrap_or(0)];
    if let Some(c) = s.first_mut() {
        *c = ' ';
    }
    if let Some(c) = s.get_mut(usize::try_from(d.pre + 1).unwrap_or(0)) {
        *c = '.';
    }
    s.into_iter().collect()
}

/// `numeric_out_sci`: `d.ddde[+-]NN` with `post` fraction digits (no sign for positives).
fn numeric_sci(n: &Numeric, post: i32) -> String {
    if n.is_zero() {
        let frac = if post > 0 { format!(".{}", "0".repeat(usize::try_from(post).unwrap_or(0))) } else { String::new() };
        return format!(" 0{frac}e+00");
    }
    let s = n.abs().to_string();
    let (int, frac) = s.split_once('.').unwrap_or((&s, ""));
    let digits: String = format!("{int}{frac}");
    let lead = digits.find(|c: char| c != '0').unwrap_or(0);
    let exp = i32::try_from(int.len()).unwrap_or(0) - 1 - i32::try_from(lead).unwrap_or(0);
    // Shift to one digit before the point, then round to `post` places.
    // Like Postgres, a mantissa that rounds up to 10 is not renormalized (`10.00e+01`).
    let rounded = n.abs().mul(&pow10_numeric(-exp)).round(post);
    let sign = if n.signum() < 0 { "-" } else { " " };
    format!("{sign}{rounded}e{}{:02}", if exp < 0 { '-' } else { '+' }, exp.unsigned_abs())
}

fn pow10_numeric(e: i32) -> Numeric {
    if e >= 0 {
        Numeric::parse(&format!("1{}", "0".repeat(usize::try_from(e).unwrap_or(0)))).unwrap_or_else(|_| Numeric::from_i64(1))
    } else {
        Numeric::parse(&format!("0.{}1", "0".repeat(usize::try_from(-e - 1).unwrap_or(0)))).unwrap_or_else(|_| Numeric::from_i64(1))
    }
}

/// With EEEE the number is copied whole, then literal text around it.
fn eeee_output(nodes: &[Node], number: &str) -> String {
    let mut out = String::new();
    let mut wrote = false;
    for n in nodes {
        match n {
            Node::Char(c) => out.push_str(c),
            Node::Action(Key::Nine | Key::Zero | Key::Dec | Key::D | Key::E) => {
                if !wrote {
                    out.push_str(number);
                    wrote = true;
                }
            }
            Node::Action(_) => {}
        }
    }
    out
}

/// `get_th`: the ordinal suffix of the number's last digit.
fn ordinal(number: &str, upper: bool) -> &'static str {
    let b = number.as_bytes();
    let last = b.last().copied().unwrap_or(b'0');
    let teen = b.len() > 1 && b[b.len() - 2] == b'1';
    let s = match (teen, last) {
        (false, b'1') => "st",
        (false, b'2') => "nd",
        (false, b'3') => "rd",
        _ => "th",
    };
    if upper {
        match s {
            "st" => "ST",
            "nd" => "ND",
            "rd" => "RD",
            _ => "TH",
        }
    } else {
        s
    }
}

/// `NUM_processor` for to_char.
fn process(nodes: &[Node], d: &mut Desc, p: &Prepared) -> String {
    let mut st = Proc {
        number: p.number.as_bytes().to_vec(),
        number_p: 0,
        sign: p.sign,
        sign_wrote: false,
        num_in: false,
        num_curr: 0,
        num_count: 0,
        out_pre_spaces: 0,
        last_relevant: None,
        out: String::new(),
    };
    if d.zero_start > 0 {
        d.zero_start -= 1;
    }
    if d.roman {
        d.lsign = LSign::None;
        d.pre_lsign_num = 0;
        d.post = 0;
        d.pre = 0;
        st.sign = '\0';
        let fill = d.fill;
        *d = Desc { fill, roman: true, ..Desc::default() };
    }
    // Sign handling.
    if d.plus || d.minus {
        st.sign_wrote = !(d.plus && !d.minus);
    } else {
        if st.sign != '-' && d.fill {
            d.bracket = false;
        }
        st.sign_wrote = st.sign == '+' && d.fill && d.lsign == LSign::None;
        if d.lsign == LSign::Pre && d.pre == d.pre_lsign_num {
            d.lsign = LSign::Post;
        }
    }
    if d.roman {
        st.sign_wrote = true;
    }
    st.num_count = d.post + d.pre - 1;
    st.out_pre_spaces = if d.roman { 0 } else { p.pre_spaces };
    if d.fill && d.decimal {
        st.last_relevant = last_relevant_decnum(&st.number);
        if let Some(lr) = st.last_relevant
            && d.zero_end > st.out_pre_spaces
        {
            let last_zero_pos = (st.number.len() as i32 - 1).min(d.zero_end - st.out_pre_spaces);
            if let Ok(lz) = usize::try_from(last_zero_pos)
                && lr < lz
            {
                st.last_relevant = Some(lz);
            }
        }
    }
    if !st.sign_wrote && st.out_pre_spaces == 0 {
        st.num_count += 1;
    }
    for n in nodes {
        match n {
            Node::Char(c) => st.out.push_str(c),
            Node::Action(key) => match key {
                Key::Nine | Key::Zero | Key::Dec | Key::D => st.numpart(d, *key),
                Key::Comma | Key::G => {
                    if !st.num_in {
                        if !d.fill {
                            st.out.push(' ');
                        }
                    } else {
                        st.out.push(',');
                    }
                }
                Key::L => st.out.push(' '),
                Key::Rn | Key::RnLower => {
                    let r = String::from_utf8_lossy(&st.number).into_owned();
                    let r = if *key == Key::RnLower { r.to_ascii_lowercase() } else { r };
                    if d.fill {
                        st.out.push_str(&r);
                    } else {
                        st.out.push_str(&format!("{r:>15}"));
                    }
                }
                Key::Th | Key::ThLower => {
                    if d.roman || st.number.first() == Some(&b'#') || st.sign == '-' || d.decimal {
                        continue;
                    }
                    let num = String::from_utf8_lossy(&st.number).into_owned();
                    st.out.push_str(ordinal(&num, *key == Key::Th));
                }
                Key::Mi => {
                    if st.sign == '-' {
                        st.out.push('-');
                    } else if !d.fill {
                        st.out.push(' ');
                    }
                }
                Key::Pl => {
                    if st.sign == '+' {
                        st.out.push('+');
                    } else if !d.fill {
                        st.out.push(' ');
                    }
                }
                Key::Sg => st.out.push(st.sign),
                _ => {}
            },
        }
    }
    st.out
}

fn last_relevant_decnum(num: &[u8]) -> Option<usize> {
    let dot = num.iter().position(|c| *c == b'.')?;
    let mut result = dot;
    for (i, c) in num.iter().enumerate().skip(dot + 1) {
        if *c != b'0' {
            result = i;
        }
    }
    Some(result)
}

struct Proc {
    number: Vec<u8>,
    number_p: usize,
    sign: char,
    sign_wrote: bool,
    num_in: bool,
    num_curr: i32,
    num_count: i32,
    out_pre_spaces: i32,
    last_relevant: Option<usize>,
    out: String,
}

impl Proc {
    fn cur(&self) -> u8 {
        self.number.get(self.number_p).copied().unwrap_or(0)
    }

    /// `IS_PREDEC_SPACE`: "0.1" under "9.9" prints " .1".
    fn predec_space(&self, d: &Desc) -> bool {
        !d.zero && self.number_p == 0 && self.number.first() == Some(&b'0') && d.post != 0
    }

    fn last_relevant_is_dot(&self) -> bool {
        self.last_relevant.is_some_and(|i| self.number.get(i) == Some(&b'.'))
    }

    /// `NUM_numpart_to_char`.
    fn numpart(&mut self, d: &Desc, key: Key) {
        if d.roman {
            return;
        }
        if !self.sign_wrote
            && (self.num_curr >= self.out_pre_spaces || (d.zero && d.zero_start == self.num_curr))
            && (!self.predec_space(d) || self.last_relevant_is_dot())
        {
            if d.lsign != LSign::None {
                if d.lsign == LSign::Pre {
                    self.out.push(if self.sign == '-' { '-' } else { '+' });
                    self.sign_wrote = true;
                }
            } else if d.bracket {
                self.out.push(if self.sign == '+' { ' ' } else { '<' });
                self.sign_wrote = true;
            } else if self.sign == '+' {
                if !d.fill {
                    self.out.push(' ');
                }
                self.sign_wrote = true;
            } else if self.sign == '-' {
                self.out.push('-');
                self.sign_wrote = true;
            }
        }
        if self.num_curr < self.out_pre_spaces && (d.zero_start > self.num_curr || !d.zero) {
            if !d.fill {
                self.out.push(' ');
            }
        } else if d.zero && self.num_curr < self.out_pre_spaces && d.zero_start <= self.num_curr {
            self.out.push('0');
            self.num_in = true;
        } else {
            if self.cur() == b'.' {
                if !self.last_relevant_is_dot() || d.fill {
                    self.out.push('.');
                }
            } else if self.last_relevant.is_some_and(|lr| self.number_p > lr) && key != Key::Zero {
                // Trailing zeros after the last relevant digit are dropped in fill mode.
            } else if self.predec_space(d) {
                if !d.fill {
                    self.out.push(' ');
                } else if self.last_relevant_is_dot() {
                    self.out.push('0');
                }
            } else if self.cur() != 0 {
                self.out.push(char::from(self.cur()));
                self.num_in = true;
            }
            if self.cur() != 0 {
                self.number_p += 1;
            }
        }
        let mut end = self.num_count + i32::from(self.out_pre_spaces != 0) + i32::from(d.decimal);
        if self.last_relevant.is_some_and(|lr| lr == self.number_p) {
            end = self.num_curr;
        }
        if self.num_curr + 1 == end {
            if self.sign_wrote && d.bracket {
                self.out.push(if self.sign == '+' { ' ' } else { '>' });
            } else if d.lsign == LSign::Post {
                self.out.push(if self.sign == '-' { '-' } else { '+' });
            }
        }
        self.num_curr += 1;
    }
}

/// `to_number(text, format)`: digits read at the format's digit positions, the decimal point
/// at `D` / `.`, a sign from `-`, `+`, `<...>` or a trailing `-`; the result has the
/// format's scale (`numeric_in` with the format's precision and scale).
pub fn to_number(input: &str, fmt: &str) -> SqlResult<Numeric> {
    let (nodes, d) = parse(fmt)?;
    if d.eeee {
        return Err(SqlError::feature_not_supported("\"EEEE\" not supported for input"));
    }
    if d.roman {
        return Err(SqlError::feature_not_supported("\"RN\" not supported for input"));
    }
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;
    let mut int_digits = String::new();
    let mut frac_digits = String::new();
    let mut negative = false;
    let mut in_frac = false;
    let skip_noise = |i: &mut usize, negative: &mut bool| {
        while *i < chars.len() && !chars[*i].is_ascii_digit() && chars[*i] != '.' {
            if chars[*i] == '-' || chars[*i] == '<' || chars[*i] == '(' {
                *negative = true;
            }
            *i += 1;
        }
    };
    for n in &nodes {
        match n {
            Node::Action(Key::Nine | Key::Zero) => {
                skip_noise(&mut i, &mut negative);
                if i < chars.len() && chars[i].is_ascii_digit() {
                    if in_frac {
                        frac_digits.push(chars[i]);
                    } else {
                        int_digits.push(chars[i]);
                    }
                    i += 1;
                }
            }
            Node::Action(Key::Dec | Key::D) => {
                skip_noise(&mut i, &mut negative);
                if i < chars.len() && chars[i] == '.' {
                    i += 1;
                }
                in_frac = true;
            }
            Node::Action(Key::Comma | Key::G) => {
                if i < chars.len() && chars[i] == ',' {
                    i += 1;
                }
            }
            Node::Action(Key::S | Key::Mi | Key::Sg | Key::Pl | Key::Pr) => {
                while i < chars.len() && matches!(chars[i], '-' | '+' | '<' | '>' | ' ') {
                    if chars[i] == '-' || chars[i] == '<' {
                        negative = true;
                    }
                    i += 1;
                }
            }
            Node::Action(Key::L) => {
                while i < chars.len() && !chars[i].is_ascii_digit() && chars[i] != '-' && chars[i] != '.' {
                    i += 1;
                }
            }
            Node::Action(_) => {}
            Node::Char(_) => {
                if i < chars.len() {
                    i += 1;
                }
            }
        }
    }
    // A sign after the last digit position (`12.5-`).
    if chars[i.min(chars.len())..].contains(&'-') {
        negative = true;
    }
    if int_digits.is_empty() && frac_digits.is_empty() {
        return Err(SqlError::new(code::INVALID_TEXT_REPRESENTATION, "invalid input syntax for type numeric: \" \""));
    }
    let text = format!(
        "{}{}{}{}",
        if negative { "-" } else { "" },
        if int_digits.is_empty() { "0" } else { &int_digits },
        if frac_digits.is_empty() { "" } else { "." },
        frac_digits
    );
    // The scale is the number of fraction digits actually read.
    let scale = i32::try_from(frac_digits.len()).unwrap_or(0);
    let n = Numeric::parse(&text)?.apply_typmod(d.pre + d.multi + scale, scale)?;
    if d.multi_flag {
        let factor = Numeric::from_i64(10).power(&Numeric::from_i64(-i64::from(d.multi)))?;
        return Ok(n.mul(&factor));
    }
    Ok(n)
}
