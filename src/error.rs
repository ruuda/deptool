use std::fmt;

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Git(git2::Error),
    NonUtf8FileName,
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<git2::Error> for Error {
    fn from(e: git2::Error) -> Self {
        Error::Git(e)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "{e}"),
            Error::Git(e) => write!(f, "{e}"),
            Error::NonUtf8FileName => write!(f, "non-utf8 file name"),
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;
