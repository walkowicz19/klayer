//! Per-language lightweight symbol extraction used while chunking files.

pub fn detect_symbol(lang: &str, lines: &[&str]) -> (Option<String>, Option<String>) {
    for line in lines.iter().take(8) {
        if let Some((k, n)) = parse_symbol(lang, line) {
            return (Some(k), Some(n));
        }
    }
    (None, None)
}

fn parse_rust_symbol(s: &str) -> Option<(String, String)> {
    for (prefix, kind) in [
        ("pub async fn ", "fn"),
        ("pub fn ", "fn"),
        ("async fn ", "fn"),
        ("fn ", "fn"),
        ("pub struct ", "struct"),
        ("struct ", "struct"),
        ("pub enum ", "enum"),
        ("enum ", "enum"),
        ("pub trait ", "trait"),
        ("trait ", "trait"),
        ("pub type ", "type"),
        ("type ", "type"),
    ] {
        if let Some(rest) = s.strip_prefix(prefix) {
            let name = rest.split(['(', '<', '{', ' ', '\n']).next()?.to_string();
            if valid_ident(&name) {
                return Some((kind.into(), name));
            }
        }
    }
    for prefix in ["pub impl ", "impl "] {
        if let Some(rest) = s.strip_prefix(prefix) {
            let part = rest.split_once(" for ").map(|(_, b)| b).unwrap_or(rest);
            let name = part.split(['<', '{', ' ']).next()?.to_string();
            if valid_ident(&name) {
                return Some(("impl".into(), name));
            }
        }
    }
    None
}

fn parse_python_symbol(s: &str) -> Option<(String, String)> {
    for (prefix, kind) in [("async def ", "fn"), ("def ", "fn"), ("class ", "class")] {
        if let Some(rest) = s.strip_prefix(prefix) {
            let name = rest.split(['(', ':', ' ']).next()?.to_string();
            if valid_ident(&name) {
                return Some((kind.into(), name));
            }
        }
    }
    None
}

fn parse_js_ts_symbol(s: &str) -> Option<(String, String)> {
    for (prefix, kind) in [
        ("export default async function ", "fn"),
        ("export async function ", "fn"),
        ("export function ", "fn"),
        ("async function ", "fn"),
        ("function ", "fn"),
        ("export default class ", "class"),
        ("export abstract class ", "class"),
        ("export class ", "class"),
        ("abstract class ", "class"),
        ("class ", "class"),
    ] {
        if let Some(rest) = s.strip_prefix(prefix) {
            let name = rest.split(['(', '<', '{', ' ']).next()?.to_string();
            if valid_ident(&name) {
                return Some((kind.into(), name));
            }
        }
    }
    for prefix in ["export const ", "const "] {
        if let Some(rest) = s.strip_prefix(prefix) {
            if rest.contains("=>") || rest.contains("= function") {
                let name = rest.split(['=', ':', ' ']).next()?.to_string();
                if valid_ident(&name) {
                    return Some(("const".into(), name));
                }
            }
        }
    }
    None
}

fn parse_go_symbol(s: &str) -> Option<(String, String)> {
    if let Some(rest) = s.strip_prefix("func ") {
        let rest = if rest.starts_with('(') {
            rest.splitn(2, ')').nth(1)?.trim_start_matches([' ', '\t'])
        } else {
            rest
        };
        let name = rest.split(['(', ' ']).next()?.to_string();
        if valid_ident(&name) {
            return Some(("fn".into(), name));
        }
    }
    if let Some(rest) = s.strip_prefix("type ") {
        let name = rest.split([' ', '[']).next()?.to_string();
        if valid_ident(&name) {
            return Some(("type".into(), name));
        }
    }
    None
}

fn parse_jvm_dotnet_symbol(s: &str) -> Option<(String, String)> {
    if s.contains('(')
        && !s.starts_with("if ")
        && !s.starts_with("for ")
        && !s.starts_with("while ")
    {
        let before = s.split('(').next()?;
        let name = before.split_whitespace().last()?.to_string();
        if valid_ident(&name) && name.len() > 1 {
            return Some(("method".into(), name));
        }
    }
    None
}

fn parse_cobol_symbol(s: &str) -> Option<(String, String)> {
    let su = s.to_uppercase();
    for (suffix, kind) in [(" DIVISION.", "division"), (" SECTION.", "section")] {
        if su.ends_with(suffix) {
            let name = su[..su.len() - suffix.len()]
                .split_whitespace()
                .last()?
                .to_string();
            if !name.is_empty() {
                return Some((kind.into(), name));
            }
        }
    }
    if su.ends_with('.') && !su.contains(' ') && su.len() > 1 {
        let name = su.trim_end_matches('.').to_string();
        if !name.is_empty() && name.len() <= 64 {
            return Some(("paragraph".into(), name));
        }
    }
    for prefix in ["PERFORM ", "CALL "] {
        if let Some(rest) = su.strip_prefix(prefix) {
            let name = rest
                .split_whitespace()
                .next()?
                .trim_end_matches(".")
                .to_string();
            if !name.is_empty() && name.len() <= 64 {
                return Some(("call".into(), name));
            }
        }
    }
    None
}

