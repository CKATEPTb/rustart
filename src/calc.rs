const MAX_INPUT: usize = 512;
const MAX_DEPTH: usize = 64;

#[derive(Debug, Eq, PartialEq)]
enum CalcError {
    Invalid,
    Incomplete,
    ZeroDivisor,
    OutOfRange,
    TooLong,
    TooDeep,
    Undefined,
}

#[cfg(windows)]
pub fn evaluate(query: &str) -> Option<Result<String, &'static str>> {
    use crate::i18n::{Msg, tr};

    evaluate_math(query).map(|result| {
        result.map_err(|error| {
            tr(match error {
                CalcError::Invalid => Msg::CalcInvalid,
                CalcError::Incomplete => Msg::CalcIncomplete,
                CalcError::ZeroDivisor => Msg::CalcZeroDivisor,
                CalcError::OutOfRange => Msg::CalcOutOfRange,
                CalcError::TooLong => Msg::CalcTooLong,
                CalcError::TooDeep => Msg::CalcTooDeep,
                CalcError::Undefined => Msg::CalcUndefined,
            })
        })
    })
}

fn evaluate_math(query: &str) -> Option<Result<String, CalcError>> {
    let trimmed = query.trim();
    let explicit = trimmed.starts_with('=');
    let input = trimmed.strip_prefix('=').unwrap_or(trimmed).trim();
    if !explicit && !looks_like_math(input) {
        return None;
    }

    let result = if input.len() > MAX_INPUT {
        Err(CalcError::TooLong)
    } else {
        let normalized = input
            .replace('×', "*")
            .replace('÷', "/")
            .replace('−', "-")
            .replace(',', ".");
        let mut parser = Parser {
            input: &normalized,
            position: 0,
        };
        parser.expression(0, 0).and_then(|value| {
            if parser.peek().is_some() {
                return Err(CalcError::Invalid);
            }
            Ok(format_result(value))
        })
    };
    Some(result)
}

fn looks_like_math(mut input: &str) -> bool {
    let has_digit = input.bytes().any(|byte| byte.is_ascii_digit());
    while let Some(c) = input.chars().next() {
        if c.is_ascii_digit() || matches!(c, '.' | ',') {
            let (end, _) = numeric_literal(input);
            input = &input[end..];
        } else if c.is_whitespace() || "+-*/^%()×÷−".contains(c) {
            input = &input[c.len_utf8()..];
        } else {
            return false;
        }
    }
    has_digit
}

fn numeric_literal(input: &str) -> (usize, bool) {
    fn digits(input: &str) -> usize {
        input.bytes().take_while(u8::is_ascii_digit).count()
    }

    let mut end = digits(input);
    let mut mantissa_digits = end;
    if matches!(input.as_bytes().get(end), Some(b'.' | b',')) {
        end += 1;
        let fractional_digits = digits(&input[end..]);
        end += fractional_digits;
        mantissa_digits += fractional_digits;
    }
    if mantissa_digits != 0 && matches!(input.as_bytes().get(end), Some(b'e' | b'E')) {
        end += 1;
        if let Some(sign @ ('+' | '-' | '−')) = input[end..].chars().next() {
            end += sign.len_utf8();
        }
        let exponent_digits = digits(&input[end..]);
        return (end + exponent_digits, exponent_digits == 0);
    }
    (end, mantissa_digits == 0)
}

fn format_result(value: f64) -> String {
    if value == 0.0 {
        return "0".into();
    }
    // Keep floating-point noise out of the displayed result.
    let rounded = if value.fract() == 0.0 {
        value
    } else {
        format!("{value:.14e}").parse::<f64>().unwrap_or(value)
    };
    if rounded.abs() >= 1e15 || rounded.abs() < 1e-6 {
        format!("{rounded:e}").replace('.', ",")
    } else {
        rounded.to_string().replace('.', ",")
    }
}

struct Parser<'a> {
    input: &'a str,
    position: usize,
}

type MathResult = Result<f64, CalcError>;

