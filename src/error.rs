use serde_json::Value;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub struct Error {
    pub code: i32,
    pub message: String,
    pub detail: Option<Value>,
}
impl Error {
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            detail: None,
        }
    }
    pub fn usage(message: impl Into<String>) -> Self {
        Self::new(2, message)
    }
    pub fn auth(message: impl Into<String>) -> Self {
        Self::new(3, message)
    }
    pub fn io(e: impl std::fmt::Display) -> Self {
        Self::new(4, e.to_string())
    }
    pub fn detail(mut self, detail: Value) -> Self {
        self.detail = Some(detail);
        self
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for Error {}
impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::io(e)
    }
}
impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Self::usage(format!("Invalid JSON: {e}"))
    }
}
