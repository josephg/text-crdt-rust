use crate::list::*;
// use crate::split_list::SplitList;
use crate::range_tree::{RangeTree, Cursor, NodeLeaf};
use crate::common::{AgentId, LocalOp};
use smallvec::smallvec;
use std::ptr::NonNull;
use crate::splitable_span::SplitableSpan;
use std::cmp::Ordering;
use crate::rle::Rle;
use std::iter::FromIterator;
use std::mem::replace;
use crate::list::external_txn::{RemoteTxn, RemoteOp, RemoteId};

// #[cfg(inlinerope)]
// const USE_INNER_ROPE: bool = true;
// #[cfg(not(inlinerope))]
const USE_INNER_ROPE: bool = false;

impl ClientData {
    pub fn get_next_seq(&self) -> u32 {
        if let Some(KVPair(loc, range)) = self.item_orders.last() {
            loc + range.len as u32
        } else { 0 }
    }

    pub fn seq_to_order(&self, seq: u32) -> Order {
        let (x, offset) = self.item_orders.find(seq).unwrap();
        x.1.order + offset
    }
}

/// Advance branch frontier by a transaction. This is written creating a new branch, which is
/// somewhat inefficient (especially if the frontier is spilled).
fn advance_branch_by(branch: &mut Branch, txn_parents: &Branch, first_order: Order, len: u32) {
    // TODO: Check the branch contains everything in txn_parents, but not txn_id:
    // Check the operation fits. The operation should not be in the branch, but
    // all the operation's parents should be.
    // From braid-kernel:
    // assert(!branchContainsVersion(db, order, branch), 'db already contains version')
    // for (const parent of op.parents) {
    //    assert(branchContainsVersion(db, parent, branch), 'operation in the future')
    // }
    assert!(!branch.contains(&first_order)); // Remove this when branch_contains_version works.

    // TODO: Consider sorting the branch after we do this.
    branch.retain(|o| !txn_parents.contains(o)); // Usually removes all elements.
    branch.push(first_order + len - 1);
}

impl ListCRDT {
    pub fn new() -> Self {
        ListCRDT {
            client_with_order: Rle::new(),
            frontier: smallvec![ROOT_ORDER],
            client_data: vec![],
            // markers: RangeTree::new(),
            index: SplitList::new(),
            range_tree: RangeTree::new(),
            text_content: Rope::new(),
            deletes: Rle::new(),
            double_deletes: Rle::new(),
            txns: Rle::new(),
        }
    }

    pub fn get_or_create_agent_id(&mut self, name: &str) -> AgentId {
        // Probably a nicer way to write this.
        if name == "ROOT" { return AgentId::MAX; }

        if let Some(id) = self.get_agent_id(name) {
            id
        } else {
            // Create a new id.
            self.client_data.push(ClientData {
                name: SmartString::from(name),
                item_orders: Rle::new()
            });
            (self.client_data.len() - 1) as AgentId
        }
    }

    fn get_agent_id(&self, name: &str) -> Option<AgentId> {
        if name == "ROOT" { Some(AgentId::MAX) }
        else {
            self.client_data.iter()
                .position(|client_data| client_data.name == name)
                .map(|id| id as AgentId)
        }
    }

    fn get_agent_name(&self, agent: AgentId) -> &str {
        self.client_data[agent as usize].name.as_str()
    }

    fn get_next_order(&self) -> Order {
        if let Some(KVPair(base, entry)) = self.client_with_order.last() {
            base + entry.len as u32
        } else { 0 }
    }

    fn marker_at(&self, order: Order) -> NonNull<NodeLeaf<YjsSpan, ContentIndex>> {
        // let cursor = self.markers.cursor_at_offset_pos(order as usize, false);
        // cursor.get_item().unwrap().unwrap()
        // self.markers.find(order).unwrap().0.ptr

        self.index.entry_at(order as usize).unwrap_ptr()
    }

    fn get_cursor_before(&self, order: Order) -> Cursor<YjsSpan, ContentIndex> {
        if order == ROOT_ORDER {
            // Or maybe we should just abort?
            self.range_tree.cursor_at_end()
        } else {
            let marker = self.marker_at(order);
            unsafe {
                RangeTree::cursor_before_item(order, marker)
            }
        }
    }

    fn get_cursor_after(&self, order: Order) -> Cursor<YjsSpan, ContentIndex> {
        if order == ROOT_ORDER {
            self.range_tree.cursor_at_start()
        } else {
            let marker = self.marker_at(order);
            // let marker: NonNull<NodeLeaf<YjsSpan, ContentIndex>> = self.markers.at(order as usize).unwrap();
            // self.range_tree.
            let mut cursor = unsafe {
                RangeTree::cursor_before_item(order, marker)
            };
            // The cursor points to parent. This is safe because of guarantees provided by
            // cursor_before_item.
            cursor.offset += 1;
            cursor
        }
    }

