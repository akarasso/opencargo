//! Maven's version order (`ComparableVersion`): what `latest`, `release`
//! and a range like `[1.0,2.0)` are computed against.

use std::cmp::Ordering;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Item {
    Int(String),
    Str(String),
    List(Vec<Item>),
}

const QUALIFIERS: [&str; 7] = ["alpha", "beta", "milestone", "rc", "snapshot", "", "sp"];

fn qualifier_rank(q: &str) -> String {
    match QUALIFIERS.iter().position(|k| *k == q) {
        Some(i) => i.to_string(),
        None => format!("{}-{q}", QUALIFIERS.len()),
    }
}

fn string_item(s: &str, followed_by_digit: bool) -> Item {
    let s = s.to_ascii_lowercase();
    let s = if followed_by_digit && s.len() == 1 {
        match s.as_str() {
            "a" => "alpha".to_string(),
            "b" => "beta".to_string(),
            "m" => "milestone".to_string(),
            _ => s,
        }
    } else {
        s
    };
    Item::Str(match s.as_str() {
        "ga" | "final" | "release" => String::new(),
        "cr" => "rc".to_string(),
        _ => s,
    })
}

fn int_item(s: &str) -> Item {
    let trimmed = s.trim_start_matches('0');
    Item::Int(trimmed.to_string())
}

fn is_null(item: &Item) -> bool {
    match item {
        Item::Int(n) => n.is_empty(),
        Item::Str(s) => s.is_empty(),
        Item::List(l) => l.is_empty(),
    }
}

fn normalize(list: &mut Vec<Item>) {
    while list.last().is_some_and(is_null) {
        list.pop();
    }
}

fn parse(version: &str) -> Vec<Item> {
    let mut stack: Vec<Vec<Item>> = vec![Vec::new()];
    let chars: Vec<char> = version.chars().collect();
    let mut start = 0;
    let mut digit = false;
    let push = |stack: &mut Vec<Vec<Item>>, token: &str, digit: bool, next_digit: bool| {
        let item = if token.is_empty() {
            Item::Int(String::new())
        } else if digit {
            int_item(token)
        } else {
            string_item(token, next_digit)
        };
        stack.last_mut().expect("never empty").push(item);
    };
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '.' {
            let token: String = chars[start..i].iter().collect();
            push(&mut stack, &token, digit, false);
            start = i + 1;
        } else if c == '-' {
            let token: String = chars[start..i].iter().collect();
            push(&mut stack, &token, digit, false);
            start = i + 1;
            stack.push(Vec::new());
        } else if c.is_ascii_digit() {
            if !digit && i > start {
                let token: String = chars[start..i].iter().collect();
                push(&mut stack, &token, false, true);
                start = i;
                stack.push(Vec::new());
            }
            digit = true;
        } else {
            if digit && i > start {
                let token: String = chars[start..i].iter().collect();
                push(&mut stack, &token, true, false);
                start = i;
                stack.push(Vec::new());
            }
            digit = false;
        }
        i += 1;
    }
    if chars.len() > start {
        let token: String = chars[start..].iter().collect();
        push(&mut stack, &token, digit, false);
    }
    while stack.len() > 1 {
        let mut inner = stack.pop().expect("more than one");
        normalize(&mut inner);
        stack.last_mut().expect("never empty").push(Item::List(inner));
    }
    let mut top = stack.pop().expect("one left");
    normalize(&mut top);
    top
}

fn cmp_int(a: &str, b: &str) -> Ordering {
    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

fn cmp_item(a: Option<&Item>, b: Option<&Item>) -> Ordering {
    match (a, b) {
        (None, None) => Ordering::Equal,
        (Some(x), None) => cmp_null(x),
        (None, Some(y)) => cmp_null(y).reverse(),
        (Some(Item::Int(x)), Some(Item::Int(y))) => cmp_int(x, y),
        (Some(Item::Int(_)), Some(_)) => Ordering::Greater,
        (Some(Item::Str(_)), Some(Item::Int(_))) => Ordering::Less,
        (Some(Item::Str(x)), Some(Item::Str(y))) => qualifier_rank(x).cmp(&qualifier_rank(y)),
        (Some(Item::Str(_)), Some(Item::List(_))) => Ordering::Less,
        (Some(Item::List(_)), Some(Item::Int(_))) => Ordering::Less,
        (Some(Item::List(_)), Some(Item::Str(_))) => Ordering::Greater,
        (Some(Item::List(x)), Some(Item::List(y))) => cmp_lists(x, y),
    }
}

fn cmp_null(item: &Item) -> Ordering {
    match item {
        Item::Int(n) => {
            if n.is_empty() {
                Ordering::Equal
            } else {
                Ordering::Greater
            }
        }
        Item::Str(s) => qualifier_rank(s).cmp(&qualifier_rank("")),
        Item::List(l) => match l.first() {
            None => Ordering::Equal,
            Some(first) => cmp_null(first),
        },
    }
}

fn cmp_lists(a: &[Item], b: &[Item]) -> Ordering {
    for i in 0..a.len().max(b.len()) {
        let ord = cmp_item(a.get(i), b.get(i));
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
}

pub fn compare(a: &str, b: &str) -> Ordering {
    cmp_lists(&parse(a), &parse(b))
}

#[cfg(test)]
mod tests {
    use super::compare;
    use std::cmp::Ordering;

    #[test]
    fn versions_order_like_maven() {
        let ascending = [
            "1-alpha-1",
            "1-alpha2",
            "1-beta-1",
            "1-milestone-1",
            "1-rc-1",
            "1-SNAPSHOT",
            "1",
            "1-sp",
            "1.0.1",
            "1.1-SNAPSHOT",
            "1.1",
            "1.2",
            "1.10",
            "2.0-rc1",
            "2.0",
            "10.0",
        ];
        for pair in ascending.windows(2) {
            assert_eq!(compare(pair[0], pair[1]), Ordering::Less, "{} < {}", pair[0], pair[1]);
            assert_eq!(compare(pair[1], pair[0]), Ordering::Greater, "{} > {}", pair[1], pair[0]);
        }
        for (a, b) in [("1", "1.0"), ("1.0", "1.0.0"), ("1-ga", "1"), ("1-final", "1.0"), ("1-cr1", "1-rc1")] {
            assert_eq!(compare(a, b), Ordering::Equal, "{a} = {b}");
        }
    }
}
