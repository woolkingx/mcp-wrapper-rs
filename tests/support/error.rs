use std::fmt;

/// A single validation failure with JSON path context.
#[derive(Debug, Clone)]
pub struct ValidationError {
    /// JSON path where the error occurred (e.g., ".user.age").
    pub path: String,
    /// What kind of validation failed.
    pub kind: ErrorKind,
    /// Human-readable message.
    pub message: String,
}

impl ValidationError {
    pub fn new(path: impl Into<String>, kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            kind,
            message: message.into(),
        }
    }

    /// Create an error at the root path.
    pub fn root(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self::new("", kind, message)
    }

    /// Create a child error by prepending a field name to the path.
    pub fn at_field(mut self, field: &str) -> Self {
        if self.path.is_empty() {
            self.path = format!(".{field}");
        } else {
            self.path = format!(".{field}{}", self.path);
        }
        self
    }

    /// Create a child error by prepending an array index to the path.
    pub fn at_index(mut self, idx: usize) -> Self {
        if self.path.is_empty() {
            self.path = format!("[{idx}]");
        } else {
            self.path = format!("[{idx}]{}", self.path);
        }
        self
    }
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.path.is_empty() {
            write!(f, "{}", self.message)
        } else {
            write!(f, "{}: {}", self.path, self.message)
        }
    }
}

impl std::error::Error for ValidationError {}

/// Categories of validation failures, mapped to Draft-07 keywords.
#[derive(Debug, Clone, PartialEq)]
pub enum ErrorKind {
    // Type
    TypeError,
    // Const / Enum
    ConstMismatch,
    EnumMismatch,
    // Numeric
    Minimum,
    Maximum,
    ExclusiveMinimum,
    ExclusiveMaximum,
    MultipleOf,
    // String
    MinLength,
    MaxLength,
    Pattern,
    // Array
    MinItems,
    MaxItems,
    UniqueItems,
    Contains,
    // Object
    Required,
    AdditionalProperties,
    MinProperties,
    MaxProperties,
    // Dependencies
    DependentRequired,
    DependencySchema,
    // Composition
    OneOfNoneMatch,
    OneOfMultipleMatch,
    AnyOfNoneMatch,
    // Conditional
    IfThenFailed,
    // Set on non-object
    NotAnObject,
}
