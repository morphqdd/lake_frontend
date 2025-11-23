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
        assert_eq!(lake_parser::ident("main"), Ok(Ident::new("main")));
        assert_eq!(lake_parser::ident("Main"), Ok(Ident::new("Main")));
        assert_eq!(lake_parser::ident("_main"), Ok(Ident::new("_main")));
        assert_eq!(lake_parser::ident("a_main"), Ok(Ident::new("a_main")));
        assert_eq!(lake_parser::ident("A_main"), Ok(Ident::new("A_main")));
        assert_eq!(lake_parser::ident("A_M"), Ok(Ident::new("A_M")));
        assert_eq!(lake_parser::ident("AM"), Ok(Ident::new("AM")));
    }
}
