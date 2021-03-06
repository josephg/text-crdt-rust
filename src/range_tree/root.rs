use super::*;

use smallvec::SmallVec;
use crate::range_tree::index::FullIndex;
use std::mem::size_of;
use humansize::{FileSize, file_size_opts};

pub type DeleteResult<E> = SmallVec<[E; 2]>;
pub fn extend_delete<E: EntryTraits>(delete: &mut DeleteResult<E>, entry: E) {
    // println!("extend_delete {:?}", op);
    if let Some(last) = delete.last_mut() {
        if last.can_append(&entry) {
            // Extend!
            last.append(entry);
        } else { delete.push(entry); }
    } else { delete.push(entry); }
}

impl<E: EntryTraits, I: TreeIndex<E>> RangeTree<E, I> {
    pub fn new() -> Pin<Box<Self>> {
        let mut tree = Box::pin(Self {
            count: I::IndexOffset::default(),
            root: unsafe { Node::new_leaf() },
            last_cursor: Cell::new(None),
            _pin: marker::PhantomPinned,
        });

        // What a mess. I'm sure there's a nicer way to write this, somehow O_o.
        let parent_ref = unsafe { tree.as_ref().get_ref().to_parent_ptr() };
        tree.as_mut().root_ref_mut().set_parent(parent_ref);

        tree
    }

    fn root_ref_mut(self: Pin<&mut Self>) -> &mut Node<E, I> {
        unsafe {
            &mut self.get_unchecked_mut().root
        }
    }

    pub fn len(&self) -> I::IndexOffset {
        self.count
    }

    // pub fn get(&self, pos: usize) -> Option<E::Item> {
    //     let cursor = self.cursor_at_pos(pos, false);
    //     cursor.get_item()
    // }

    unsafe fn to_parent_ptr(&self) -> ParentPtr<E, I> {
        ParentPtr::Root(ref_to_nonnull(self))
    }

    pub fn cursor_at_query<F, G>(&self, raw_pos: usize, stick_end: bool, offset_to_num: F, entry_to_num: G) -> Cursor<E, I>
            where F: Fn(I::IndexOffset) -> usize, G: Fn(E) -> usize {
        // if let Some((pos, mut cursor)) = self.last_cursor.get() {
        //     if pos == raw_pos {
        //         if cursor.offset == 0 {
        //             cursor.prev_entry();
        //         }
        //         return cursor;
        //     }
        // }

        unsafe {
            let mut node = self.root.as_ptr();
            let mut offset_remaining = raw_pos;
            while let NodePtr::Internal(data) = node {
                let (new_offset_remaining, next) = data.as_ref()
                    .find_child_at_offset(offset_remaining, stick_end, &offset_to_num)
                    .expect("Internal consistency violation");
                offset_remaining = new_offset_remaining;
                node = next;
            };

            let leaf_ptr = node.unwrap_leaf();
            let (idx, offset_remaining) = leaf_ptr
                .as_ref().find_offset(offset_remaining, stick_end, entry_to_num)
                .expect("Element does not contain entry");

            Cursor {
                node: leaf_ptr,
                idx,
                offset: offset_remaining,
                // _marker: marker::PhantomData
            }
        }
    }

    pub fn cursor_at_end(&self) -> Cursor<E, I> {
        // There's ways to write this to be faster, but this method is called rarely enough that it
        // should be fine.
        // let cursor = self.cursor_at_query(offset_to_num(self.count), true, offset_to_num, entry_to_num);

        let cursor = unsafe {
            let mut node = self.root.as_ptr();
            while let NodePtr::Internal(ptr) = node {
                node = ptr.as_ref().last_child();
            };

            // Now scan to the end of the leaf
            let leaf_ptr = node.unwrap_leaf();
            let leaf = leaf_ptr.as_ref();
            let idx = leaf.len_entries() - 1;
            let offset = leaf.data[idx].len();

            Cursor {
                node: leaf_ptr,
                idx,
                offset
            }
        };

        if cfg!(debug_assertions) {
            // Make sure nothing went wrong while we're here.
            let mut cursor = cursor;
            let node = unsafe { cursor.node.as_ref() };
            assert_eq!(cursor.get_entry().len(), cursor.offset);
            assert_eq!(cursor.idx, node.len_entries() - 1);
            assert_eq!(cursor.next_entry(), false);
        }

        cursor
    }

