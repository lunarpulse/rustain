use std::fmt;
use std::io::Write as _;
use std::sync::LazyLock;

#[derive(Clone, Copy)]
pub(super) struct Decimal {
    coefficient: u128,
    exponent: i32,
}

impl Decimal {
    pub(super) fn from_f64(value: f64) -> Option<Self> {
        if !value.is_finite() || value < 0.0 {
            return None;
        }
        if value == 0.0 {
            return Some(Self {
                coefficient: 0,
                exponent: 0,
            });
        }
        // LowerExp without a precision produces the shortest round-trip decimal.
        let mut buffer = [0; 32];
        let mut tail = &mut buffer[..];
        write!(tail, "{value:e}").ok()?;
        let length = 32 - tail.len();
        let (mantissa, exponent) = std::str::from_utf8(&buffer[..length])
            .ok()?
            .split_once('e')?;
        let mut exponent = exponent.parse::<i32>().ok()?;
        let mut coefficient = 0;
        let mut fraction = false;
        for byte in mantissa.bytes() {
            if byte == b'.' {
                fraction = true;
            } else {
                coefficient = coefficient * 10 + u128::from(byte - b'0');
                exponent -= i32::from(fraction);
            }
        }
        Some(Self {
            coefficient,
            exponent,
        })
    }

    pub(super) fn scaled(self, numerator: u128, decimal_places: i32) -> Self {
        Self {
            coefficient: self.coefficient * numerator,
            exponent: self.exponent - decimal_places,
        }
    }
}

const BASE: u32 = 1_000_000_000;
const FRACTION_BLOCKS: usize = 39;
const WHOLE_BLOCKS: usize = 35;
const MIN_EXPONENT: i32 = -351;
const OVERFLOW: &str = "usage cost overflow";

/// Whole dollars and fractional dollars, in big-endian base 10^9 blocks.
/// Finite f64 inputs need at most 17 digits with scientific exponents ≥ -324;
/// per-token pricing and the cache multipliers need up to 8 more places. The
/// whole blocks cover even f64::MAX × u32::MAX / 1 million × 1.25.
#[derive(Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct Usd([u32; WHOLE_BLOCKS + FRACTION_BLOCKS]);

impl Usd {
    pub(super) const ZERO: Self = Self([0; WHOLE_BLOCKS + FRACTION_BLOCKS]);

    pub(super) fn from_f64(value: f64) -> Option<Self> {
        let mut amount = Self::ZERO;
        amount.add(Decimal::from_f64(value)?).ok()?;
        Some(amount)
    }

    pub(super) fn add_tokens(&mut self, rate: Decimal, tokens: u32) -> Result<(), &'static str> {
        self.add(rate.scaled(u128::from(tokens), 6))
    }

    fn add(&mut self, term: Decimal) -> Result<(), &'static str> {
        if term.coefficient == 0 {
            return Ok(());
        }
        let place = usize::try_from(term.exponent - MIN_EXPONENT).map_err(|_| OVERFLOW)?;
        // 17 price digits + 3 multiplier digits + 10 token digits + 8 alignment
        // digits fit in u128. Adding an existing 9-digit block cannot overflow it.
        let mut carry = term.coefficient * 10u128.pow((place % 9) as u32);
        for digit in self.0.iter_mut().rev().skip(place / 9) {
            carry += u128::from(*digit);
            *digit = (carry % u128::from(BASE)) as u32;
            carry /= u128::from(BASE);
            if carry == 0 {
                return Ok(());
            }
        }
        Err(OVERFLOW)
    }

    pub(super) fn check_range(&self) -> Result<(), &'static str> {
        static MAX_USD: LazyLock<Usd> =
            LazyLock::new(|| Usd::from_f64(f64::MAX).expect("f64 limit fits USD representation"));
        if self > &*MAX_USD {
            return Err(OVERFLOW);
        }
        Ok(())
    }
}

impl fmt::Display for Usd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (whole, fraction) = self.0.split_at(WHOLE_BLOCKS);
        let first = whole
            .iter()
            .position(|&n| n != 0)
            .unwrap_or(whole.len() - 1);
        write!(f, "{}", whole[first])?;
        for &n in &whole[first + 1..] {
            write!(f, "{n:09}")?;
        }
        if let Some(last) = fraction.iter().rposition(|&n| n != 0) {
            f.write_str(".")?;
            for &n in &fraction[..last] {
                write!(f, "{n:09}")?;
            }
            let mut n = fraction[last];
            let mut width = 9;
            while n.is_multiple_of(10) {
                n /= 10;
                width -= 1;
            }
            write!(f, "{n:0width$}")?;
        }
        Ok(())
    }
}
