use serde_json as sj;
use std::convert::TryFrom;
use std::str::FromStr;
use thiserror;
use tinyvec::TinyVec;

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("non-empty JSON pointer must have a leading '/'")]
    NotRooted,
}

/// Pointer is a parsed JSON pointer.
#[derive(Debug, Eq, PartialEq, Clone)]
pub struct Pointer(TinyVec<[u8; 16]>);

/// Token is a parsed token of a JSON pointer.
#[derive(Debug, Eq, PartialEq)]
pub enum Token<'t> {
    /// Integer index of a JSON array.
    /// If applied to a JSON object, the index is may also serve as a property name.
    Index(usize),
    /// JSON object property name. Never an integer.
    Property(&'t str),
    /// Next JSON index which is one beyond the current array extent.
    /// If applied to a JSON object, the property literal "-" is used.
    NextIndex,
}

/// Iter is the iterator type over Tokens that's returned by Pointer::iter().
pub struct Iter<'t>(&'t [u8]);

impl Pointer {
    // Builds an empty Pointer which references the document root.
    pub fn new() -> Pointer {
        Pointer(TinyVec::new())
    }

    // Push a new Token onto the Pointer.
    pub fn push<'t>(&mut self, token: Token<'t>) -> &mut Pointer {
        match token {
            Token::Index(ind) => {
                self.0.push('I' as u8);
                self.enc_varint(ind as u64);
            }
            Token::Property(prop) => {
                // Encode as 'P' control code,
                // followed by varint *byte* (not char) length,
                // followed by property UTF-8 bytes.
                self.0.push('P' as u8);
                let prop = prop.as_bytes();
                self.enc_varint(prop.len() as u64);
                self.0.extend(prop.iter().copied());
            }
            Token::NextIndex => {
                self.0.push('-' as u8);
            }
        }
        self
    }

    /// Iterate over pointer tokens.
    pub fn iter<'t>(&'t self) -> Iter<'t> {
        Iter(&self.0)
    }

    fn enc_varint(&mut self, n: u64) {
        let mut buf = [0 as u8; 10];
        let n = super::varint::write_varu64(&mut buf, n);
        self.0.extend(buf.iter().copied().take(n));
    }
}

impl TryFrom<&str> for Pointer {
    type Error = Error;

    fn try_from(s: &str) -> Result<Self, Error> {
        if s.is_empty() {
            return Ok(Pointer(TinyVec::new()));
        } else if !s.starts_with('/') {
            return Err(Error::NotRooted);
        }

        let mut tape = Pointer(TinyVec::new());

        s.split('/')
            .skip(1)
            .map(|t| t.replace("~1", "/").replace("~0", "~"))
            .for_each(|t| {
                if t == "-" {
                    tape.push(Token::NextIndex);
                } else if t.starts_with('+') {
                    tape.push(Token::Property(&t));
                } else if t.starts_with('0') && t.len() > 1 {
                    tape.push(Token::Property(&t));
                } else if let Ok(ind) = usize::from_str(&t) {
                    tape.push(Token::Index(ind));
                } else {
                    tape.push(Token::Property(&t));
                }
            });

        Ok(tape)
    }
}

impl<'t> Iterator for Iter<'t> {
    type Item = Token<'t>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.0.is_empty() {
            return None;
        }
        // Match on next control code.
        Some(match self.0[0] as char {
            '-' => {
                self.0 = &self.0[1..]; // Pop control code.
                Token::NextIndex
            }
            'P' => {
                let (prop_len, prop_len_len) = super::varint::read_varu64(&self.0[1..]);
                let prop = &self.0[1 + prop_len_len..1 + prop_len_len + prop_len as usize];
                let prop = unsafe { std::str::from_utf8_unchecked(prop) };
                self.0 = &self.0[1 + prop_len_len + prop_len as usize..]; // Pop.
                Token::Property(prop)
            }
            'I' => {
                let (ind, ind_len) = super::varint::read_varu64(&self.0[1..]);
                self.0 = &self.0[1 + ind_len..]; // Pop.
                Token::Index(ind as usize)
            }
            c @ _ => panic!("unexpected tape control {:?}", c),
        })
    }
}

