// Copyright (c) 2004-present, Facebook, Inc.
// All Rights Reserved.
//
// This software may be used and distributed according to the terms of the
// GNU General Public License version 2 or any later version.

use quickcheck::{Arbitrary, Gen};
use rand::distributions::{IndependentSample, LogNormal};

use errors::*;

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, HeapSizeOf)]
pub struct Delta {
    // Fragments should be in sorted order by start offset and should not overlap.
    frags: Vec<Fragment>,
}

impl Delta {
    /// Construct a new Delta object. Verify that `frags` is sane, sorted and
    /// non-overlapping.
    pub fn new(frags: Vec<Fragment>) -> Result<Self> {
        Self::verify(&frags)?;
        Ok(Delta { frags: frags })
    }

    /// Construct a new Delta object given a fulltext (no delta).
    pub fn new_fulltext<T: Into<Vec<u8>>>(text: T) -> Self {
        Self {
            frags: vec![
                Fragment {
                    start: 0,
                    end: 0,
                    content: text.into(),
                },
            ],
        }
    }

    pub fn fragments(&self) -> &[Fragment] {
        self.frags.as_slice()
    }

    /// If this delta might be a fulltext, return the fulltext. Note that we can only tell with
    /// certainty that something is *not* a fulltext. A delta with one fragment that inserts text
    /// in the beginning appears identical to a fulltext at this layer.
    pub fn maybe_fulltext(&self) -> Option<&[u8]> {
        if self.frags.len() == 1 && self.frags[0].start == 0 && self.frags[0].end == 0 {
            Some(self.frags[0].content.as_slice())
        } else {
            None
        }
    }

    fn verify(frags: &[Fragment]) -> Result<()> {
        let mut prev_frag: Option<&Fragment> = None;
        for (i, frag) in frags.iter().enumerate() {
            frag.verify().with_context(|_| {
                ErrorKind::InvalidFragmentList(format!("invalid fragment {}", i))
            })?;
            if let Some(prev) = prev_frag {
                if frag.start < prev.end {
                    let msg = format!(
                        "fragment {}: previous end {} overlaps with start {}",
                        i, prev.end, frag.start
                    );
                    bail!(ErrorKind::InvalidFragmentList(msg));
                }
            }
            prev_frag = Some(frag);
        }
        Ok(())
    }
}

impl Default for Delta {
    fn default() -> Delta {
        Delta { frags: Vec::new() }
    }
}

impl Arbitrary for Delta {
    fn arbitrary<G: Gen>(g: &mut G) -> Self {
        let size = g.size();
        let nfrags = g.gen_range(0, size);

        // Maintain invariants (start <= end, no overlap).
        let mut start = 0;
        let mut end = 0;

        let frags = (0..nfrags)
            .map(|_| {
                start = end + g.gen_range(0, size);
                end = start + g.gen_range(0, size);
                let val = Fragment {
                    start: start,
                    end: end,
                    content: arbitrary_frag_content(g),
                };
                val
            })
            .collect();
        Delta { frags: frags }
    }

    fn shrink(&self) -> Box<Iterator<Item = Self>> {
        // Not all instances generated by Vec::shrink will be
        // valid. Theoretically we could shrink in ways such that the invariants
        // are maintained, but just filtering is easier.
        Box::new(
            self.frags
                .shrink()
                .filter(|frags| Delta::verify(&frags).is_ok())
                .map(|frags| Delta { frags: frags }),
        )
    }
}

/// Represents a single contiguous modified region of text.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, HeapSizeOf)]
pub struct Fragment {
    pub start: usize,
    pub end: usize,
    pub content: Vec<u8>,
}

impl Fragment {
    /// Return the end offset of this Fragment's content, after application.
    pub fn post_end(&self) -> usize {
        self.start + self.content.len()
    }

    /// Return the change in text length this Fragment will cause when applied.
    pub fn length_change(&self) -> isize {
        self.content.len() as isize - (self.end - self.start) as isize
    }

    /// Return true if the given offset falls within this Fragment's content (post-application).
    pub fn contains_offset(&self, offset: usize) -> bool {
        self.start <= offset && offset < self.post_end()
    }

    fn verify(&self) -> Result<()> {
        if self.start > self.end {
            bail!("invalid fragment: start {} > end {}", self.start, self.end);
        }
        Ok(())
    }
}

