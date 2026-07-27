//! Value and descriptor types passed between the FFI layer and a backend:
//! [`ColumnValue`] (a single fetched cell), [`FetchResult`], [`Nullable`],
//! [`ColumnDescriptor`] (result-set column metadata) and [`TypeInfoRow`] (a
//! `SQLGetTypeInfo` row).

use odbc_sys::SqlDataType;

// ---------------------------------------------------------------------------
// Nullable
// ---------------------------------------------------------------------------

/// ODBC nullable attribute values, used in the `SQLColumns` result column 11 (NULLABLE),
/// the `SQLGetTypeInfo` result column 7 (NULLABLE), and the `SQLDescribeCol` output parameter.
///
/// Corresponds to `SQL_NO_NULLS` (0), `SQL_NULLABLE` (1), and `SQL_NULLABLE_UNKNOWN` (2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i16)]
pub enum Nullable {
    /// SQL_NO_NULLS (0): the column does not allow NULL values.
    SqlNoNulls = 0,
    /// SQL_NULLABLE (1): the column allows NULL values.
    SqlNullable = 1,
    /// SQL_NULLABLE_UNKNOWN (2): it is unknown whether the column allows NULL values.
    SqlNullableUnknown = 2,
}

impl Nullable {
    /// Returns the IS_NULLABLE string as defined by the ODBC spec for `SQLColumns` column 18:
    /// `"YES"`, `"NO"`, or `""` (empty) when nullability is unknown.
    pub fn as_is_nullable_str(self) -> &'static str {
        match self {
            Nullable::SqlNullable => "YES",
            Nullable::SqlNoNulls => "NO",
            Nullable::SqlNullableUnknown => "",
        }
    }
}

impl From<Nullable> for i16 {
    fn from(n: Nullable) -> i16 {
        n as i16
    }
}

/// SQLSpecialColumns `IdentifierType` argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentifierType {
    /// SQL_BEST_ROWID (1): the optimal column(s) that uniquely identify a row.
    BestRowId,
    /// SQL_ROWVER (2): columns the data source auto-updates on any row change.
    RowVer,
}

/// SQLSpecialColumns `Scope` argument and the SCOPE result column value.
///
/// Ordered CURROW < TRANSACTION < SESSION; the derived `Ord` lets a driver
/// compare the requested minimum scope against the scope it can guarantee.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(i16)]
pub enum Scope {
    /// SQL_SCOPE_CURROW (0).
    CurRow = 0,
    /// SQL_SCOPE_TRANSACTION (1).
    Transaction = 1,
    /// SQL_SCOPE_SESSION (2).
    Session = 2,
}

impl From<Scope> for i16 {
    fn from(s: Scope) -> i16 {
        s as i16
    }
}

// ---------------------------------------------------------------------------
// FetchResult
// ---------------------------------------------------------------------------

/// The outcome of a single row advance in a backend's cursor, returned by
/// [`crate::backend::StatementBackend`] and surfaced to the application as
/// `SQL_SUCCESS` or `SQL_NO_DATA` from `SQLFetch`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchResult {
    /// A row was available; its values can be read.
    Row,
    /// The cursor is exhausted; no more rows.
    NoData,
}

// ---------------------------------------------------------------------------
// ExecuteOutcome
// ---------------------------------------------------------------------------

