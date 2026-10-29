/// Split a submitted slash invocation without changing its name or argument text.
pub(crate) fn parse_slash_command(input: &str) -> Option<(&str, Option<&str>)> {
    let body = input.trim_start().strip_prefix('/')?;
    let name_end = body.find(char::is_whitespace).unwrap_or(body.len());
    if name_end == 0 {
        return None;
    }
    let (name, suffix) = body.split_at(name_end);
    let args = suffix.trim();
    Some((name, (!args.is_empty()).then_some(args)))
}

#[cfg(test)]
mod tests {
    use super::parse_slash_command;

    #[test]
    fn borrowed_lexical_boundary_preserves_name_and_suffix() {
        for (input, expected) in [
            ("/logout openai", Some(("logout", Some("openai")))),
            ("  /logout openai  ", Some(("logout", Some("openai")))),
            ("/logout\t\topenai", Some(("logout", Some("openai")))),
            (
                "/export  résumé notes.md  ",
                Some(("export", Some("résumé notes.md"))),
            ),
            (
                "/fanout 3  inspect\t模块\nthen report ",
                Some(("fanout", Some("3  inspect\t模块\nthen report"))),
            ),
            (
                "/deploy:staging  \"a  b\" \\x ",
                Some(("deploy:staging", Some("\"a  b\" \\x"))),
            ),
            ("/éclair\u{2003}résumé", Some(("éclair", Some("résumé")))),
            (
                "\u{2003}/accounts\u{2003}openai\u{2003}",
                Some(("accounts", Some("openai"))),
            ),
            ("/accounts", Some(("accounts", None))),
            ("/accounts  ", Some(("accounts", None))),
            ("/accounts\t\n", Some(("accounts", None))),
            ("/Logout", Some(("Logout", None))),
            ("/ logout openai", None),
            ("/\tlogout openai", None),
            ("/\nlogout openai", None),
            ("/\u{2003}logout openai", None),
            ("/", None),
            ("/ \t", None),
            ("", None),
            (" \t\n", None),
            ("hello /logout openai", None),
        ] {
            assert_eq!(parse_slash_command(input), expected, "input: {input:?}");
        }
    }
}