impl Arbitrary for Fragment {
    fn arbitrary<G: Gen>(g: &mut G) -> Self {
        let size = g.size();

        // Maintain invariant start <= end.
        let start = g.gen_range(0, size);
        let end = start + g.gen_range(0, size);

        Fragment {
            start: start,
            end: end,
            content: arbitrary_frag_content(g),
        }
    }

    fn shrink(&self) -> Box<Iterator<Item = Self>> {
        Box::new(
            (self.start, self.end, self.content.clone())
                .shrink()
                .filter(|&(start, end, ref _content)| {
                    // shrink could produce bad values
                    start <= end
                })
                .map(|(start, end, content)| Fragment {
                    start: start,
                    end: end,
                    content: content,
                }),
        )
    }
}

fn arbitrary_frag_content<G: Gen>(g: &mut G) -> Vec<u8> {
    let size = g.size();
    // Using a uniform distribution over size here can lead to extremely bloated
    // data structures. We also want to test zero-length data with more than a
    // (1/size) probability. So use a lognormal distribution.
    //
    // The choice of mean and stdev are pretty arbitrary, but they work well for
    // common sizes (~100).
    // TODO: make this more rigorous, e.g. by using params such that p95 = size.
    let lognormal = LogNormal::new(-3.0, 2.0);
    let content_len = ((size as f64) * lognormal.ind_sample(g)) as usize;

    let mut v = Vec::with_capacity(content_len);
    g.fill_bytes(&mut v);
    v
}

/// Apply a Delta to an input text, returning the result.
pub fn apply(text: &[u8], delta: &Delta) -> Vec<u8> {
    let mut chunks = Vec::with_capacity(delta.frags.len() * 2);
    let mut off = 0;

    for frag in &delta.frags {
        assert!(off <= frag.start);
        if off < frag.start {
            chunks.push(&text[off..frag.start]);
        }
        if frag.content.len() > 0 {
            chunks.push(frag.content.as_ref())
        }
        off = frag.end;
    }
    if off < text.len() {
        chunks.push(&text[off..text.len()]);
    }

    let size = chunks.iter().map(|c| c.len()).sum::<usize>();
    let mut output = Vec::with_capacity(size);
    for c in chunks {
        output.extend_from_slice(c);
    }
    output
}

/// Apply a chain of Deltas to an input text, returning the result.
pub fn apply_chain<I: IntoIterator<Item = Delta>>(text: &[u8], deltas: I) -> Vec<u8> {
    let mut res = Vec::from(text);
    for delta in deltas {
        res = apply(&res, &delta);
    }
    res
}

/// XXX: Compatibility functions for the old bdiff module for testing purposes. The delta
/// module will replace that one once all instances of Vec<bdiff::Delta> are replaced
/// with delta::Delta, and this compatibility module will be removed at that time.
pub mod compat {
    use super::*;
    use bdiff;

    pub fn convert<T>(deltas: T) -> Delta
    where
        T: IntoIterator<Item = bdiff::Delta>,
    {
        Delta {
            frags: deltas
                .into_iter()
                .map(|delta| Fragment {
                    start: delta.start,
                    end: delta.end,
                    content: delta.content.clone(),
                })
                .collect(),
        }
    }