impl Parser<'_> {
    fn peek(&mut self) -> Option<char> {
        let rest = &self.input[self.position..];
        let trimmed = rest.trim_start();
        self.position += rest.len() - trimmed.len();
        trimmed.chars().next()
    }

    fn expression(&mut self, min_precedence: u8, depth: usize) -> MathResult {
        if depth >= MAX_DEPTH {
            return Err(CalcError::TooDeep);
        }
        let mut left = match self.peek() {
            Some(sign @ ('+' | '-')) => {
                self.position += 1;
                let value = self.expression(5, depth + 1)?;
                if sign == '-' { -value } else { value }
            }
            Some('(') => {
                self.position += 1;
                let value = self.expression(0, depth + 1)?;
                match self.peek() {
                    Some(')') => self.position += 1,
                    None => return Err(CalcError::Incomplete),
                    _ => return Err(CalcError::Invalid),
                }
                value
            }
            Some('0'..='9' | '.') => self.number()?,
            Some(_) => return Err(CalcError::Invalid),
            None => return Err(CalcError::Incomplete),
        };

        while let Some(operator) = self.peek() {
            let (precedence, rhs_precedence) = match operator {
                '+' | '-' => (1, 2),
                '*' | '/' | '%' => (3, 4),
                '^' => (5, 5),
                _ => break,
            };
            if precedence < min_precedence {
                break;
            }
            self.position += 1;
            let right = self.expression(rhs_precedence, depth + 1)?;
            left = match operator {
                '+' => left + right,
                '-' => left - right,
                '*' => left * right,
                '/' | '%' if right == 0.0 => return Err(CalcError::ZeroDivisor),
                '/' => left / right,
                '%' => left % right,
                '^' => left.powf(right),
                _ => unreachable!(),
            };
            if left.is_nan() {
                return Err(CalcError::Undefined);
            }
            if !left.is_finite() {
                return Err(CalcError::OutOfRange);
            }
        }
        Ok(left)
    }

    fn number(&mut self) -> MathResult {
        let remaining = &self.input[self.position..];
        let (end, incomplete) = numeric_literal(remaining);
        self.position += end;
        if incomplete {
            return Err(if remaining[end..].trim().is_empty() {
                CalcError::Incomplete
            } else {
                CalcError::Invalid
            });
        }
        let value: f64 = remaining[..end].parse().map_err(|_| CalcError::Invalid)?;
        if !value.is_finite() {
            return Err(CalcError::OutOfRange);
        }
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(expression: &str) -> String {
        evaluate_math(expression).unwrap().unwrap()
    }

    #[test]
    fn numeric_app_names_are_search_queries() {
        for query in [
            "7-Zip",
            "1Password",
            "3D Viewer",
            "3E Viewer",
            "E3",
            "Visual Studio 2022",
            "Windows 11",
        ] {
            assert_eq!(evaluate_math(query), None);
        }
    }

    #[test]
    fn respects_operator_precedence_and_associativity() {
        assert_eq!(result("2 + 3 * 4"), "14");
        assert_eq!(result("(2 + 3) * 4"), "20");
        assert_eq!(result("-2^2"), "-4");
        assert_eq!(result("2^3^2"), "512");
        assert_eq!(result("2^-3"), "0,125");
        assert_eq!(result("12 / 3 / 2"), "2");
    }

    #[test]
    fn accepts_standalone_numbers_and_localized_input() {
        assert_eq!(result("42"), "42");
        assert_eq!(result("123456789012345"), "123456789012345");
        assert_eq!(result("0,1 + 0.2"), "0,3");
        assert_eq!(result("2 × 3 − 6 ÷ 2"), "3");
    }

    #[test]
    fn scientific_results_can_be_reused_as_input() {
        let copied = result("1,25 / 1000000000");
        assert_eq!(copied, "1,25e-9");
        assert_eq!(result(&format!("{copied} × 1E+9")), "1,25");
        assert_eq!(result("2,5E−2 + 1"), "1,025");
        assert_eq!(evaluate_math("1e-"), Some(Err(CalcError::Incomplete)));
        assert_eq!(evaluate_math("1e999"), Some(Err(CalcError::OutOfRange)));
    }

    #[test]
    fn reports_incomplete_and_invalid_expressions() {
        assert_eq!(evaluate_math("2+"), Some(Err(CalcError::Incomplete)));
        assert_eq!(evaluate_math("(2"), Some(Err(CalcError::Incomplete)));
        assert_eq!(evaluate_math("2 + ,"), Some(Err(CalcError::Incomplete)));
        assert_eq!(evaluate_math("2 3"), Some(Err(CalcError::Invalid)));
        assert_eq!(evaluate_math("2 / 0"), Some(Err(CalcError::ZeroDivisor)));
        assert_eq!(evaluate_math("2^1024"), Some(Err(CalcError::OutOfRange)));
        assert!(
            evaluate_math(&format!("{}1", "(".repeat(80)))
                .unwrap()
                .is_err()
        );
        assert!(evaluate_math(&"1".repeat(513)).unwrap().is_err());
    }
}