/// The result of executing a prepared statement via
/// [`crate::backend::Backend::execute`].
///
/// `#[non_exhaustive]`: this is `execute`'s return contract. Reserving it as an
/// extensible struct means the contract can grow in a minor release (output
/// parameters today, and later e.g. generated keys or a richer affected-row
/// report) without the breaking signature change that turning a bare `()` into a
/// struct would otherwise force on every out-of-tree driver. Because it is
/// non-exhaustive, driver crates construct it through [`ExecuteOutcome::default`]
/// (the common no-output case) or [`ExecuteOutcome::with_output_params`], never
/// a struct literal.
#[non_exhaustive]
#[derive(Debug, Default, Clone)]
pub struct ExecuteOutcome {
    /// Values the backend produced for `SQL_PARAM_OUTPUT` /
    /// `SQL_PARAM_INPUT_OUTPUT` parameters, each keyed by its 1-based parameter
    /// number. Empty for backends without output-parameter support (the
    /// default).
    ///
    /// `stackable-odbc-core` writes each value back into the application's bound parameter
    /// buffer after `execute` returns; a backend only needs to populate this
    /// vector. This mirrors the input path, where core hands the backend the
    /// bound input values via `execute`'s `params` argument.
    pub output_params: Vec<OutputParam>,
}

impl ExecuteOutcome {
    /// An outcome carrying the given OUTPUT / INOUT parameter values.
    pub fn with_output_params(output_params: Vec<OutputParam>) -> Self {
        Self { output_params }
    }
}

/// A single OUTPUT / INOUT parameter value produced by
/// [`crate::backend::Backend::execute`], written back into the application's
/// bound buffer by `stackable-odbc-core`.
#[derive(Debug, Clone, PartialEq)]
pub struct OutputParam {
    /// The 1-based parameter number, matching `SQLBindParameter`'s
    /// `ParameterNumber` argument.
    pub parameter_number: u16,
    /// The value to marshal into the application's bound buffer.
    pub value: ColumnValue,
}

impl OutputParam {
    /// A new output-parameter value for the given 1-based parameter number.
    pub fn new(parameter_number: u16, value: ColumnValue) -> Self {
        Self {
            parameter_number,
            value,
        }
    }
}

// ---------------------------------------------------------------------------
// ColumnValue
// ---------------------------------------------------------------------------

/// A single fetched cell, produced by a backend and marshalled into the
/// caller's buffer by [`crate::column_value::write_column_value`]. The variant
/// records the source type; the target C type is chosen by the application's
/// `SQLGetData` / `SQLBindCol` call, and conversion (or a truncation/failure
/// SQLSTATE) happens at marshal time.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub enum ColumnValue {
    /// SQL NULL.
    Null,
    /// A character string (`CHAR`/`VARCHAR` and their Unicode variants).
    String(String),
    /// An 8-bit signed integer (`TINYINT`).
    I8(i8),
    /// A 16-bit signed integer (`SMALLINT`).
    I16(i16),
    /// A 32-bit signed integer (`INTEGER`).
    I32(i32),
    /// A 64-bit signed integer (`BIGINT`).
    I64(i64),
    /// A 32-bit floating-point value (`REAL`).
    F32(f32),
    /// A 64-bit floating-point value (`FLOAT`/`DOUBLE`).
    F64(f64),
    /// A boolean (`BIT`/`BOOLEAN`).
    Bool(bool),
    /// A calendar date (`DATE`).
    Date { year: i16, month: u16, day: u16 },
    /// A time of day (`TIME`).
    Time {
        hour: u16,
        minute: u16,
        second: u16,
        /// Fractional seconds in nanoseconds, matching `Timestamp::fraction`'s unit.
        ///
        /// `SQL_TIME_STRUCT` (`odbc_sys::Time`) has no fraction field, so this
        /// value can never reach a `SQL_C_TYPE_TIME` target -- writing it there
        /// truncates the fraction and reports SQLSTATE 01S07. It survives
        /// intact, however, when the value is rendered as a string for
        /// `SQL_C_CHAR` / `SQL_C_WCHAR` targets.
        fraction: u32,
    },
    /// A date and time (`TIMESTAMP`); `fraction` is in nanoseconds.
    Timestamp {
        year: i16,
        month: u16,
        day: u16,
        hour: u16,
        minute: u16,
        second: u16,
        fraction: u32,
    },
    /// Raw binary data (`BINARY`/`VARBINARY`/`BLOB`).
    Bytes(Vec<u8>),
    /// A 16-byte globally unique identifier (`GUID`).
    Guid([u8; 16]),
    /// DECIMAL(p,s) — stored as a string to preserve exact precision.
    Decimal(String),
    /// TIMESTAMP WITH TIME ZONE
    TimestampTz {
        year: i16,
        month: u16,
        day: u16,
        hour: u16,
        minute: u16,
        second: u16,
        fraction: u32,
        timezone_offset_minutes: i16,
    },
    /// JSON — raw JSON text
    Json(String),
    /// ARRAY — ordered sequence of values
    Array(Vec<ColumnValue>),
    /// MAP — key/value pairs
    Map(Vec<(ColumnValue, ColumnValue)>),
    /// ROW / struct — ordered fields
    Row(Vec<ColumnValue>),
    /// INTERVAL YEAR TO MONTH
    IntervalYearMonth { years: i32, months: i32 },
    /// INTERVAL DAY TO SECOND, as a single signed total.
    ///
    /// A split days/milliseconds representation admits states where the two
    /// carry different signs, and cannot represent a negative interval of less
    /// than one day at all.
    IntervalDayTime { total_milliseconds: i64 },
}

