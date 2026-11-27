//! A public search may send a query to one explicitly selected connection.

/// Selection represented by the enabled entries in saved settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchSelection<T> {
    /// Search is turned off.
    Off,
    /// This is the one enabled destination.
    Selected(T),
    /// A saved configuration enables several destinations. Choose one before sending a query.
    Ambiguous,
}

impl<T> SearchSelection<T> {
    /// Evaluates at most two enabled connections because any excess makes selection ambiguous.
    pub fn from_enabled(mut enabled: impl Iterator<Item = T>) -> Self {
        let Some(first) = enabled.next() else {
            return Self::Off;
        };
        if enabled.next().is_some() {
            Self::Ambiguous
        } else {
            Self::Selected(first)
        }
    }
}