    // fn time_diff(&self, a: Branch, b: Branch) -> (SmallVec<[OrderSpan; 4]>, SmallVec<[OrderSpan; 4]>) {
    //
    //
    // }

    fn notify(markers: &mut SpaceIndex, entry: YjsSpan, ptr: NonNull<NodeLeaf<YjsSpan, ContentIndex>>) {
        // println!("notify {:?}", &entry);

        // let cursor = markers.cursor_at_offset_pos(entry.order as usize, true);
        // markers.replace_range(cursor, MarkerEntry {
        //     ptr: Some(ptr), len: entry.len() as u32
        // }, |_,_| {});
        markers.replace_range(entry.order as usize, MarkerEntry {
            ptr: Some(ptr), len: entry.len() as u32
        });
    }

    fn assign_order_to_client(&mut self, loc: CRDTLocation, order: Order, len: usize) {
        self.client_with_order.append(KVPair(order, CRDTSpan {
            loc,
            len: len as _
        }));

        self.client_data[loc.agent as usize].item_orders.append(KVPair(loc.seq, OrderSpan {
            order,
            len: len as _
        }));
    }

    fn integrate(&mut self, agent: AgentId, item: YjsSpan, ins_content: &str, cursor_hint: Option<Cursor<YjsSpan, ContentIndex>>) {
        // if cfg!(debug_assertions) {
        //     let next_order = self.get_next_order();
        //     assert_eq!(item.order, next_order);
        // }

        // self.assign_order_to_client(loc, item.order, item.len as _);

        // Ok now that's out of the way, lets integrate!
        let mut cursor = cursor_hint.unwrap_or_else(|| {
            self.get_cursor_after(item.origin_left)
        });
        let left_cursor = cursor;
        let mut scan_start = cursor;
        let mut scanning = false;

        loop {
            let other_order = match cursor.get_item() {
                None => { break; } // End of the document
                Some(o) => { o }
            };

            // Almost always true.
            if other_order == item.origin_right { break; }

            // This code could be better optimized, but its already O(n * log n), and its extremely
            // rare that you actually get concurrent inserts at the same location in the document
            // anyway.

            let other_entry = cursor.get_entry();
            let other_left_order = other_entry.origin_left_at_offset(cursor.offset as u32);
            let other_left_cursor = self.get_cursor_after(other_left_order);

            // Yjs semantics.
            match std::cmp::Ord::cmp(&other_left_cursor, &left_cursor) {
                Ordering::Less => { break; } // Top row
                Ordering::Greater => { } // Bottom row. Continue.
                Ordering::Equal => {
                    // These items might be concurrent.
                    let my_name = self.get_agent_name(agent);
                    let other_loc = self.client_with_order.get(other_entry.order);
                    let other_name = self.get_agent_name(other_loc.agent);
                    if my_name > other_name {
                        scanning = false;
                    } else if item.origin_right == other_entry.origin_right {
                        break;
                    } else {
                        scanning = true;
                        scan_start = cursor;
                    }
                }
            }

            cursor.next_entry();
        }
        if scanning { cursor = scan_start; }

        // Now insert here.
        let markers = &mut self.index;
        self.range_tree.insert(cursor, item, |entry, leaf| {
            Self::notify(markers, entry, leaf);
        });

        if USE_INNER_ROPE {
            let pos = cursor.count_pos() as usize;
            self.text_content.insert(pos, ins_content);
        }
    }

    fn remote_id_to_order(&self, id: &RemoteId) -> Order {
        let agent = self.get_agent_id(id.agent.as_str()).unwrap();
        if agent == AgentId::MAX { ROOT_ORDER }
        else { self.client_data[agent as usize].seq_to_order(id.seq) }
    }