    pub fn apply_deltas<T>(text: &[u8], deltas: T) -> Vec<u8>
    where
        T: IntoIterator<Item = Vec<bdiff::Delta>>,
    {
        apply_chain(text, deltas.into_iter().map(convert))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test that fragments are verified properly.
    #[test]
    fn test_delta_new() {
        #[cfg_attr(rustfmt, rustfmt_skip)]
        let test_cases = vec![
            (vec![Fragment { start: 0, end: 0, content: vec![] }], true),
            (vec![Fragment { start: 0, end: 5, content: vec![] }], true),
            (vec![Fragment { start: 0, end: 5, content: vec![] },
                  Fragment { start: 5, end: 8, content: vec![] }], true),
            (vec![Fragment { start: 0, end: 5, content: vec![] },
                  Fragment { start: 6, end: 9, content: vec![] }], true),
            (vec![Fragment { start: 0, end: 5, content: vec![] },
                  Fragment { start: 6, end: 5, content: vec![] }], false),
            (vec![Fragment { start: 0, end: 5, content: vec![] },
                  Fragment { start: 4, end: 8, content: vec![] }], false),
        ];

        for (frags, success) in test_cases.into_iter() {
            let delta = Delta::new(frags);
            if success {
                assert!(delta.is_ok());
            } else {
                assert!(delta.is_err());
            }
        }
    }

    #[test]
    fn test_maybe_fulltext() {
        #[cfg_attr(rustfmt, rustfmt_skip)]
        let test_cases = vec![
            (vec![Fragment { start: 0, end: 0, content: vec![] }], true),
            (vec![Fragment { start: 0, end: 0, content: vec![b'a'] }], true),
            (vec![Fragment { start: 0, end: 1, content: vec![b'b'] }], false),
            (vec![Fragment { start: 1, end: 2, content: vec![b'c'] }], false),
            (vec![Fragment { start: 0, end: 0, content: vec![b'd'] },
                  Fragment { start: 1, end: 2, content: vec![b'e'] }], false),
        ];

        for (frags, maybe_fulltext) in test_cases.into_iter() {
            let delta = Delta::new(frags).unwrap();
            if maybe_fulltext {
                assert!(delta.maybe_fulltext().is_some());
            } else {
                assert!(delta.maybe_fulltext().is_none());
            }
        }
    }

    quickcheck! {
        fn delta_gen(delta: Delta) -> bool {
            Delta::verify(&delta.frags).is_ok()
        }

        fn delta_shrink(delta: Delta) -> bool {
            // This test is a bit redundant, but let's just verify.
            delta.shrink().take(100).all(|d| {
                Delta::verify(&d.frags).is_ok()
            })
        }

        fn fragment_gen(fragment: Fragment) -> bool {
            fragment.verify().is_ok()
        }

        fn fragment_shrink(fragment: Fragment) -> bool {
            fragment.shrink().take(100).all(|f| f.verify().is_ok())
        }
    }

    #[test]
    fn test_apply_1() {
        let text = b"aaaa\nbbbb\ncccc\n";
        let delta = Delta {
            frags: vec![
                Fragment {
                    start: 5,
                    end: 10,
                    content: (&b"xxxx\n"[..]).into(),
                },
            ],
        };

        let res = apply(text, &delta);
        assert_eq!(&res[..], b"aaaa\nxxxx\ncccc\n");
    }

    #[test]
    fn test_apply_2() {
        let text = b"bbbb\ncccc\n";
        let delta = Delta {
            frags: vec![
                Fragment {
                    start: 0,
                    end: 5,
                    content: (&b"aaaabbbb\n"[..]).into(),
                },
                Fragment {
                    start: 10,
                    end: 10,
                    content: (&b"dddd\n"[..]).into(),
                },
            ],
        };

        let res = apply(text, &delta);
        assert_eq!(&res[..], b"aaaabbbb\ncccc\ndddd\n");
    }

    #[test]
    fn test_apply_3a() {
        let text = b"aaaa\nbbbb\ncccc\n";
        let delta = Delta {
            frags: vec![
                Fragment {
                    start: 0,
                    end: 15,
                    content: (&b"zzzz\nyyyy\nxxxx\n"[..]).into(),
                },
            ],
        };

        let res = apply(text, &delta);
        assert_eq!(&res[..], b"zzzz\nyyyy\nxxxx\n");
    }

    #[test]
    fn test_apply_3b() {
        let text = b"aaaa\nbbbb\ncccc\n";
        let delta = Delta {
            frags: vec![
                Fragment {
                    start: 0,
                    end: 5,
                    content: (&b"zzzz\n"[..]).into(),
                },
                Fragment {
                    start: 5,
                    end: 10,
                    content: (&b"yyyy\n"[..]).into(),
                },
                Fragment {
                    start: 10,
                    end: 15,
                    content: (&b"xxxx\n"[..]).into(),
                },
            ],
        };

        let res = apply(text, &delta);
        assert_eq!(&res[..], b"zzzz\nyyyy\nxxxx\n");
    }

    #[test]
    fn test_apply_4() {
        let text = b"aaaa\nbbbb";
        let delta = Delta {
            frags: vec![
                Fragment {
                    start: 5,
                    end: 9,
                    content: (&b"bbbbcccc"[..]).into(),
                },
            ],
        };

        let res = apply(text, &delta);
        assert_eq!(&res[..], b"aaaa\nbbbbcccc");
    }

    #[test]
    fn test_apply_5() {
        let text = b"aaaa\nbbbb\ncccc\n";
        let delta = Delta {
            frags: vec![
                Fragment {
                    start: 5,
                    end: 10,
                    content: (&b""[..]).into(),
                },
            ],
        };

        let res = apply(text, &delta);
        assert_eq!(&res[..], b"aaaa\ncccc\n");
    }
}