// ---------------------------------------------------------------------------
// TypeInfoRow
// ---------------------------------------------------------------------------

/// One row of the `SQLGetTypeInfo` result set: a data type the driver exposes.
#[derive(Debug, Clone)]
pub struct TypeInfoRow {
    pub type_name: &'static str,
    pub data_type: SqlDataType,
    pub column_size: i32,
    pub literal_prefix: Option<&'static str>,
    pub literal_suffix: Option<&'static str>,
    pub create_params: Option<&'static str>,
    pub nullable: i16,
    pub case_sensitive: bool,
    pub searchable: i16,
    pub unsigned: Option<bool>,
    pub fixed_prec_scale: bool,
    pub auto_unique_value: Option<bool>,
    pub local_type_name: Option<&'static str>,
    pub minimum_scale: Option<i16>,
    pub maximum_scale: Option<i16>,
    pub sql_data_type: i16,
    pub sql_datetime_sub: Option<i16>,
    pub num_prec_radix: Option<i32>,
    pub interval_precision: Option<i16>,
}

impl TypeInfoRow {
    /// Convert this row into a vector of [`ColumnValue`]s matching the ODBC
    /// `SQLGetTypeInfo` result set column order (19 columns).
    pub fn to_column_values(&self) -> Vec<ColumnValue> {
        vec![
            // 1. TYPE_NAME
            ColumnValue::String(self.type_name.to_string()),
            // 2. DATA_TYPE
            ColumnValue::I16(self.data_type.0),
            // 3. COLUMN_SIZE
            ColumnValue::I32(self.column_size),
            // 4. LITERAL_PREFIX
            match self.literal_prefix {
                Some(s) => ColumnValue::String(s.to_string()),
                None => ColumnValue::Null,
            },
            // 5. LITERAL_SUFFIX
            match self.literal_suffix {
                Some(s) => ColumnValue::String(s.to_string()),
                None => ColumnValue::Null,
            },
            // 6. CREATE_PARAMS
            match self.create_params {
                Some(s) => ColumnValue::String(s.to_string()),
                None => ColumnValue::Null,
            },
            // 7. NULLABLE
            ColumnValue::I16(self.nullable),
            // 8. CASE_SENSITIVE
            ColumnValue::I16(i16::from(self.case_sensitive)),
            // 9. SEARCHABLE
            ColumnValue::I16(self.searchable),
            // 10. UNSIGNED_ATTRIBUTE
            match self.unsigned {
                Some(b) => ColumnValue::I16(i16::from(b)),
                None => ColumnValue::Null,
            },
            // 11. FIXED_PREC_SCALE
            ColumnValue::I16(i16::from(self.fixed_prec_scale)),
            // 12. AUTO_UNIQUE_VALUE
            match self.auto_unique_value {
                Some(b) => ColumnValue::I16(i16::from(b)),
                None => ColumnValue::Null,
            },
            // 13. LOCAL_TYPE_NAME
            match self.local_type_name {
                Some(s) => ColumnValue::String(s.to_string()),
                None => ColumnValue::Null,
            },
            // 14. MINIMUM_SCALE
            match self.minimum_scale {
                Some(v) => ColumnValue::I16(v),
                None => ColumnValue::Null,
            },
            // 15. MAXIMUM_SCALE
            match self.maximum_scale {
                Some(v) => ColumnValue::I16(v),
                None => ColumnValue::Null,
            },
            // 16. SQL_DATA_TYPE
            ColumnValue::I16(self.sql_data_type),
            // 17. SQL_DATETIME_SUB
            match self.sql_datetime_sub {
                Some(v) => ColumnValue::I16(v),
                None => ColumnValue::Null,
            },
            // 18. NUM_PREC_RADIX
            match self.num_prec_radix {
                Some(v) => ColumnValue::I32(v),
                None => ColumnValue::Null,
            },
            // 19. INTERVAL_PRECISION
            match self.interval_precision {
                Some(v) => ColumnValue::I16(v),
                None => ColumnValue::Null,
            },
        ]
    }
}