    pub fn apply_remote_txn(&mut self, txn: &RemoteTxn) {
        let agent = self.get_or_create_agent_id(txn.id.agent.as_str());
        let client = &self.client_data[agent as usize];
        let next_seq = client.get_next_seq();
        // If the seq does not match we either need to skip or buffer the transaction.
        assert_eq!(next_seq, txn.id.seq);

        let first_order = self.get_next_order();
        let mut next_order = first_order;

        // Figure out the order range for this txn and assign
        let mut txn_len = 0;
        for op in txn.ops.iter() {
            match op {
                RemoteOp::Ins { ins_content, .. } => {
                    txn_len += ins_content.chars().count();
                }
                RemoteOp::Del { len, .. } => {
                    txn_len += *len as usize;
                }
            }
        }

        // TODO: This may be premature - we may be left in an invalid state if the txn is invalid.
        self.assign_order_to_client(CRDTLocation {
            agent,
            seq: txn.id.seq,
        }, first_order, txn_len);

        // Apply the changes.
        for op in txn.ops.iter() {
            match op {
                RemoteOp::Ins { origin_left, origin_right, ins_content } => {
                    let ins_len = ins_content.chars().count();

                    let order = next_order;
                    next_order += ins_len as u32;

                    // Convert origin left and right to order numbers
                    let origin_left = self.remote_id_to_order(&origin_left);
                    let origin_right = self.remote_id_to_order(&origin_right);

                    let item = YjsSpan {
                        order,
                        origin_left,
                        origin_right,
                        len: ins_len as i32
                    };
                    // dbg!(item);

                    self.integrate(agent, item, ins_content.as_str(), None);
                }

                RemoteOp::Del { id, len } => {
                    // The order of this delete operation
                    let order = next_order;
                    next_order += len;

                    // The order of the item we're deleting
                    let mut target_order = self.remote_id_to_order(&id);

                    // We're deleting a span of target_order..target_order+len.

                    self.deletes.append(KVPair(order, DeleteEntry {
                        order: target_order,
                        len: *len
                    }));

                    let mut remaining_len = *len;
                    while remaining_len > 0 {
                        // We need to loop here because the deleted items may not be in a run
                        // in the local range tree. They usually will be though.
                        let cursor = self.get_cursor_before(target_order);

                        let markers = &mut self.index;
                        let amt_deactivated = self.range_tree.remote_deactivate(cursor, remaining_len as _, |entry, leaf| {
                            Self::notify(markers, entry, leaf);
                        });

                        let deleted_here = amt_deactivated.abs() as u32;
                        if amt_deactivated < 0 {
                            // This span was already deleted by a different peer. Mark duplicate delete.
                            self.double_deletes.increment_delete_range(target_order, deleted_here);
                        }
                        remaining_len -= deleted_here;
                        target_order += deleted_here;

                        if USE_INNER_ROPE {
                            // Use cursor to figure out the position + span.
                            todo!()
                            // self.text_content.remove(pos..pos + *del_span);
                        }
                    }

                    // TODO: Remove me. This is only needed because SplitList doesn't support gaps.
                    self.index.append_entry(self.index.last().map_or(MarkerEntry::default(), |m| {
                        MarkerEntry { len: *len, ptr: m.ptr }
                    }));
                }
            }
        }

        let parents: Branch = SmallVec::from_iter(txn.parents.iter().map(|remote_id| {
            self.remote_id_to_order(remote_id)
        }));
        self.insert_txn(Some(parents), first_order, txn_len as u32);
    }

    fn insert_txn(&mut self, txn_parents: Option<Branch>, first_order: Order, len: u32) {
        let last_order = first_order + len - 1;
        let txn_parents = if let Some(txn_parents) = txn_parents {
            advance_branch_by(&mut self.frontier, &txn_parents, first_order, len);
            txn_parents
        } else {
            // Local change - Use the current frontier as the txn's parents.
            // The new frontier points to the last order in the txn.
            replace(&mut self.frontier, smallvec![last_order])
        };
        // let parents = replace(&mut self.frontier, txn_parents);
        let mut shadow = first_order;
        while shadow >= 1 && txn_parents.contains(&(shadow - 1)) {
            shadow = self.txns.find(shadow - 1).unwrap().0.shadow;
        }

        let txn = TxnSpan {
            order: first_order,
            len,
            shadow,
            parents: SmallVec::from_iter(txn_parents.into_iter())
        };

        self.txns.append(txn);
    }

