#![allow(unsafe_code)]

//! [`InlineArray`] is an inlinable array of bytes that is intended for situations where many bytes
//! are being shared in database-like scenarios, where optimizing for space usage is extremely
//! important.
//!
//! [`InlineArray`] uses 8 bytes on the stack. It will inline arrays of up to 7 bytes. If the bytes
//! are longer than that, it will store them in an optimized reference-count-backed structure,
//! where the atomic reference count is 16 bytes. If the maximum counter is reached, the bytes
//! are copied into a new `InlineArray` with a fresh reference count of 1. This is made with
//! the assumption that most reference counts will be far lower than 2^16.
//!
//! Both the inline and shared instances of `InlineArray` guarantee that the stored array is
//! always aligned to 8-byte boundaries, regardless of if it is inline on the stack or
//! shared on the heap. This is advantageous for using in combination with certain
//! zero-copy serialization techniques.
//!
//! The 16-bit reference counter is stored packed with a 48-bit length field at the beginning
//! of the shared array. Byte arrays that require more than 48 bits to store their length
//! (256 terabytes) are not supported.
//!
//! `InlineArray::make_mut` can be used for getting a mutable reference to the bytes in this
//! structure. If the shared reference counter is higher than  1, this acts like a `Cow` and
//! will make self into a private copy that is safe for modification.

use std::{
    alloc::{alloc, dealloc, Layout},
    convert::TryFrom,
    fmt,
    hash::{Hash, Hasher},
    iter::FromIterator,
    mem::size_of,
    ops::Deref,
    sync::atomic::{AtomicU16, AtomicU8, Ordering},
};

const SZ: usize = size_of::<usize>();
const INLINE_CUTOFF: usize = SZ - 1;
const SMALL_REMOTE_CUTOFF: usize = u8::MAX as usize;
const BIG_REMOTE_LEN_BYTES: usize = 6;

const INLINE_TRAILER_TAG: u8 = 0b00;
const SMALL_REMOTE_TRAILER_TAG: u8 = 0b01;
const BIG_REMOTE_TRAILER_TAG: u8 = 0b10;
const TRAILER_TAG_MASK: u8 = 0b0000_0011;
const TRAILER_PTR_MASK: u8 = 0b1111_1100;

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Inline,
    SmallRemote,
    BigRemote,
}

const fn _static_tests() {
    // static assert that BigRemoteHeader is 8 bytes in size
    let _: [u8; 8] = [0; std::mem::size_of::<BigRemoteHeader>()];

    // static assert that BigRemoteHeader is 8 byte-aligned
    let _: [u8; 8] = [0; std::mem::align_of::<BigRemoteHeader>()];

    // static assert that SmallRemoteTrailer is 2 bytes in size
    let _: [u8; 2] = [0; std::mem::size_of::<SmallRemoteTrailer>()];

    // static assert that SmallRemoteTrailer is 1 byte-aligned
    let _: [u8; 1] = [0; std::mem::align_of::<SmallRemoteTrailer>()];

    // static assert that InlineArray is 8 bytes
    let _: [u8; 8] = [0; std::mem::size_of::<InlineArray>()];

    // static assert that InlineArray is 8 byte-aligned
    let _: [u8; 8] = [0; std::mem::align_of::<InlineArray>()];
}

/// A buffer that may either be inline or remote and protected
/// by an Arc. The inner buffer is guaranteed to be aligned to
/// 8 byte boundaries.
#[repr(align(8))]
pub struct InlineArray([u8; SZ]);

impl Clone for InlineArray {
    fn clone(&self) -> InlineArray {
        // We use 16 bytes for the reference count at
        // the cost of this CAS and copying the inline
        // array when we reach our max reference count size.
        //
        // When measured against the standard Arc reference
        // count increment, this had a negligible performance
        // hit that only became measurable at high contention,
        // which is probably not likely for DB workloads where
        // it is expected that most concurrent operations will
        // distributed somewhat across larger structures.

        if self.kind() == Kind::SmallRemote {
            let rc = &self.deref_small_trailer().rc;

            loop {
                let current = rc.load(Ordering::Relaxed);
                if current == u8::MAX {
                    return InlineArray::from(self.deref());
                }

                let cas_res = rc.compare_exchange_weak(
                    current,
                    current + 1,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                );
                if cas_res.is_ok() {
                    break;
                }
            }
        } else if self.kind() == Kind::BigRemote {
            let rc = &self.deref_big_header().rc;

            loop {
                let current = rc.load(Ordering::Relaxed);
                if current == u16::MAX {
                    return InlineArray::from(self.deref());
                }

                let cas_res = rc.compare_exchange_weak(
                    current,
                    current + 1,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                );
                if cas_res.is_ok() {
                    break;
                }
            }
        }
        InlineArray(self.0)
    }
}