    // pub fn clear_cursor_cache(self: &Pin<Box<Self>>) {
    //     self.as_ref().last_cursor.set(None);
    // }
    // pub fn cache_cursor(self: &Pin<Box<Self>>, pos: usize, cursor: Cursor<E>) {
    //     self.as_ref().last_cursor.set(Some((pos, cursor)));
    // }

    pub fn cursor_at_start(&self) -> Cursor<E, I> {
        // self.cursor_at_pos(0, false)

        unsafe {
            let mut node = self.root.as_ptr();
            while let NodePtr::Internal(data) = node {
                node = data.as_ref().data[0].1.as_ref().unwrap().as_ptr()
            };

            let leaf_ptr = node.unwrap_leaf();
            Cursor {
                node: leaf_ptr,
                idx: 0,
                offset: 0,
                // _marker: marker::PhantomData
            }
        }
    }

    pub fn iter(&self) -> Cursor<E, I> { self.cursor_at_start() }

    pub fn item_iter(&self) -> ItemIterator<E, I> {
        ItemIterator(self.iter())
    }

    pub fn next_entry_or_panic(cursor: &mut Cursor<E, I>, marker: &mut I::IndexUpdate) {
        if !cursor.next_entry_marker(Some(marker)) {
            panic!("Local delete past the end of the document");
        }
    }

    // Returns size.
    fn check_leaf(leaf: &NodeLeaf<E, I>, expected_parent: ParentPtr<E, I>) -> I::IndexOffset {
        assert_eq!(leaf.parent, expected_parent);
        
        // let mut count: usize = 0;
        let mut count = I::IndexOffset::default();
        let mut done = false;
        let mut num: usize = 0;

        for e in &leaf.data[..] {
            if e.is_valid() {
                // Make sure there's no data after an invalid entry
                assert_eq!(done, false, "Leaf contains gaps");
                assert_ne!(e.len(), 0, "Invalid leaf - 0 length");
                // count += e.content_len() as usize;
                I::increment_offset(&mut count, &e);
                num += 1;
            } else {
                done = true;
            }
        }

        // An empty leaf is only valid if we're the root element.
        if let ParentPtr::Internal(_) = leaf.parent {
            assert!(num > 0, "Non-root leaf is empty");
        }

        assert_eq!(num, leaf.num_entries as usize, "Cached leaf len does not match");

        count
    }
    
    // Returns size.
    fn check_internal(node: &NodeInternal<E, I>, expected_parent: ParentPtr<E, I>) -> I::IndexOffset {
        assert_eq!(node.parent, expected_parent);
        
        // let mut count_total: usize = 0;
        let mut count_total = I::IndexOffset::default();
        let mut done = false;
        let mut child_type = None; // Make sure all the children have the same type.
        // let self_parent = ParentPtr::Internal(NonNull::new(node as *const _ as *mut _).unwrap());
        let self_parent = unsafe { node.to_parent_ptr() };

        for (child_count_expected, child) in &node.data[..] {
            if let Some(child) = child {
                // Make sure there's no data after an invalid entry
                assert_eq!(done, false);

                let child_ref = child;

                let actual_type = match child_ref {
                    Node::Internal(_) => 1,
                    Node::Leaf(_) => 2,
                };
                // Make sure all children have the same type.
                if child_type.is_none() { child_type = Some(actual_type) }
                else { assert_eq!(child_type, Some(actual_type)); }

                // Recurse
                let count_actual = match child_ref {
                    Node::Leaf(ref n) => { Self::check_leaf(n.as_ref().get_ref(), self_parent) },
                    Node::Internal(ref n) => { Self::check_internal(n.as_ref().get_ref(), self_parent) },
                };

                // Make sure all the individual counts match.
                // if *child_count_expected as usize != count_actual {
                //     eprintln!("xxx {:#?}", node);
                // }
                assert_eq!(*child_count_expected, count_actual, "Child node count does not match");
                count_total += count_actual;
            } else {
                done = true;
            }
        }

        count_total
    }

