use std::fmt;

use bytes::Bytes;

/// The body of a `Request`.
///
/// On WaOS a body is bytes held in memory, so it can always be sent again
/// when a redirect asks for it.
pub struct Body {
    bytes: Bytes,
}

impl Body {
    /// Returns a reference to the internal data of the `Body`.
    #[inline]
    pub fn as_bytes(&self) -> Option<&[u8]> {
        Some(&self.bytes)
    }

    pub(crate) fn try_clone(&self) -> Option<Body> {
        Some(Body {
            bytes: self.bytes.clone(),
        })
    }

    pub(super) fn into_bytes(self) -> Bytes {
        self.bytes
    }
}

impl From<Bytes> for Body {
    #[inline]
    fn from(bytes: Bytes) -> Body {
        Body { bytes }
    }
}

impl From<Vec<u8>> for Body {
    #[inline]
    fn from(vec: Vec<u8>) -> Body {
        Body { bytes: vec.into() }
    }
}

impl From<&'static [u8]> for Body {
    #[inline]
    fn from(s: &'static [u8]) -> Body {
        Body {
            bytes: Bytes::from_static(s),
        }
    }
}

impl From<String> for Body {
    #[inline]
    fn from(s: String) -> Body {
        Body { bytes: s.into() }
    }
}

impl From<&'static str> for Body {
    #[inline]
    fn from(s: &'static str) -> Body {
        s.as_bytes().into()
    }
}

impl fmt::Debug for Body {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Body").finish()
    }
}

impl Default for Body {
    fn default() -> Body {
        Body {
            bytes: Bytes::new(),
        }
    }
}
