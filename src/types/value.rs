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
    Date {
        /// Year, including century. Signed to match `SQL_DATE_STRUCT`.
        year: i16,
        /// Month, 1-12.
        month: u16,
        /// Day of month, 1-31.
        day: u16,
    },
    /// A time of day (`TIME`).
    Time {
        /// Hour, 0-23.
        hour: u16,
        /// Minute, 0-59.
        minute: u16,
        /// Second, 0-61 (the spec allows leap seconds).
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
        /// Year, including century. Signed to match `SQL_TIMESTAMP_STRUCT`.
        year: i16,
        /// Month, 1-12.
        month: u16,
        /// Day of month, 1-31.
        day: u16,
        /// Hour, 0-23.
        hour: u16,
        /// Minute, 0-59.
        minute: u16,
        /// Second, 0-61 (the spec allows leap seconds).
        second: u16,
        /// Fractional seconds, in nanoseconds.
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
        /// Year, including century.
        year: i16,
        /// Month, 1-12.
        month: u16,
        /// Day of month, 1-31.
        day: u16,
        /// Hour, 0-23.
        hour: u16,
        /// Minute, 0-59.
        minute: u16,
        /// Second, 0-61 (the spec allows leap seconds).
        second: u16,
        /// Fractional seconds, in nanoseconds.
        fraction: u32,
        /// Offset from UTC in minutes, positive east. Minutes rather than
        /// hours because several zones are offset by 30 or 45.
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
    IntervalYearMonth {
        /// Whole years.
        years: i32,
        /// Whole months, not normalised into `years`.
        months: i32,
    },
    /// INTERVAL DAY TO SECOND, as a single signed total.
    ///
    /// A split days/milliseconds representation admits states where the two
    /// carry different signs, and cannot represent a negative interval of less
    /// than one day at all.
    IntervalDayTime {
        /// The whole interval as signed milliseconds.
        total_milliseconds: i64,
    },
}

// ---------------------------------------------------------------------------
// TypeInfoRow
// ---------------------------------------------------------------------------

