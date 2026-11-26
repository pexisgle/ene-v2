/// Discards a result on purpose, where nothing can be done about it.
///
/// The Win32 destroy calls return a `BOOL` that only reports whether the
/// object existed; the object is gone either way, so there is no failure to
/// report and no state left wrong.
#[inline]
pub(crate) fn ignore_teardown<T>(_result: T) {
    // Intentionally empty: the call's effect is the point, not its return.
}