impl Drop for InlineArray {
    fn drop(&mut self) {
        let kind = self.kind();

        if kind == Kind::SmallRemote {
            let small_trailer = self.deref_small_trailer();
            let rc = small_trailer.rc.fetch_sub(1, Ordering::Release) - 1;

            if rc == 0 {
                std::sync::atomic::fence(Ordering::Acquire);

                let layout = Layout::from_size_align(
                    small_trailer.len() + size_of::<SmallRemoteTrailer>(),
                    8,
                )
                .unwrap();

                unsafe {
                    let ptr = self.remote_ptr().sub(small_trailer.len());
                    dealloc(ptr as *mut u8, layout);
                }
            }
        } else if kind == Kind::BigRemote {
            let big_header = self.deref_big_header();
            let rc = big_header.rc.fetch_sub(1, Ordering::Release) - 1;

            if rc == 0 {
                std::sync::atomic::fence(Ordering::Acquire);

                let layout =
                    Layout::from_size_align(big_header.len() + size_of::<BigRemoteHeader>(), 8)
                        .unwrap();

                unsafe {
                    dealloc(self.remote_ptr() as *mut u8, layout);
                }
            }
        }
    }
}

struct SmallRemoteTrailer {
    rc: AtomicU8,
    len: u8,
}

impl SmallRemoteTrailer {
    const fn len(&self) -> usize {
        self.len as usize
    }
}

#[repr(align(8))]
struct BigRemoteHeader {
    rc: AtomicU16,
    len: [u8; BIG_REMOTE_LEN_BYTES],
}

impl BigRemoteHeader {
    const fn len(&self) -> usize {
        let buf: [u8; 8] = [
            self.len[0],
            self.len[1],
            self.len[2],
            self.len[3],
            self.len[4],
            self.len[5],
            0,
            0,
        ];
        usize::from_le_bytes(buf)
    }
}

impl Deref for InlineArray {
    type Target = [u8];

    #[inline]
    fn deref(&self) -> &[u8] {
        match self.kind() {
            Kind::Inline => &self.0[..self.inline_len()],
            Kind::SmallRemote => unsafe {
                let len = self.deref_small_trailer().len();
                let data_ptr = self.remote_ptr().sub(len);
                std::slice::from_raw_parts(data_ptr, len)
            },
            Kind::BigRemote => unsafe {
                let data_ptr = self.remote_ptr().add(size_of::<BigRemoteHeader>());
                let len = self.deref_big_header().len();
                std::slice::from_raw_parts(data_ptr, len)
            },
        }
    }
}

impl AsRef<[u8]> for InlineArray {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        self
    }
}

impl Default for InlineArray {
    fn default() -> Self {
        Self::from(&[])
    }
}

impl Hash for InlineArray {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.deref().hash(state);
    }
}

impl InlineArray {
    fn new(slice: &[u8]) -> Self {
        let mut data = [0_u8; SZ];
        if slice.len() <= INLINE_CUTOFF {
            data[SZ - 1] = u8::try_from(slice.len()).unwrap() << 2;
            data[..slice.len()].copy_from_slice(slice);
            data[SZ - 1] |= INLINE_TRAILER_TAG;
        } else if slice.len() <= SMALL_REMOTE_CUTOFF {
            let layout =
                Layout::from_size_align(slice.len() + size_of::<SmallRemoteTrailer>(), 8).unwrap();

            let trailer = SmallRemoteTrailer {
                rc: 1.into(),
                len: u8::try_from(slice.len()).unwrap(),
            };

            unsafe {
                let data_ptr = alloc(layout);
                assert!(!data_ptr.is_null());
                let trailer_ptr = data_ptr.add(slice.len());

                std::ptr::write(trailer_ptr as *mut SmallRemoteTrailer, trailer);
                std::ptr::copy_nonoverlapping(slice.as_ptr(), data_ptr, slice.len());
                std::ptr::write_unaligned(data.as_mut_ptr() as _, trailer_ptr);
            }

            // assert that the bottom 3 bits are empty, as we expect
            // the buffer to always have an alignment of 8 (2 ^ 3).
            #[cfg(not(miri))]
            assert_eq!(data[SZ - 1] & 0b111, 0);

            data[SZ - 1] |= SMALL_REMOTE_TRAILER_TAG;
        } else {
            let layout =
                Layout::from_size_align(slice.len() + size_of::<BigRemoteHeader>(), 8).unwrap();

            let slice_len_buf = slice.len().to_le_bytes();
            let len: [u8; BIG_REMOTE_LEN_BYTES] = [
                slice_len_buf[0],
                slice_len_buf[1],
                slice_len_buf[2],
                slice_len_buf[3],
                slice_len_buf[4],
                slice_len_buf[5],
            ];
            assert_eq!(slice_len_buf[6], 0);
            assert_eq!(slice_len_buf[7], 0);

            let header = BigRemoteHeader { rc: 1.into(), len };

            unsafe {
                let header_ptr = alloc(layout);
                assert!(!header_ptr.is_null());
                let data_ptr = header_ptr.add(size_of::<BigRemoteHeader>());

                std::ptr::write(header_ptr as *mut BigRemoteHeader, header);
                std::ptr::copy_nonoverlapping(slice.as_ptr(), data_ptr, slice.len());
                std::ptr::write_unaligned(data.as_mut_ptr() as _, header_ptr);
            }

            // assert that the bottom 3 bits are empty, as we expect
            // the buffer to always have an alignment of 8 (2 ^ 3).
            #[cfg(not(miri))]
            assert_eq!(data[SZ - 1] & 0b111, 0);

            data[SZ - 1] |= BIG_REMOTE_TRAILER_TAG;
        }
        Self(data)
    }