    pub fn apply_local_txn(&mut self, agent: AgentId, local_ops: &[LocalOp]) {
        let first_order = self.get_next_order();
        let mut next_order = first_order;

        let mut txn_span = 0;
        for LocalOp { pos: _, ins_content, del_span } in local_ops {
            txn_span += *del_span;
            txn_span += ins_content.chars().count();
        }

        self.assign_order_to_client(CRDTLocation {
            agent,
            seq: self.client_data[agent as usize].get_next_seq()
        }, first_order, txn_span);


        for LocalOp { pos, ins_content, del_span } in local_ops {
            let pos = *pos;
            if *del_span > 0 {
                let cursor = self.range_tree.cursor_at_content_pos(pos, false);
                let markers = &mut self.index;
                let deleted_items = self.range_tree.local_deactivate(cursor, *del_span, |entry, leaf| {
                    Self::notify(markers, entry, leaf);
                });

                // TODO: Remove me. This is only needed because SplitList doesn't support gaps.
                self.index.append_entry(self.index.last().map_or(MarkerEntry::default(), |m| {
                    MarkerEntry { len: *del_span as u32, ptr: m.ptr }
                }));

                // let cursor = self.markers.cursor_at_end();
                // self.markers.insert(cursor, MarkerEntry {
                //     ptr: None,
                //     len: *del_span as u32,
                // }, |_, _| {});

                // dbg!(&deleted_items);
                let mut deleted_length = 0; // To check.
                for item in deleted_items {
                    // self.markers.append_entry(MarkerEntry::Del {
                    //     len: item.len as u32,
                    //     order: item.order
                    // });

                    self.deletes.append(KVPair(next_order, DeleteEntry {
                        order: item.order,
                        len: item.len as u32
                    }));
                    deleted_length += item.len as usize;
                    next_order += item.len as u32;
                }
                // I might be able to relax this, but we'd need to change del_span above.
                assert_eq!(deleted_length, *del_span);

                if USE_INNER_ROPE {
                    self.text_content.remove(pos..pos + *del_span);
                }
            }

            if !ins_content.is_empty() {
                // First we need the insert's base order
                let ins_len = ins_content.chars().count();

                let order = next_order;
                next_order += ins_len as u32;

                // Find the preceeding item and successor
                let (origin_left, cursor) = if pos == 0 {
                    (ROOT_ORDER, self.range_tree.cursor_at_start())
                } else {
                    let mut cursor = self.range_tree.cursor_at_content_pos(pos - 1, false);
                    let origin_left = cursor.get_item().unwrap();
                    assert!(cursor.next());
                    (origin_left, cursor)
                };

                // TODO: This should scan & skip past deleted items!
                let origin_right = cursor.get_item().unwrap_or(ROOT_ORDER);

                let item = YjsSpan {
                    order,
                    origin_left,
                    origin_right,
                    len: ins_len as i32
                };
                // dbg!(item);

                self.integrate(agent, item, ins_content.as_str(), Some(cursor));
            }
        }

        self.insert_txn(None, first_order, next_order - first_order);
        debug_assert_eq!(next_order, self.get_next_order());
    }

    // pub fn internal_insert(&mut self, agent: AgentId, pos: usize, ins_content: SmartString) -> Order {
    pub fn local_insert(&mut self, agent: AgentId, pos: usize, ins_content: SmartString) {
        self.apply_local_txn(agent, &[LocalOp {
            ins_content, pos, del_span: 0
        }])
    }

    pub fn local_delete(&mut self, agent: AgentId, pos: usize, del_span: usize) {
        self.apply_local_txn(agent, &[LocalOp {
            ins_content: SmartString::default(), pos, del_span
        }])
    }

    pub fn len(&self) -> usize {
        self.range_tree.content_len()
    }

    pub fn is_empty(&self) -> bool {
        self.range_tree.len() != 0
    }

    pub fn print_stats(&self, detailed: bool) {
        self.range_tree.print_stats(detailed);
        self.index.print_stats("index", detailed);
        // self.markers.print_rle_size();
        self.deletes.print_stats("deletes", detailed);
        self.txns.print_stats("txns", detailed);
    }
}

impl ToString for ListCRDT {
    fn to_string(&self) -> String {
        self.text_content.to_string()
    }
}

impl Default for ListCRDT {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use crate::list::*;
    use rand::prelude::*;
    use crate::common::*;
    use crate::list::doc::USE_INNER_ROPE;
    use crate::list::external_txn::{RemoteTxn, RemoteId, RemoteOp};
    use smallvec::smallvec;

    #[test]
    fn smoke() {
        let mut doc = ListCRDT::new();
        doc.get_or_create_agent_id("seph"); // 0
        doc.local_insert(0, 0, "hi".into());
        doc.local_insert(0, 1, "yooo".into());
        doc.local_delete(0, 0, 3);
        // "hyoooi"

        dbg!(doc);
    }


    fn random_str(len: usize, rng: &mut SmallRng) -> String {
        let mut str = String::new();
        let alphabet: Vec<char> = "abcdefghijklmnop_".chars().collect();
        for _ in 0..len {
            str.push(alphabet[rng.gen_range(0..alphabet.len())]);
        }
        str
    }