    pub fn check(&self) {
        // Check the parent of each node is its correct parent
        // Check the size of each node is correct up and down the tree
        // println!("check tree {:#?}", self);
        let root = &self.root;
        let expected_parent = ParentPtr::Root(unsafe { ref_to_nonnull(self) });
        let expected_size = match root {
            Node::Internal(n) => { Self::check_internal(&n, expected_parent) },
            Node::Leaf(n) => { Self::check_leaf(&n, expected_parent) },
        };
        assert_eq!(self.count, expected_size, "tree.count is incorrect");
    }

    fn print_node_tree(node: &Node<E, I>, depth: usize) {
        for _ in 0..depth { eprint!("  "); }
        match node {
            Node::Internal(n) => {
                let n = n.as_ref().get_ref();
                eprintln!("Internal {:?} (parent: {:?})", n as *const _, n.parent);
                let mut unused = 0;
                for (_, e) in &n.data[..] {
                    if let Some(e) = e {
                        Self::print_node_tree(e, depth + 1);
                    } else { unused += 1; }
                }

                if unused > 0 {
                    for _ in 0..=depth { eprint!("  "); }
                    eprintln!("({} empty places)", unused);
                }
            },
            Node::Leaf(n) => {
                eprintln!("Leaf {:?} (parent: {:?}) - {} filled", n as *const _, n.parent, n.len_entries());
            }
        }
    }

    #[allow(unused)]
    pub fn print_ptr_tree(&self) {
        eprintln!("Tree count {:?} ptr {:?}", self.count, self as *const _);
        Self::print_node_tree(&self.root, 1);
    }

    /// Returns a cursor right before the named location, referenced by the pointer.
    pub unsafe fn cursor_before_item(loc: E::Item, ptr: NonNull<NodeLeaf<E, I>>) -> Cursor<E, I> {
        // First make a cursor to the specified item
        let leaf = ptr.as_ref();
        leaf.find(loc).expect("Position not in named leaf")
    }

    #[allow(unused)]
    pub fn print_stats(&self, detailed: bool) {
        // We'll get the distribution of entry sizes
        let mut size_counts = vec!();

        for entry in self.iter() {
            // println!("entry {:?}", entry);
            let bucket = entry.len() as usize;
            if bucket >= size_counts.len() {
                size_counts.resize(bucket + 1, 0);
            }
            size_counts[bucket] += 1;
        }

        let num_internal_nodes = self.count_internal_nodes();

        println!("-------- Range tree stats --------");
        println!("Number of entries {}", self.count_entries());
        println!("Number of internal nodes {}", num_internal_nodes);
        println!("(Size of internal nodes {})",
            (num_internal_nodes * size_of::<NodeInternal<E, I>>()).file_size(file_size_opts::CONVENTIONAL).unwrap());
        println!("Depth {}", self.get_depth());
        println!("Total range tree memory usage {}", self.count_total_memory().file_size(file_size_opts::CONVENTIONAL).unwrap());

        println!("(efficient size: {})", (self.count_entries() * size_of::<E>()).file_size(file_size_opts::CONVENTIONAL).unwrap());

        if detailed {
            println!("Entry distribution {:?}", size_counts);
            println!("Internal node size {}", std::mem::size_of::<NodeInternal<E, I>>());
            println!("Node entry size {} alignment {}",
                     std::mem::size_of::<Option<Node<E, I>>>(),
                     std::mem::align_of::<Option<Node<E, I>>>());
            println!("Leaf size {}", std::mem::size_of::<NodeLeaf<E, I>>());
        }
    }