    fn remote_ptr(&self) -> *const u8 {
        assert_ne!(self.kind(), Kind::Inline);
        let mut copied = self.0;
        copied[SZ - 1] &= TRAILER_PTR_MASK;

        unsafe { std::ptr::read((&copied).as_ptr() as *const *const u8) }
    }

    fn deref_small_trailer(&self) -> &SmallRemoteTrailer {
        assert_eq!(self.kind(), Kind::SmallRemote);
        unsafe { &*(self.remote_ptr() as *mut SmallRemoteTrailer) }
    }

    fn deref_big_header(&self) -> &BigRemoteHeader {
        assert_eq!(self.kind(), Kind::BigRemote);
        unsafe { &*(self.remote_ptr() as *mut BigRemoteHeader) }
    }

    #[cfg(miri)]
    fn inline_len(&self) -> usize {
        (self.trailer() >> 2) as usize
    }

    #[cfg(miri)]
    fn kind(&self) -> Kind {
        self.trailer() & TRAILER_TAG_MASK == INLINE_TRAILER_TAG
    }

    #[cfg(miri)]
    fn inline_trailer(&self) -> u8 {
        self.deref()[SZ - 1]
    }

    #[cfg(not(miri))]
    const fn inline_len(&self) -> usize {
        (self.inline_trailer() >> 2) as usize
    }

    #[cfg(not(miri))]
    const fn kind(&self) -> Kind {
        match self.inline_trailer() & TRAILER_TAG_MASK {
            INLINE_TRAILER_TAG => Kind::Inline,
            SMALL_REMOTE_TRAILER_TAG => Kind::SmallRemote,
            BIG_REMOTE_TRAILER_TAG => Kind::BigRemote,
            _other => unsafe { std::hint::unreachable_unchecked() },
        }
    }

    #[cfg(not(miri))]
    const fn inline_trailer(&self) -> u8 {
        self.0[SZ - 1]
    }

    /// This function returns a mutable reference to the inner
    /// byte array. If there are more than 1 atomic references
    /// to the inner array, the array is copied into a new
    /// `InlineVec` and a reference to that is returned. This
    /// functions similarly in spirit to [`std::sync::Arc::make_mut`].
    pub fn make_mut(&mut self) -> &mut [u8] {
        match self.kind() {
            Kind::Inline => {
                let inline_len = self.inline_len();
                &mut self.0[..inline_len]
            }
            Kind::SmallRemote => {
                if self.deref_small_trailer().rc.load(Ordering::Acquire) != 1 {
                    *self = InlineArray::from(self.deref())
                }
                unsafe {
                    let len = self.deref_small_trailer().len();
                    let data_ptr = self.remote_ptr().sub(len);
                    std::slice::from_raw_parts_mut(data_ptr as *mut u8, len)
                }
            }
            Kind::BigRemote => {
                if self.deref_big_header().rc.load(Ordering::Acquire) != 1 {
                    *self = InlineArray::from(self.deref())
                }
                unsafe {
                    let data_ptr = self.remote_ptr().add(size_of::<BigRemoteHeader>());
                    let len = self.deref_big_header().len();
                    std::slice::from_raw_parts_mut(data_ptr as *mut u8, len)
                }
            }
        }
    }
}

