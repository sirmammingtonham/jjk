//! Small text helpers for user-facing messages.

/// Pick the singular or plural form for a count (`1 branch`, `2 branches`). English plurals (and
/// the verbs that agree with them) are irregular, so both forms are passed explicitly.
///
/// ```
/// # use jjk::text::plural;
/// assert_eq!(plural(1, "branch", "branches"), "branch");
/// assert_eq!(plural(2, "branch", "branches"), "branches");
/// assert_eq!(plural(0, "branch", "branches"), "branches");
/// ```
pub fn plural<'a>(n: usize, one: &'a str, many: &'a str) -> &'a str {
    if n == 1 {
        one
    } else {
        many
    }
}
