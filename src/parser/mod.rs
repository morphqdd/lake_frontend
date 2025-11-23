use crate::api::ast::Ident;

peg::parser! (
    grammar lake_parser() for str {
        pub(crate) rule ident() -> Ident = n:$(['a'..='z' | 'A'..='Z' | '_']+) {? Ok(Ident::new(n)) }
        pub(crate) rule number() -> i32 = n:$(['0'..='9']+) {? n.parse().or(Err("i32"))}
    }
);

#[cfg(test)]
mod tests {
    use crate::{api::ast::Ident, parser::lake_parser};

    #[test]
    fn number_parse_test() {
        assert_eq!(lake_parser::number("10"), Ok(10))
    }

    #[test]
    fn ident_parse_test() {
        assert_eq!(lake_parser::ident("main"), Ok(Ident::new("main")))
    }
}
