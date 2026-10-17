//! Small safe helpers over CoreFoundation collections used by the video path.

use objc2_core_foundation::{CFBoolean, CFDictionary, CFRetained, CFString, CFType};

/// Builds an immutable `CFDictionary` from `(key, value)` pairs. Taking the array by value
/// lets callers pass any CF object as a value (deref coercion to `&CFType`).
pub(crate) fn dict<const N: usize>(
    pairs: [(&CFString, &CFType); N],
) -> CFRetained<CFDictionary<CFString, CFType>> {
    let keys = pairs.map(|(k, _)| k);
    let values = pairs.map(|(_, v)| v);
    CFDictionary::from_slices(&keys, &values)
}

/// Looks up `key` in an untyped dictionary whose keys and values are CF objects.
pub(crate) fn dict_get(dict: &CFDictionary, key: &CFString) -> Option<CFRetained<CFType>> {
    // SAFETY: `CFDictionary<K, V>` only differs from the untyped dictionary by zero-sized
    // markers. Every dictionary passed here (format-description extensions, sample
    // attachments, the session dictionary) stores CF objects as values, and the lookup itself
    // hashes and compares through the dictionary's own callbacks, not through `K`.
    let typed = unsafe { &*(dict as *const CFDictionary).cast::<CFDictionary<CFString, CFType>>() };
    typed.get(key)
}

/// `dict[key]` as a boolean; `None` if absent or not a `CFBoolean`.
pub(crate) fn dict_bool(dict: &CFDictionary, key: &CFString) -> Option<bool> {
    let value = dict_get(dict, key)?;
    value.downcast_ref::<CFBoolean>().map(CFBoolean::as_bool)
}