// ---------------------------------------------------------------------------
// ColumnDescriptor
// ---------------------------------------------------------------------------

/// Describes one result-set column: what `SQLDescribeColW` and
/// `SQLColAttributeW` report to the application.
///
/// `#[non_exhaustive]`: `SQLColAttribute` defines many more descriptor fields
/// than this carries, and each one added would otherwise be a breaking change
/// for every driver that builds a descriptor with struct-literal syntax. Build
/// one with [`ColumnDescriptor::new`] and the `with_*` builders, which stay
/// source-compatible as fields are added. (`..Default::default()` is not an
/// escape hatch here — `#[non_exhaustive]` forbids that form downstream too.)
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ColumnDescriptor {
    /// Column alias if present, otherwise the column name.
    pub name: String,
    /// Data source-dependent type name, e.g. `"VARCHAR"` or `"DECIMAL"`: a
    /// bare name, matching the spec's own examples ("CHAR", "VARCHAR",
    /// "MONEY", ...), not a parameterised declaration like `"varchar(50)"`.
    /// Declared length/scale are carried separately via `precision`/`scale`
    /// below, not encoded into this name.
    ///
    /// This is what `SQL_DESC_TYPE_NAME` returns. Empty when the backend has no
    /// native name to offer, in which case a generic name is derived from
    /// `sql_type`.
    pub type_name: String,
    pub sql_type: SqlDataType,
    pub precision: u32,
    pub scale: i16,
    /// Whether the column accepts `NULL` — `SQL_DESC_NULLABLE`.
    ///
    /// A [`Nullable`], not a `bool`, because the spec defines *three* values and
    /// the third is not expressible as a boolean: `SQL_NULLABLE_UNKNOWN` is what
    /// a driver must report for a computed or outer-joined column whose
    /// nullability it cannot determine. Reporting `SQL_NO_NULLS` for those, as a
    /// `bool` forced, tells an application it may omit a NULL check it needs.
    pub nullable: Nullable,
    /// `SQL_DESC_SEARCHABLE` — one of [`SQL_PRED_NONE`], [`SQL_PRED_CHAR`],
    /// [`SQL_PRED_BASIC`] or [`SQL_SEARCHABLE`].
    ///
    /// [`SQL_PRED_NONE`]: crate::types::SQL_PRED_NONE
    /// [`SQL_PRED_CHAR`]: crate::types::SQL_PRED_CHAR
    /// [`SQL_PRED_BASIC`]: crate::types::SQL_PRED_BASIC
    /// [`SQL_SEARCHABLE`]: crate::types::SQL_SEARCHABLE
    pub searchable: i16,
    /// `SQL_DESC_LITERAL_PREFIX` — the character(s) that open a literal of this
    /// type, e.g. `'` for a character type or `0x` for binary. Empty when the
    /// type has no literal form.
    pub literal_prefix: String,
    /// `SQL_DESC_LITERAL_SUFFIX` — the closing counterpart of
    /// `literal_prefix`.
    pub literal_suffix: String,
    /// `SQL_DESC_TABLE_NAME` — the table this column came from, empty when the
    /// backend does not track it or the column is computed.
    pub table_name: String,
    /// `SQL_DESC_SCHEMA_NAME` — empty when the data source has no schemas or
    /// the backend does not track them.
    pub schema_name: String,
    /// `SQL_DESC_CATALOG_NAME` — empty when the data source has no catalogs or
    /// the backend does not track them.
    pub catalog_name: String,
}

