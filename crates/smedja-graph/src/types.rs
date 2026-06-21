/// The kind of a named symbol extracted from a source file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SymbolKind {
    /// A `fn` item.
    Function,
    /// A `struct` item.
    Struct,
    /// An `enum` item.
    Enum,
    /// A `trait` item.
    Trait,
    /// An `impl` block.
    Impl,
    /// A `const` item.
    Const,
    /// A `type` alias item.
    TypeAlias,
}

impl SymbolKind {
    /// Returns the canonical string used in the `symbols` table `kind` column.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Function => "function",
            Self::Struct => "struct",
            Self::Enum => "enum",
            Self::Trait => "trait",
            Self::Impl => "impl",
            Self::Const => "const",
            Self::TypeAlias => "type_alias",
        }
    }

    /// Parses a `kind` string as stored in `SQLite` back to a [`SymbolKind`].
    ///
    /// Returns `None` when the string is not a recognised kind.
    #[must_use]
    pub fn try_from_str(s: &str) -> Option<Self> {
        match s {
            "function" => Some(Self::Function),
            "struct" => Some(Self::Struct),
            "enum" => Some(Self::Enum),
            "trait" => Some(Self::Trait),
            "impl" => Some(Self::Impl),
            "const" => Some(Self::Const),
            "type_alias" => Some(Self::TypeAlias),
            _ => None,
        }
    }
}

/// A named symbol extracted from a Rust source file.
#[derive(Debug, Clone)]
pub struct Symbol {
    /// UUID v4 identifier.
    pub id: String,
    /// The workspace this symbol belongs to.
    pub workspace_id: String,
    /// Path to the source file, relative to the workspace root.
    pub file_path: String,
    /// The symbol name as it appears in source.
    pub name: String,
    /// The kind of symbol.
    pub kind: SymbolKind,
    /// 0-based line number where the definition starts.
    pub start_line: u32,
    /// 0-based line number where the definition ends.
    pub end_line: u32,
    /// Up to the first 10 lines of the definition, taken verbatim from source.
    pub snippet: String,
}
