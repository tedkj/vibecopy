//! The rules that make a move safe. On the same drive a move is a rename (instant, nothing to
//! lose: the file is never in two places or none). Across drives it is copy, check, then delete
//! the original, and the delete happens only when every piece of evidence says the copy is
//! there and right. A source folder is only ever removed empty (`RemoveDirectory`): nothing in
//! the engine can delete a folder with something still in it.

/// What is known about one file's copy when its original is about to go.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Evidence {
    /// The copy finished (all bytes written, no error).
    pub copied: bool,
    /// It is in place under its final name.
    pub placed: bool,
    /// Its size and modified time equal the original's.
    pub quick: bool,
    /// Verify was on for the job...
    pub verify_on: bool,
    /// ...and both files hashed alike (None: not hashed).
    pub hashed: Option<bool>,
    /// The original has not changed since it was checked (looked at again right before the delete).
    pub source_unchanged: bool,
}

/// The one rule: delete only what has a checked copy in place. Every field must say yes.
pub fn may_delete_source(e: &Evidence) -> bool {
    e.copied && e.placed && e.quick && e.source_unchanged && (!e.verify_on || e.hashed == Some(true))
}

/// Why an original was kept, in words, when the rule said no.
pub fn kept_because(e: &Evidence) -> &'static str {
    if !e.copied {
        "The copy failed; the original was kept"
    } else if !e.placed {
        "The copy could not be put in place; the original was kept"
    } else if !e.quick {
        "The copy does not match the original; the original was kept"
    } else if e.verify_on && e.hashed != Some(true) {
        "The copy did not verify; the original was kept"
    } else if !e.source_unchanged {
        "The original changed after it was copied; it was kept"
    } else {
        ""
    }
}