    fn make_random_change(doc: &mut ListCRDT, rope: &mut Rope, agent: AgentId, rng: &mut SmallRng) {
        let doc_len = doc.len();
        let insert_weight = if doc_len < 100 { 0.55 } else { 0.45 };
        if doc_len == 0 || rng.gen_bool(insert_weight) {
            // Insert something.
            let pos = rng.gen_range(0..=doc_len);
            let len: usize = rng.gen_range(1..2); // Ideally skew toward smaller inserts.
            // let len: usize = rng.gen_range(1..10); // Ideally skew toward smaller inserts.

            let content = random_str(len as usize, rng);
            // println!("Inserting '{}' at position {}", content, pos);
            rope.insert(pos, content.as_str());
            doc.local_insert(agent, pos, content.into())
        } else {
            // Delete something
            let pos = rng.gen_range(0..doc_len);
            // println!("range {}", u32::min(10, doc_len - pos));
            let span = rng.gen_range(1..=usize::min(10, doc_len - pos));
            // dbg!(&state.marker_tree, pos, len);
            // println!("deleting {} at position {}", span, pos);
            rope.remove(pos..pos+span);
            doc.local_delete(agent, pos, span)
        }
        // dbg!(&doc.markers);
        doc.index.check();
    }

    #[test]
    fn random_single_document() {
        let mut rng = SmallRng::seed_from_u64(7);
        let mut doc = ListCRDT::new();

        let agent = doc.get_or_create_agent_id("seph");
        let mut expected_content = Rope::new();

        for _i in 0..1000 {
            make_random_change(&mut doc, &mut expected_content, agent, &mut rng);
            if USE_INNER_ROPE {
                assert_eq!(doc.text_content, expected_content);
            }
        }
        assert_eq!(doc.client_data[0].item_orders.num_entries(), 1);
        assert_eq!(doc.client_with_order.num_entries(), 1);
    }

    #[test]
    fn deletes_merged() {
        let mut doc = ListCRDT::new();
        doc.get_or_create_agent_id("seph");
        doc.local_insert(0, 0, "abc".into());
        // doc.local_delete(0, 2, 1);
        // doc.local_delete(0, 1, 1);
        // doc.local_delete(0, 0, 1);
        doc.local_delete(0, 0, 1);
        doc.local_delete(0, 0, 1);
        doc.local_delete(0, 0, 1);
        dbg!(doc);
    }

    // #[test]
    // fn shadow() {
    //     let mut doc = ListCRDT::new();
    //     let seph = doc.get_or_create_client_id("seph");
    //     let mike = doc.get_or_create_client_id("mike");
    //
    //     doc.local_insert(seph, 0, "a".into());
    //     assert_eq!(doc.txns.find(0).unwrap().0.shadow, 0);
    // }

    fn root_id() -> RemoteId {
        RemoteId {
            agent: "ROOT".into(),
            seq: u32::MAX
        }
    }

    #[test]
    fn remote_txns() {
        let mut doc_remote = ListCRDT::new();
        doc_remote.apply_remote_txn(&RemoteTxn {
            id: RemoteId {
                agent: "seph".into(),
                seq: 0
            },
            parents: smallvec![root_id()],
            ops: smallvec![
                RemoteOp::Ins {
                    origin_left: root_id(),
                    origin_right: root_id(),
                    ins_content: "hi".into()
                }
            ]
        });

        let mut doc_local = ListCRDT::new();
        doc_local.get_or_create_agent_id("seph");
        doc_local.local_insert(0, 0, "hi".into());
        // dbg!(&doc_remote);
        assert_eq!(doc_remote.frontier, doc_local.frontier);
        assert_eq!(doc_remote.txns, doc_local.txns);
        assert_eq!(doc_remote.text_content, doc_local.text_content);
        assert_eq!(doc_remote.deletes, doc_local.deletes);

        doc_remote.apply_remote_txn(&RemoteTxn {
            id: RemoteId {
                agent: "seph".into(),
                seq: 2
            },
            parents: smallvec![RemoteId {
                agent: "seph".into(),
                seq: 1
            }],
            ops: smallvec![
                RemoteOp::Del {
                    id: RemoteId {
                        agent: "seph".into(),
                        seq: 0
                    },
                    len: 2,
                }
            ]
        });

        // dbg!(&doc_remote);
        doc_local.local_delete(0, 0, 2);
        // dbg!(&doc_local);

        assert_eq!(doc_remote.frontier, doc_local.frontier);
        assert_eq!(doc_remote.txns, doc_local.txns);
        assert_eq!(doc_remote.text_content, doc_local.text_content);
        assert_eq!(doc_remote.deletes, doc_local.deletes);

    }
}