impl FromIterator<u8> for InlineArray {
    fn from_iter<T>(iter: T) -> Self
    where
        T: IntoIterator<Item = u8>,
    {
        let bs: Vec<u8> = iter.into_iter().collect();
        bs.into()
    }
}

impl From<&[u8]> for InlineArray {
    fn from(slice: &[u8]) -> Self {
        InlineArray::new(slice)
    }
}

impl From<&str> for InlineArray {
    fn from(s: &str) -> Self {
        Self::from(s.as_bytes())
    }
}

impl From<String> for InlineArray {
    fn from(s: String) -> Self {
        Self::from(s.as_bytes())
    }
}

impl From<&String> for InlineArray {
    fn from(s: &String) -> Self {
        Self::from(s.as_bytes())
    }
}

impl From<&InlineArray> for InlineArray {
    fn from(v: &Self) -> Self {
        v.clone()
    }
}

impl From<Vec<u8>> for InlineArray {
    fn from(v: Vec<u8>) -> Self {
        InlineArray::new(&v)
    }
}

impl From<Box<[u8]>> for InlineArray {
    fn from(v: Box<[u8]>) -> Self {
        InlineArray::new(&v)
    }
}

impl std::borrow::Borrow<[u8]> for InlineArray {
    fn borrow(&self) -> &[u8] {
        self.as_ref()
    }
}

impl std::borrow::Borrow<[u8]> for &InlineArray {
    fn borrow(&self) -> &[u8] {
        self.as_ref()
    }
}

impl<const N: usize> From<&[u8; N]> for InlineArray {
    fn from(v: &[u8; N]) -> Self {
        Self::from(&v[..])
    }
}

impl Ord for InlineArray {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.as_ref().cmp(other.as_ref())
    }
}

impl PartialOrd for InlineArray {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<T: AsRef<[u8]>> PartialEq<T> for InlineArray {
    fn eq(&self, other: &T) -> bool {
        self.as_ref() == other.as_ref()
    }
}

impl PartialEq<[u8]> for InlineArray {
    fn eq(&self, other: &[u8]) -> bool {
        self.as_ref() == other
    }
}

impl Eq for InlineArray {}

impl fmt::Debug for InlineArray {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.as_ref().fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use super::InlineArray;

    #[test]
    fn inline_array_smoke() {
        let ia = InlineArray::from(vec![1, 2, 3]);
        assert_eq!(ia, vec![1, 2, 3]);
    }

    #[test]
    fn small_remote_array_smoke() {
        let ia = InlineArray::from(&[4; 200][..]);
        assert_eq!(ia, vec![4; 200]);
    }

    #[test]
    fn big_remote_array_smoke() {
        let ia = InlineArray::from(&[4; 256][..]);
        assert_eq!(ia, vec![4; 256]);
    }

    #[test]
    fn boxed_slice_conversion() {
        let boite1: Box<[u8]> = Box::new([1, 2, 3]);
        let iv1: InlineArray = boite1.into();
        assert_eq!(iv1, vec![1, 2, 3]);
        let boite2: Box<[u8]> = Box::new([4; 128]);
        let iv2: InlineArray = boite2.into();
        assert_eq!(iv2, vec![4; 128]);
    }

    #[test]
    fn inline_array_as_mut_identity() {
        let initial = &[1];
        let mut iv = InlineArray::from(initial);
        assert_eq!(initial, &*iv);
        assert_eq!(initial, iv.make_mut());
    }

    fn prop_identity(inline_array: &InlineArray) -> bool {
        let mut iv2 = inline_array.clone();

        if iv2 != inline_array {
            println!("expected clone to equal original");
            return false;
        }

        if *inline_array != *iv2 {
            println!("expected AsMut to equal original");
            return false;
        }

        if &*inline_array != iv2.make_mut() {
            println!("expected AsMut to equal original");
            return false;
        }

        let buf: &[u8] = inline_array.as_ref();
        assert_eq!(buf.as_ptr() as usize % 8, 0);

        true
    }

    impl quickcheck::Arbitrary for InlineArray {
        fn arbitrary(g: &mut quickcheck::Gen) -> Self {
            InlineArray::from(Vec::arbitrary(g))
        }
    }

    quickcheck::quickcheck! {
        #[cfg_attr(miri, ignore)]
        fn inline_array(item: InlineArray) -> bool {
            dbg!(item.len());
            prop_identity(&item)
        }
    }

    #[test]
    fn inline_array_bug_00() {
        assert!(prop_identity(&InlineArray::new(&[
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ])));
    }
}
