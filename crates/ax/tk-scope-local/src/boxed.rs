use alloc::alloc::{AllocError, alloc, dealloc, handle_alloc_error};
use core::{alloc::Layout, ptr::NonNull};

use crate::item::Item;

#[repr(C)]
struct Header {
    item: &'static Item,
}

fn try_layout(body: Layout) -> Result<(Layout, usize), AllocError> {
    Layout::new::<Header>().extend(body).map_err(|_| AllocError)
}

fn layout_or_abort(body: Layout) -> (Layout, usize) {
    try_layout(body).unwrap_or_else(|_| handle_alloc_error(body))
}

impl Header {
    #[inline]
    fn body(&self) -> NonNull<()> {
        let (_, offset) = layout_or_abort(self.item.layout);
        unsafe {
            NonNull::new_unchecked(self as *const Self as *mut Self)
                .cast::<()>()
                .byte_add(offset)
        }
    }
}

pub(crate) struct ItemBox {
    ptr: NonNull<Header>,
}

unsafe impl Send for ItemBox {}
unsafe impl Sync for ItemBox {}

impl ItemBox {
    pub(crate) fn try_new(item: &'static Item) -> Result<Self, AllocError> {
        let (layout, offset) = try_layout(item.layout)?;
        let ptr = NonNull::new(unsafe { alloc(layout) })
            .ok_or(AllocError)?
            .cast();
        unsafe {
            ptr.write(Header { item });
            (item.init)(ptr.cast().byte_add(offset));
        }
        Ok(Self { ptr })
    }

    #[inline]
    fn header(&self) -> &Header {
        unsafe { self.ptr.as_ref() }
    }
}

impl<T> AsRef<T> for ItemBox {
    #[inline]
    fn as_ref(&self) -> &T {
        unsafe { self.header().body().cast().as_ref() }
    }
}

impl<T> AsMut<T> for ItemBox {
    #[inline]
    fn as_mut(&mut self) -> &mut T {
        unsafe { self.header().body().cast().as_mut() }
    }
}

impl Drop for ItemBox {
    fn drop(&mut self) {
        let item = self.header().item;
        let (layout, offset) = layout_or_abort(item.layout);
        unsafe {
            (item.drop)(self.ptr.cast().byte_add(offset));
            dealloc(self.ptr.cast().as_ptr(), layout);
        }
    }
}