/// One row of the `SQLGetTypeInfo` result set: a data type the driver exposes.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct TypeInfoRow {
    /// `TYPE_NAME` — column 1 of the `SQLGetTypeInfo` result set.
    ///
    /// The data-source-specific type name, as it appears in DDL.
    pub type_name: &'static str,
    /// `DATA_TYPE` — column 2 of the `SQLGetTypeInfo` result set.
    ///
    /// The ODBC SQL type this maps to.
    pub data_type: SqlDataType,
    /// `COLUMN_SIZE` — column 3 of the `SQLGetTypeInfo` result set.
    ///
    /// Maximum size, in the units the spec defines for the type.
    pub column_size: i32,
    /// `LITERAL_PREFIX` — column 4 of the `SQLGetTypeInfo` result set.
    ///
    /// Characters that open a literal of this type, or `None` if it has no literal form.
    pub literal_prefix: Option<&'static str>,
    /// `LITERAL_SUFFIX` — column 5 of the `SQLGetTypeInfo` result set.
    ///
    /// The closing counterpart of `literal_prefix`.
    pub literal_suffix: Option<&'static str>,
    /// `CREATE_PARAMS` — column 6 of the `SQLGetTypeInfo` result set.
    ///
    /// Comma-separated names of the parameters a `CREATE TABLE` spelling takes, e.g. `"precision,scale"`.
    pub create_params: Option<&'static str>,
    /// `NULLABLE` — column 7 of the `SQLGetTypeInfo` result set.
    ///
    /// Whether the data source accepts `NULL` for this type, as a `SQL_NULLABLE_*` value.
    pub nullable: i16,
    /// `CASE_SENSITIVE` — column 8 of the `SQLGetTypeInfo` result set.
    ///
    /// Whether the type is a character type with case-sensitive collation.
    pub case_sensitive: bool,
    /// `SEARCHABLE` — column 9 of the `SQLGetTypeInfo` result set.
    ///
    /// How the type may be used in a `WHERE` clause, as a `SQL_PRED_*` value.
    pub searchable: i16,
    /// `UNSIGNED_ATTRIBUTE` — column 10 of the `SQLGetTypeInfo` result set.
    ///
    /// Whether the type is unsigned; `None` for non-numeric types.
    pub unsigned: Option<bool>,
    /// `FIXED_PREC_SCALE` — column 11 of the `SQLGetTypeInfo` result set.
    ///
    /// Whether the type has fixed precision and scale, i.e. is a money type.
    pub fixed_prec_scale: bool,
    /// `AUTO_UNIQUE_VALUE` — column 12 of the `SQLGetTypeInfo` result set.
    ///
    /// Whether the type auto-increments; `None` when not applicable.
    pub auto_unique_value: Option<bool>,
    /// `LOCAL_TYPE_NAME` — column 13 of the `SQLGetTypeInfo` result set.
    ///
    /// The localised name for display, which is not usable in SQL.
    pub local_type_name: Option<&'static str>,
    /// `MINIMUM_SCALE` — column 14 of the `SQLGetTypeInfo` result set.
    ///
    /// Least scale the type accepts; `None` when scale does not apply.
    pub minimum_scale: Option<i16>,
    /// `MAXIMUM_SCALE` — column 15 of the `SQLGetTypeInfo` result set.
    ///
    /// Greatest scale the type accepts; `None` when scale does not apply.
    pub maximum_scale: Option<i16>,
    /// `SQL_DATA_TYPE` — column 16 of the `SQLGetTypeInfo` result set.
    ///
    /// The verbose type, which differs from `data_type` for datetimes and intervals.
    pub sql_data_type: i16,
    /// `SQL_DATETIME_SUB` — column 17 of the `SQLGetTypeInfo` result set.
    ///
    /// The datetime or interval subcode, when `sql_data_type` is one of those.
    pub sql_datetime_sub: Option<i16>,
    /// `NUM_PREC_RADIX` — column 18 of the `SQLGetTypeInfo` result set.
    ///
    /// Whether `column_size` counts bits or decimal digits: 2 or 10.
    pub num_prec_radix: Option<i32>,
    /// `INTERVAL_PRECISION` — column 19 of the `SQLGetTypeInfo` result set.
    ///
    /// Leading-field precision, for interval types only.
    pub interval_precision: Option<i16>,
}

impl TypeInfoRow {
    /// A row for `type_name` of `data_type`, with every other column at its
    /// least-committal value: no declared size or scale, nullable, fully
    /// searchable, no literal form and no localised name.
    ///
    /// `sql_data_type` defaults to `data_type`, which is correct for every type
    /// except the datetime and interval families — those set it with
    /// [`TypeInfoRow::with_verbose_type`].
    ///
    /// `const` on this and every builder below, because
    /// [`Backend::get_type_info`](crate::backend::Backend::get_type_info)
    /// returns `&'static [TypeInfoRow]`: a driver builds its type list in a
    /// `static`, where a non-const builder cannot be called.
    pub const fn new(type_name: &'static str, data_type: SqlDataType) -> Self {
        Self {
            type_name,
            data_type,
            column_size: 0,
            literal_prefix: None,
            literal_suffix: None,
            create_params: None,
            nullable: Nullable::SqlNullable as i16,
            case_sensitive: false,
            searchable: crate::types::SQL_SEARCHABLE,
            unsigned: None,
            fixed_prec_scale: false,
            auto_unique_value: None,
            local_type_name: None,
            minimum_scale: None,
            maximum_scale: None,
            sql_data_type: data_type.0,
            sql_datetime_sub: None,
            num_prec_radix: None,
            interval_precision: None,
        }
    }

    /// Sets `COLUMN_SIZE`.
    #[must_use]
    pub const fn with_column_size(mut self, column_size: i32) -> Self {
        self.column_size = column_size;
        self
    }

    /// Sets `LITERAL_PREFIX` and `LITERAL_SUFFIX`.
    #[must_use]
    pub const fn with_literal_affixes(
        mut self,
        prefix: Option<&'static str>,
        suffix: Option<&'static str>,
    ) -> Self {
        self.literal_prefix = prefix;
        self.literal_suffix = suffix;
        self
    }

