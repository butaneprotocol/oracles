#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Token {
    ADA,
    BTC,
    DJED,
    ENCS,
    IUSD,
    LENFI,
    MIN,
    SNEK,
    USDT,
}
impl Token {
    pub fn name(&self) -> String {
        format!("{:?}", self)
    }

    pub fn value_of(name: &str) -> Option<Token> {
        match name.to_uppercase().as_str() {
            "ADA" => Some(Token::ADA),
            "BTC" => Some(Token::BTC),
            "DJED" => Some(Token::DJED),
            "ENCS" => Some(Token::ENCS),
            "IUSD" => Some(Token::IUSD),
            "LENFI" => Some(Token::LENFI),
            "MIN" => Some(Token::MIN),
            "SNEK" => Some(Token::SNEK),
            "USDT" => Some(Token::USDT),
            _ => None,
        }
    }
}
