//! The failures a model is meant to read.

use std::fmt;
use std::path::Path;

/// The code a failed call reports, one of the ten the contract names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Code {
    InvalidArgs,
    OutsideWorkspace,
    NotFound,
    NotAFile,
    NotText,
    TooLarge,
    Ambiguous,
    ProgramNotAllowed,
    Io,
    Exec,
}

impl Code {
    /// The code as the tool's answer spells it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidArgs => "invalid_args",
            Self::OutsideWorkspace => "outside_workspace",
            Self::NotFound => "not_found",
            Self::NotAFile => "not_a_file",
            Self::NotText => "not_text",
            Self::TooLarge => "too_large",
            Self::Ambiguous => "ambiguous",
            Self::ProgramNotAllowed => "program_not_allowed",
            Self::Io => "io",
            Self::Exec => "exec",
        }
    }
}

/// Why a call did not produce a result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolError {
    code: Code,
    message: String,
}

impl ToolError {
    /// Says what went wrong under one of the ten codes.
    pub fn new(
        code: Code,
        message: impl Into<String>,
    ) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// The code the answer carries.
    #[must_use]
    pub const fn code(&self) -> Code {
        self.code
    }

    /// The sentence the model reads.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    #[must_use]
    pub fn io(
        path: &Path,
        error: &std::io::Error,
    ) -> Self {
        Self::new(Code::Io, format!("{}: {error}", path.display()))
    }
}

impl fmt::Display for ToolError {
    fn fmt(
        &self,
        formatter: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ToolError {}

macro_rules! codes {
    ($($name:ident: $code:ident),* $(,)?) => {
        impl ToolError {
            $(
                #[doc = concat!("Reports the failure the model reads as `", stringify!($code), "`.")]
                pub fn $name(message: impl Into<String>) -> Self {
                    Self::new(Code::$code, message)
                }
            )*
        }
    };
}

codes! {
    invalid_args: InvalidArgs,
    outside_workspace: OutsideWorkspace,
    not_found: NotFound,
    not_a_file: NotAFile,
    not_text: NotText,
    too_large: TooLarge,
    ambiguous: Ambiguous,
    program_not_allowed: ProgramNotAllowed,
    exec: Exec,
}