fn parse_natural_symbol(s: &str) -> Option<(String, String)> {
    let su = s.to_uppercase();
    for (prefix, kind) in [
        ("DEFINE SUBROUTINE ", "subroutine"),
        ("DEFINE FUNCTION ", "function"),
        ("DEFINE DATA", "data-section"),
        ("DEFINE WINDOW ", "window"),
    ] {
        if su.starts_with(prefix) {
            let rest = &su[prefix.len()..];
            let name = rest.split_whitespace().next().unwrap_or("").to_string();
            if !name.is_empty() {
                return Some((kind.into(), name));
            }
            if prefix.ends_with("DATA") {
                return Some(("data-section".into(), "DATA".into()));
            }
        }
    }
    for prefix in ["SUBROUTINE ", "FUNCTION "] {
        if let Some(rest) = su.strip_prefix(prefix) {
            let name = rest.split_whitespace().next().unwrap_or("").to_string();
            if valid_ident(&name) {
                return Some(("subroutine".into(), name));
            }
        }
    }
    None
}

fn parse_rpg_symbol(s: &str) -> Option<(String, String)> {
    let su = s.to_uppercase();
    for (prefix, kind) in [
        ("DCL-PROC ", "procedure"),
        ("DCL-DS ", "data-struct"),
        ("DCL-S ", "variable"),
        ("DCL-C ", "constant"),
        ("DCL-F ", "file"),
    ] {
        if let Some(rest) = su.strip_prefix(prefix) {
            let name = rest.split_whitespace().next().unwrap_or("").to_string();
            if valid_ident(&name) {
                return Some((kind.into(), name));
            }
        }
    }
    if su.starts_with('P') && su.len() > 1 {
        let name_part: String = su.chars().skip(6).take(14).collect();
        let name = name_part.trim().to_string();
        if valid_ident(&name) {
            return Some(("procedure".into(), name));
        }
    }
    for prefix in ["BEGSR ", "BEGSR\n"] {
        if su.starts_with(prefix) {
            let name = su[prefix.len()..]
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_string();
            if valid_ident(&name) {
                return Some(("subroutine".into(), name));
            }
        }
    }
    None
}

fn parse_powerscript_symbol(s: &str) -> Option<(String, String)> {
    let sl = s.to_lowercase();
    for (prefix, kind) in [
        ("forward\n", "forward"),
        ("type ", "type"),
        ("global type ", "global-type"),
    ] {
        if sl.starts_with(prefix) {
            let rest = &s[prefix.len()..];
            let name = rest.split_whitespace().next().unwrap_or("").to_string();
            if valid_ident(&name) {
                return Some((kind.into(), name));
            }
        }
    }
    for (prefix, kind) in [
        ("public function ", "function"),
        ("private function ", "function"),
        ("protected function ", "function"),
        ("function ", "function"),
        ("public subroutine ", "subroutine"),
        ("private subroutine ", "subroutine"),
        ("subroutine ", "subroutine"),
        ("on ", "event"),
        ("event ", "event"),
    ] {
        if sl.starts_with(prefix) {
            let rest = &s[prefix.len()..];
            let name = rest.split(['(', ' ']).next().unwrap_or("").to_string();
            if valid_ident(&name) {
                return Some((kind.into(), name));
            }
        }
    }
    None
}

/// Lightweight, line-based JCL structural extraction: JOB card, EXEC
/// PGM=/PROC= steps, and DD statement names. Not a full JCL grammar (no
/// continuation-line handling) — matches the pragmatic fidelity of the
/// COBOL/RPG extractors above.
fn parse_jcl_symbol(s: &str) -> Option<(String, String)> {
    let su = s.to_uppercase();
    let su = su.strip_prefix("//").unwrap_or(&su);

    // JOB card: `JOBNAME JOB ...`
    if let Some(rest) = su.split_whitespace().collect::<Vec<_>>().get(1).copied() {
        if rest == "JOB" {
            let name = su.split_whitespace().next().unwrap_or("").to_string();
            if !name.is_empty() && name.len() <= 8 {
                return Some(("job".into(), name));
            }
        }
    }

    // EXEC PGM=xxx or EXEC PROC=xxx (step name is the token before EXEC).
    if let Some(idx) = su.find("EXEC ") {
        let before = su[..idx].split_whitespace().last();
        let after = &su[idx + "EXEC ".len()..];
        for prefix in ["PGM=", "PROC="] {
            if let Some(rest) = after.trim_start().strip_prefix(prefix) {
                let target = rest
                    .split(|c: char| c == ',' || c.is_whitespace())
                    .next()
                    .unwrap_or("")
                    .to_string();
                if !target.is_empty() {
                    let step = before.unwrap_or("").to_string();
                    let name = if step.is_empty() { target.clone() } else { step };
                    return Some(("step".into(), name));
                }
            }
        }
    }

    // DD statement: `NAME DD ...`
    if let Some(rest) = su.split_whitespace().collect::<Vec<_>>().get(1).copied() {
        if rest == "DD" {
            let name = su.split_whitespace().next().unwrap_or("").to_string();
            if !name.is_empty() && name.len() <= 8 {
                return Some(("dd".into(), name));
            }
        }
    }

    None
}

pub fn parse_symbol(lang: &str, line: &str) -> Option<(String, String)> {
    let s = line.trim();
    match lang {
        "rust" => parse_rust_symbol(s),
        "python" => parse_python_symbol(s),
        "javascript" | "typescript" | "tsx" | "jsx" => parse_js_ts_symbol(s),
        "go" => parse_go_symbol(s),
        "java" | "kotlin" | "csharp" => parse_jvm_dotnet_symbol(s),
        "cobol" => parse_cobol_symbol(s),
        "natural" => parse_natural_symbol(s),
        "rpg" => parse_rpg_symbol(s),
        "powerscript" => parse_powerscript_symbol(s),
        "jcl" => parse_jcl_symbol(s),
        _ => None,
    }
}

fn valid_ident(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars().all(|c| c.is_alphanumeric() || c == '_')
        && s.chars()
            .next()
            .map(|c| !c.is_ascii_digit())
            .unwrap_or(false)
}