    /// Sets `CREATE_PARAMS`, e.g. `Some("length")` or `Some("precision,scale")`.
    #[must_use]
    pub const fn with_create_params(mut self, create_params: Option<&'static str>) -> Self {
        self.create_params = create_params;
        self
    }

    /// Sets `NULLABLE`, one of the `SQL_NULLABLE_*` values.
    #[must_use]
    pub const fn with_nullable(mut self, nullable: i16) -> Self {
        self.nullable = nullable;
        self
    }

    /// Sets `CASE_SENSITIVE`.
    #[must_use]
    pub const fn with_case_sensitive(mut self, case_sensitive: bool) -> Self {
        self.case_sensitive = case_sensitive;
        self
    }

    /// Sets `SEARCHABLE`, one of the `SQL_PRED_*` values.
    #[must_use]
    pub const fn with_searchable(mut self, searchable: i16) -> Self {
        self.searchable = searchable;
        self
    }

    /// Sets `UNSIGNED_ATTRIBUTE`; `None` for a non-numeric type.
    #[must_use]
    pub const fn with_unsigned(mut self, unsigned: Option<bool>) -> Self {
        self.unsigned = unsigned;
        self
    }

    /// Sets `FIXED_PREC_SCALE`, i.e. whether this is a money type.
    #[must_use]
    pub const fn with_fixed_prec_scale(mut self, fixed_prec_scale: bool) -> Self {
        self.fixed_prec_scale = fixed_prec_scale;
        self
    }

    /// Sets `AUTO_UNIQUE_VALUE`; `None` when auto-increment does not apply.
    #[must_use]
    pub const fn with_auto_unique_value(mut self, auto_unique_value: Option<bool>) -> Self {
        self.auto_unique_value = auto_unique_value;
        self
    }

    /// Sets `LOCAL_TYPE_NAME`, the display name, which is not usable in SQL.
    #[must_use]
    pub const fn with_local_type_name(mut self, local_type_name: Option<&'static str>) -> Self {
        self.local_type_name = local_type_name;
        self
    }

    /// Sets `MINIMUM_SCALE` and `MAXIMUM_SCALE`.
    #[must_use]
    pub const fn with_scale_range(
        mut self,
        minimum_scale: Option<i16>,
        maximum_scale: Option<i16>,
    ) -> Self {
        self.minimum_scale = minimum_scale;
        self.maximum_scale = maximum_scale;
        self
    }

    /// Sets `SQL_DATA_TYPE` and `SQL_DATETIME_SUB`, which differ from
    /// `data_type` only for the datetime and interval families.
    #[must_use]
    pub const fn with_verbose_type(
        mut self,
        sql_data_type: i16,
        sql_datetime_sub: Option<i16>,
    ) -> Self {
        self.sql_data_type = sql_data_type;
        self.sql_datetime_sub = sql_datetime_sub;
        self
    }

    /// Sets `NUM_PREC_RADIX`: 2 or 10, or `None` for a non-numeric type.
    #[must_use]
    pub const fn with_num_prec_radix(mut self, num_prec_radix: Option<i32>) -> Self {
        self.num_prec_radix = num_prec_radix;
        self
    }

    /// Sets `INTERVAL_PRECISION`, for interval types only.
    #[must_use]
    pub const fn with_interval_precision(mut self, interval_precision: Option<i16>) -> Self {
        self.interval_precision = interval_precision;
        self
    }

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
    /// The ODBC SQL type of the column — `SQL_DESC_CONCISE_TYPE`.
    ///
    /// `SQL_DESC_TYPE` reports the *verbose* type derived from this, which
    /// differs for the datetime and interval families.
    pub sql_type: SqlDataType,
    /// The column's declared precision: digits for a numeric type, characters
    /// for a character one, as `SQL_DESC_PRECISION` defines per type.
    pub precision: u32,
    /// The column's declared scale — digits right of the decimal point.
    /// Signed, because the spec permits a negative scale.
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