    fn get_depth(&self) -> usize {
        unsafe {
            let mut depth = 0;
            let mut node = self.root.as_ptr();
            while let NodePtr::Internal(data) = node {
                depth += 1;
                node = data.as_ref().data[0].1.as_ref().unwrap().as_ptr()
            };
            depth
        }
    }

    #[allow(unused)]
    pub(crate) fn count_entries(&self) -> usize {
        self.iter().fold(0, |a, _| a + 1)
    }

    fn count_internal_nodes_internal(node: &Node<E, I>, num: &mut usize) {
        if let Node::Internal(n) = node {
            *num += 1;

            for (_, e) in &n.data[..] {
                if let Some(e) = e {
                    Self::count_internal_nodes_internal(e, num);
                }
            }
        }
    }

    #[allow(unused)]
    pub(crate) fn count_internal_nodes(&self) -> usize {
        let mut num = 0;
        Self::count_internal_nodes_internal(&self.root, &mut num);
        num
    }

    fn count_memory_internal(node: &Node<E, I>, size: &mut usize) {
        match node {
            Node::Internal(n) => {
                *size += size_of::<NodeInternal<E, I>>();

                for (_, e) in &n.data[..] {
                    if let Some(e) = e {
                        Self::count_memory_internal(e, size);
                    }
                }
            }
            Node::Leaf(_) => {
                *size += std::mem::size_of::<NodeLeaf<E, I>>();
            }
        }
    }

    #[allow(unused)]
    pub(crate) fn count_total_memory(&self) -> usize {
        let mut size = size_of::<RangeTree<E, I>>();
        Self::count_memory_internal(&self.root, &mut size);
        size
    }
}

impl<E: EntryTraits> RangeTree<E, RawPositionIndex> {
    pub fn cursor_at_offset_pos(&self, pos: usize, stick_end: bool) -> Cursor<E, RawPositionIndex> {
        self.cursor_at_query(pos, stick_end,
                             |i| i as usize,
                             |e| e.len())
    }

    pub fn at(&self, pos: usize) -> Option<E::Item> {
        let cursor = self.cursor_at_offset_pos(pos, false);
        cursor.get_item()
    }
}
impl<E: EntryTraits + EntryWithContent> RangeTree<E, ContentIndex> {
    pub fn content_len(&self) -> usize {
        self.count as usize
    }

    pub fn cursor_at_content_pos(&self, pos: usize, stick_end: bool) -> Cursor<E, ContentIndex> {
        self.cursor_at_query(pos, stick_end,
                                         |i| i as usize,
                                         |e| e.content_len())
    }
}
impl<E: EntryTraits + EntryWithContent> RangeTree<E, FullIndex> {
    pub fn content_len(&self) -> usize {
        self.count.content as usize
    }

    pub fn cursor_at_content_pos(&self, pos: usize, stick_end: bool) -> Cursor<E, FullIndex> {
        self.cursor_at_query(pos, stick_end,
                                         |i| i.content as usize,
                                         |e| e.content_len())
    }

    pub fn cursor_at_offset_pos(&self, pos: usize, stick_end: bool) -> Cursor<E, FullIndex> {
        self.cursor_at_query(pos, stick_end,
                                         |i| i.len as usize,
                                         |e| e.len())
    }
}

#[cfg(test)]
mod tests {
    use crate::range_tree::{RangeTree, CRDTSpan, ContentIndex, FullIndex, TreeIndex};
    use std::mem::size_of;

    #[test]
    fn print_memory_stats() {
        let x = RangeTree::<CRDTSpan, ContentIndex>::new();
        x.print_stats(false);
        let x = RangeTree::<CRDTSpan, FullIndex>::new();
        x.print_stats(false);

        println!("sizeof ContentIndex offset {}", size_of::<<ContentIndex as TreeIndex<CRDTSpan>>::IndexOffset>());
        println!("sizeof FullIndex offset {}", size_of::<<FullIndex as TreeIndex<CRDTSpan>>::IndexOffset>());
    }
}