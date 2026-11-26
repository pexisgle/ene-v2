//! Maps shared SQL projections onto stored domain records.

pub(crate) trait FromRow: Sized {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self>;
}

/// Declares a projection and its field mapping in the same column order.
macro_rules! row_fields {
    ($ty:ty, $columns:ident {
        $first:ident $(=> $first_read:ident($first_error:literal))?,
        $($field:ident $(as $column:ident)? $(=> $read:ident($error:literal))?),+ $(,)?
    }) => {
        const $columns: &str = concat!(
            stringify!($first) $(, ", ", $crate::row::row_fields!(@column $field $($column)?))+
        );

        impl $crate::row::FromRow for $ty {
            fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
                Ok(Self {
                    $first: $crate::row::row_fields!(
                        @read row, stringify!($first) $(, $first_read, $first_error)?
                    )?,
                    $($field: $crate::row::row_fields!(
                        @read row, $crate::row::row_fields!(@column $field $($column)?)
                        $(, $read, $error)?
                    )?),+
                })
            }
        }
    };
    (@column $field:ident $column:ident) => { stringify!($column) };
    (@column $field:ident) => { stringify!($field) };
    (@read $row:ident, $column:expr) => { $row.get($column) };
    (@read $row:ident, $column:expr, $read:ident, $error:literal) => {
        $crate::$read($row, $column, $error)
    };
}
pub(crate) use row_fields;