impl Pointer {
    /// Query an existing value at the pointer location within the document.
    /// Returns None if the pointed location (or a parent thereof) does not exist.
    pub fn query<'v>(&self, doc: &'v sj::Value) -> Option<&'v sj::Value> {
        use sj::Value::{Array, Object};
        use Token::*;

        let mut v = doc;

        for token in self.iter() {
            let next = match v {
                Object(map) => match token {
                    Index(ind) => map.get(&ind.to_string()),
                    Property(prop) => map.get(prop),
                    NextIndex => map.get("-"),
                },
                Array(arr) => match token {
                    Index(ind) => arr.get(ind),
                    Property(_) | NextIndex => None,
                },
                _ => None,
            };

            if let Some(vv) = next {
                v = vv;
            } else {
                return None;
            }
        }
        Some(v)
    }

    /// Query a mutable existing value at the pointer location within the document,
    /// recursively creating the location if it doesn't exist. Existing parent locations
    /// which are Null are instantiated as an Object or Array, depending on the type of
    /// Token at that location (Property or Index/NextIndex). An existing Array is
    /// extended with Nulls as required to instantiate a specified Index.
    /// Returns a mutable Value at the pointed location, or None only if the document
    /// structure is incompatible with the pointer (eg, because a parent location is
    /// a scalar type, or attempts to index an array by-property).
    pub fn create<'v>(&self, doc: &'v mut sj::Value) -> Option<&'v mut sj::Value> {
        use sj::Value as sjv;
        use Token::*;

        let mut v = doc;

        for token in self.iter() {
            // If the current value is null but more tokens remain in the pointer,
            // instantiate it as an object or array (depending on token type) in
            // which we'll create the next child location.
            if let sjv::Null = v {
                match token {
                    Property(_) => {
                        *v = sjv::Object(sj::map::Map::new());
                    }
                    Index(_) | NextIndex => {
                        *v = sjv::Array(Vec::new());
                    }
                };
            }

            v = match v {
                sjv::Object(map) => match token {
                    // Create or modify existing entry.
                    Index(ind) => map.entry(ind.to_string()).or_insert(sj::Value::Null),
                    Property(prop) => map.entry(prop).or_insert(sj::Value::Null),
                    NextIndex => map.entry("-").or_insert(sj::Value::Null),
                },
                sjv::Array(arr) => match token {
                    Index(ind) => {
                        // Create any required indices [0..ind) as Null.
                        if ind >= arr.len() {
                            arr.extend(
                                std::iter::repeat(sj::Value::Null).take(1 + ind - arr.len()),
                            );
                        }
                        // Create or modify |ind| entry.
                        arr.get_mut(ind).unwrap()
                    }
                    NextIndex => {
                        // Append and return a Null.
                        arr.push(sj::Value::Null);
                        arr.last_mut().unwrap()
                    }
                    // Cannot match (attempt to query property of an array).
                    Property(_) => return None,
                },
                sjv::Number(_) | sjv::Bool(_) | sjv::String(_) => {
                    return None; // Cannot match (attempt to take child of scalar).
                }
                sjv::Null => panic!("unexpected null"),
            };
        }
        Some(v)
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_ptr_parsing() -> Result<(), Error> {
        use Token::*;

        // Basic example.
        let ptr = Pointer::try_from("/p1/2/p3/-")?;
        assert!(vec![Property("p1"), Index(2), Property("p3"), NextIndex]
            .into_iter()
            .eq(ptr.iter()));

        // Empty pointer.
        let ptr = Pointer::try_from("")?;
        assert_eq!(ptr.iter().next(), None);

        // Un-rooted pointers are an error.
        match Pointer::try_from("p1/2") {
            Err(Error::NotRooted) => (),
            _ => panic!("expected error"),
        }

        // Handles escapes.
        let ptr = Pointer::try_from("/p~01/~12")?;
        assert!(vec![Property("p~1"), Property("/2")]
            .into_iter()
            .eq(ptr.iter()));

        // Handles disallowed integer representations.
        let ptr = Pointer::try_from("/01/+2/-3/4/-")?;
        assert!(vec![
            Property("01"),
            Property("+2"),
            Property("-3"),
            Index(4),
            NextIndex,
        ]
        .into_iter()
        .eq(ptr.iter()));

        Ok(())
    }

    #[test]
    fn test_ptr_size() -> Result<(), Error> {
        assert_eq!(std::mem::size_of::<Pointer>(), 32);

        let small = Pointer::try_from("/_estuary/uuid")?;
        assert_eq!(small.0.len(), 16);

        if let TinyVec::Heap(_) = small.0 {
            panic!("didn't expect fixture to spill to heap");
        }

        let large = Pointer::try_from("/large key/and child")?;
        assert_eq!(large.0.len(), 22);

        if let TinyVec::Inline(_) = large.0 {
            panic!("expected large fixture to spill to heap");
        }

        Ok(())
    }

    #[test]
    fn test_ptr_query() -> Result<(), Error> {
        // Extended document fixture from RFC-6901.
        let doc = sj::json!({
            "foo": ["bar", "baz"],
            "": 0,
            "a/b": 1,
            "c%d": 2,
            "e^f": 3,
            "g|h": 4,
            "i\\j": 5,
            "k\"l": 6,
            " ": 7,
            "m~n": 8,
            "9": 10,
            "-": 11,
        });

        // Query document locations which exist (cases from RFC-6901).
        for case in [
            ("", sj::json!(doc)),
            ("/foo", sj::json!(["bar", "baz"])),
            ("/foo/0", sj::json!("bar")),
            ("/foo/1", sj::json!("baz")),
            ("/", sj::json!(0)),
            ("/a~1b", sj::json!(1)),
            ("/c%d", sj::json!(2)),
            ("/e^f", sj::json!(3)),
            ("/g|h", sj::json!(4)),
            ("/i\\j", sj::json!(5)),
            ("/k\"l", sj::json!(6)),
            ("/ ", sj::json!(7)),
            ("/m~0n", sj::json!(8)),
            ("/9", sj::json!(10)),
            ("/-", sj::json!(11)),
        ]
        .iter()
        {
            let ptr = Pointer::try_from(case.0)?;
            assert_eq!(ptr.query(&doc).unwrap(), &case.1);
        }

        // Locations which don't exist.
        for case in [
            "/bar",      // Missing property.
            "/foo/2",    // Missing index.
            "/foo/prop", // Cannot take property of array.
            "/e^f/3",    // Not an object or array.
        ]
        .iter()
        {
            let ptr = Pointer::try_from(*case)?;
            assert!(ptr.query(&doc).is_none());
        }

        Ok(())
    }

    #[test]
    fn test_ptr_create() -> Result<(), Error> {
        use estuary_json as ej;
        use sj::Value as sjv;

        // Modify a Null root by applying a succession of upserts.
        let mut root = sjv::Null;

        for case in [
            // Creates Object root, Array at /foo, and Object at /foo/1.
            ("/foo/2/a", sjv::String("hello".to_owned())),
            // Add property to existing object.
            ("/foo/2/b", ej::Number::Unsigned(3).into()),
            ("/foo/0", sjv::Bool(false)), // Update existing Null.
            ("/bar", sjv::Null),          // Add property to doc root.
            ("/foo/0", sjv::Bool(true)),  // Update from 'false'.
            ("/foo/-", sjv::String("world".to_owned())), // NextIndex extends Array.
            // Index token is interpreted as property because object exists.
            ("/foo/2/4", ej::Number::Unsigned(5).into()),
            // NextIndex token is also interpreted as property.
            ("/foo/2/-", sjv::Bool(false)),
        ]
        .iter_mut()
        {
            let ptr = Pointer::try_from(case.0)?;
            std::mem::swap(ptr.create(&mut root).unwrap(), &mut case.1);
        }

        assert_eq!(
            root,
            sj::json!({
                "foo": [true, sjv::Null, {"-": false, "a": "hello", "b": 3, "4": 5}, "world"],
                "bar": sjv::Null,
            })
        );

        // Cases which return None.
        for case in [
            "/foo/2/a/3", // Attempt to index string scalar.
            "/foo/bar",   // Attempt to take property of array.
        ]
        .iter()
        {
            let ptr = Pointer::try_from(*case)?;
            assert!(ptr.create(&mut root).is_none());
        }

        Ok(())
    }
}