impl Default for ColumnDescriptor {
    /// An unnamed column of unknown type, claiming nothing.
    ///
    /// Exists so core can extend the struct without rewriting every literal;
    /// a driver should build descriptors with [`ColumnDescriptor::new`], which
    /// makes the two fields that matter explicit.
    fn default() -> Self {
        Self::new(String::new(), SqlDataType::UNKNOWN_TYPE)
    }
}

impl ColumnDescriptor {
    /// A descriptor for `name` of `sql_type`, with every other field at its
    /// least-committal value: no declared precision or scale, nullability
    /// unknown, fully searchable, and no literal or origin information.
    ///
    /// `SQL_NULLABLE_UNKNOWN` is the default because it is the only one of the
    /// three that claims nothing. A backend that knows better says so with
    /// [`ColumnDescriptor::with_nullable`].
    pub fn new(name: impl Into<String>, sql_type: SqlDataType) -> Self {
        Self {
            name: name.into(),
            type_name: String::new(),
            sql_type,
            precision: 0,
            scale: 0,
            nullable: Nullable::SqlNullableUnknown,
            searchable: crate::types::SQL_SEARCHABLE,
            literal_prefix: String::new(),
            literal_suffix: String::new(),
            table_name: String::new(),
            schema_name: String::new(),
            catalog_name: String::new(),
        }
    }

    /// Sets the data-source-specific type name (`SQL_DESC_TYPE_NAME`).
    #[must_use]
    pub fn with_type_name(mut self, type_name: impl Into<String>) -> Self {
        self.type_name = type_name.into();
        self
    }

    /// Sets the declared precision and scale.
    #[must_use]
    pub fn with_precision_scale(mut self, precision: u32, scale: i16) -> Self {
        self.precision = precision;
        self.scale = scale;
        self
    }

    /// Sets the column's nullability (`SQL_DESC_NULLABLE`).
    #[must_use]
    pub fn with_nullable(mut self, nullable: Nullable) -> Self {
        self.nullable = nullable;
        self
    }

    /// Sets how the column may be used in a `WHERE` clause
    /// (`SQL_DESC_SEARCHABLE`).
    #[must_use]
    pub fn with_searchable(mut self, searchable: i16) -> Self {
        self.searchable = searchable;
        self
    }

    /// Sets the literal prefix and suffix for this column's type.
    #[must_use]
    pub fn with_literal_affixes(
        mut self,
        prefix: impl Into<String>,
        suffix: impl Into<String>,
    ) -> Self {
        self.literal_prefix = prefix.into();
        self.literal_suffix = suffix.into();
        self
    }

    /// Sets the catalog, schema and table the column originates from.
    #[must_use]
    pub fn with_origin(
        mut self,
        catalog: impl Into<String>,
        schema: impl Into<String>,
        table: impl Into<String>,
    ) -> Self {
        self.catalog_name = catalog.into();
        self.schema_name = schema.into();
        self.table_name = table.into();
        self
    }
}
