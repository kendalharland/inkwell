//! Tiny single-pass `{{var}}` template substitution.
//!
//! No template engine is pulled in. Templates live in `src/templates/*.html`
//! and are `include_str!`'d into the binary at compile time. The
//! substitution does **one pass**, so a value containing `{{x}}` cannot be
//! re-expanded — that matters because article bodies are interpolated and
//! could in principle contain `{{...}}` patterns from the source text.

pub fn render(template: &str, vars: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        rest = &rest[start + 2..];
        let Some(end) = rest.find("}}") else {
            // Unterminated brace — keep the literal characters and stop.
            out.push_str("{{");
            out.push_str(rest);
            return out;
        };
        let key = rest[..end].trim();
        if let Some((_, v)) = vars.iter().find(|(k, _)| *k == key) {
            out.push_str(v);
        }
        // Unknown key expands to empty so a stray template variable can't
        // leak the literal `{{name}}` into the rendered page.
        rest = &rest[end + 2..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substitutes_known_vars() {
        let out = render("hi {{name}}!", &[("name", "world")]);
        assert_eq!(out, "hi world!");
    }

    #[test]
    fn unknown_vars_render_empty() {
        let out = render("a={{x}}b", &[]);
        assert_eq!(out, "a=b");
    }

    #[test]
    fn does_not_re_expand_value_with_braces() {
        let out = render("{{a}}", &[("a", "{{b}}"), ("b", "BUG")]);
        assert_eq!(out, "{{b}}", "values must not be re-substituted");
    }

    #[test]
    fn unterminated_brace_is_preserved() {
        let out = render("hi {{name", &[("name", "world")]);
        assert_eq!(out, "hi {{name");
    }

    #[test]
    fn trims_whitespace_around_key() {
        let out = render("[{{ key }}]", &[("key", "val")]);
        assert_eq!(out, "[val]");
    }
}
