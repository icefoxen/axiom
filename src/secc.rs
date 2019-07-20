//! An Skip Enabled Concurrent Channel (SECC) is a bounded capacity channel that allows users
//! to send and receive messages from multiple threads and allows the receiver to temporarily skip
//! receiving messages if they desire.
//!
//! The channel is a FIFO structure unless the user intends to skip one or more messages
//! in which case a message could be read in a different order. The channel does, however,
//! guarantee that the messages will remain in the same order as sent and, unless skipped, will
//! be received in order.
//!
//! The module is implemented using two linked lists where one list acts as a pool of nodes and
//! the other list acts as the queue holding the messages. This allows us to move nodes in and out
//! of the list and even skip a message with O(1) efficiency. If there are 1000 messages and
//! the user desires to skip one in the middle they will incur virtually the exact same
//! performance cost as a normal read operation. There are only a couple of additional pointer
//! operations necessary to remove a node out of the middle of the linked list that implements
//! the queue.  When a message is received from the channel the node holding the message is
//! removed from the queue and appended to the tail of the pool. Conversely, when a  message is
//! sent to the channel the node moves from the head of the pool to the tail of the queue. In
//! this manner nodes are constantly cycled in and out of the queue so we only need to allocate
//! them once when the channel is created.

use std::cell::UnsafeCell;
use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// A message that is used to indicate that a position index points to no other node. Note that
/// this message is something beyond the capability of any user to allocate for the channel size.
const NIL_NODE: usize = 1 << 16 as usize;

/// Errors potentially returned from channel operations.
#[derive(Eq, PartialEq)]
pub enum SeccErrors<T: Sync + Send> {
    /// Channel is full, no more messages can be sent, the enclosed message contains the last
    /// message attempted to be sent.
    Full(T),

    /// Channel is empty so no more messages can be received. This can also be returned if there
    /// is an active cursor and there are no messages to receive after the cursor even though
    /// there are skipped messages.
    Empty,
}

impl<T: Sync + Send> fmt::Debug for SeccErrors<T> {
    fn fmt(&self, formatter: &'_ mut fmt::Formatter) -> fmt::Result {
        match self {
            SeccErrors::Full(_) => write!(formatter, "SeccErrors::Full"),
            SeccErrors::Empty => write!(formatter, "SeccErrors::Empty"),
        }
    }
}

/// A single node in the channel's buffer.
struct SeccNode<T: Sync + Send> {
    /// Contains a message set in the node in a Some or a None if empty.  Note that this is unsafe
    /// in order to get around Rust mutability locks so that this data structure can be passed
    /// around immutably but also still be able to send and receive.
    cell: UnsafeCell<Option<T>>,
    /// The pointer to the next node in the channel.
    next: AtomicUsize,
    // FIXME (Issue #12) Add tracking of time in channel by milliseconds.
}

impl<T: Sync + Send> SeccNode<T> {
    /// Creates a new node where the next index is set to the nil node.
    fn new() -> SeccNode<T> {
        SeccNode {
            cell: UnsafeCell::new(None),
            next: AtomicUsize::new(NIL_NODE),
        }
    }

    /// Creates a new node where the next index is the given message.
    fn with_next(next: usize) -> SeccNode<T> {
        SeccNode {
            cell: UnsafeCell::new(None),
            next: AtomicUsize::new(next),
        }
    }
}

pub trait SeccCoreOps<T: Sync + Send> {
    /// Fetch the core of the channel.
    fn core(&self) -> &SeccCore<T>;

    /// Returns the capacity of the channel.
    fn capacity(&self) -> usize {
        self.core().capacity
    }

    /// Count of the number of times receivers of this channel waited for messages.
    fn awaited_messages(&self) -> usize {
        self.core().awaited_messages.load(Ordering::Relaxed)
    }

    /// Count of the number of times senders to the channel waited for capacity.
    fn awaited_capacity(&self) -> usize {
        self.core().awaited_capacity.load(Ordering::Relaxed)
    }

    /// Returns the number of items are in the channel currently without regard to cursors.
    fn pending(&self) -> usize {
        self.core().pending.load(Ordering::Relaxed)
    }

    /// Number of messages in the channel that are available to be received. This will normally be
    /// the same as `pending` unless there is a skip cursor active; in which case it may be
    /// smaller than pending or even 0.
    fn receivable(&self) -> usize {
        self.core().receivable.load(Ordering::Relaxed)
    }

    /// Returns the total number of messages that have been sent to the channel.
    fn sent(&self) -> usize {
        self.core().sent.load(Ordering::Relaxed)
    }

    /// Returns the total number of objects messages that have been received from the channel.
    fn received(&self) -> usize {
        self.core().received.load(Ordering::Relaxed)
    }
}

/// A structure containing the pointers used when sending items to the channel.
#[derive(Debug)]
struct SeccSendPtrs {
    /// The tail of the queue which holds messages currently in the channel.
    queue_tail: usize,
    /// The head of the pool of available nodes to be used when sending messages to the channel.  
    /// Note that if there is only one node in the pool then the channel is full.
    pool_head: usize,
}

/// A structure containing pointers used when receiving messages from the channel.
#[derive(Debug)]
struct SeccReceivePtrs {
    /// The head of the queue which holds messages currently in the channel.  Note that if there
    /// is only one node in the queue then the channel is empty.
    queue_head: usize,
    /// The tail of the pool of available nodes to be used when sending messages to the channel.
    pool_tail: usize,
    /// Either [`NIL_NODE`], when there is no current skip cursor, or a pointer to the last
    /// element skipped.
    skipped: usize,
    /// Either [`NIL_NODE`], when there is no current skip cursor, or a pointer to the next
    /// element that can be received from the channel.
    cursor: usize,
}

/// Data structure that contains the core of the channel including tracking of statistics
/// and node storage.
pub struct SeccCore<T: Sync + Send> {
    /// Capacity of the channel, which is the total number of items that can be stored. Note that
    /// there will be 2 additional nodes allocated because neither the queue nor pool should ever
    /// be empty.
    capacity: usize,
    /// Node storage of the nodes. These nodes are never read directly except during allocation
    /// and tests. Note this field is preceded with an underscore because although the nodes
    /// live here they are never used directly once allocated.
    _nodes: Box<[SeccNode<T>]>,
    /// Pointers to the nodes in the channel. It is critical that these pointers never change
    /// order during the operations of the channel because the next pointers of the nodes refer
    /// to indexes in this vector rather than the raw pointers.
    node_ptrs: UnsafeCell<Vec<*mut SeccNode<T>>>,
    /// Indexes in the `node_ptrs` used for sending elements to the channel.  These pointers are
    /// paired together with a [`std::sync::Condvar`] that allows receivers awaiting messages
    /// to be notified that messages are available but this mutex should only be used by receivers
    /// with a [`std::sync::Condvar`] to prevent deadlocking the channel.
    send_ptrs: Arc<(Mutex<SeccSendPtrs>, Condvar)>,
    /// Indexes in the `node_ptrs` used for receiving elements from the channel. These pointers
    /// are combined with a [`std::sync::Condvar`] that can be used by senders awaiting capacity
    /// but the mutex should only be used by the senders with a [`std::sync::Condvar`] to avoid
    /// deadlocking the channel.
    receive_ptrs: Arc<(Mutex<SeccReceivePtrs>, Condvar)>,
    /// Count of the number of times receivers of this channel waited for messages.
    awaited_messages: AtomicUsize,
    /// Count of the number of times senders to the channel waited for capacity.
    awaited_capacity: AtomicUsize,
    /// Number of messages currently in the channel.
    pending: AtomicUsize,
    /// Number of messages in the channel that are available to be received. This will normally be
    /// the same as `pending` unless there is a skip cursor active; in which case it may be
    /// smaller than pending or even 0.
    receivable: AtomicUsize,
    /// Total number of messages that have been sent in the channel.
    sent: AtomicUsize,
    /// Total number of messages that have been received in the channel.
    received: AtomicUsize,
}

/// Sender side of the channel.
pub struct SeccSender<T: Sync + Send> {
    /// The core of the channel.
    core: Arc<SeccCore<T>>,
}

impl<T: Sync + Send> SeccSender<T> {
    /// Sends a message to the channel, the message will be moved into the channel, it will take
    /// ownership of the message. This function will either return an empty [`std::Result::Ok`] or
    /// an [`std::Result::Err`] containing the last message sent if something went wrong.
    pub fn send(&self, message: T) -> Result<(), SeccErrors<T>> {
        unsafe {
            // Retrieve send pointers and the encoded indexes inside them and their Condvar.
            let (ref mutex, ref condvar) = &*self.core.send_ptrs;
            let mut send_ptrs = mutex.lock().unwrap();

            // Get a pointer to the current pool_head and see if we have space to send.
            let pool_head_ptr = (*self.core.node_ptrs.get())[send_ptrs.pool_head];
            let next_pool_head = (*pool_head_ptr).next.load(Ordering::SeqCst);
            if NIL_NODE == next_pool_head {
                Err(SeccErrors::Full(message))
            } else {
                // We get the queue tail because the node from the pool will move here.
                let queue_tail_ptr = (*self.core.node_ptrs.get())[send_ptrs.queue_tail];

                // Add the message to the node, transferring ownership.
                (*(*queue_tail_ptr).cell.get()) = Some(message);

                // Update the pointers in the mutex.
                let old_pool_head = send_ptrs.pool_head;
                send_ptrs.queue_tail = send_ptrs.pool_head;
                send_ptrs.pool_head = next_pool_head;

                // Adjust channel metrics
                self.core.sent.fetch_add(1, Ordering::SeqCst);
                self.core.receivable.fetch_add(1, Ordering::SeqCst);
                self.core.pending.fetch_add(1, Ordering::SeqCst);

                // The now filled node will get moved to the queue.
                (*pool_head_ptr).next.store(NIL_NODE, Ordering::SeqCst);

                // We MUST set this LAST or we will get into a race with the receiver that would
                // think this node is ready for receiving when it isn't until just now.
                (*queue_tail_ptr)
                    .next
                    .store(old_pool_head, Ordering::SeqCst);

                // Notify anyone that was waiting on the Condvar and we are done.
                condvar.notify_all();
                Ok(())
            }
        }
    }

    /// Send to the channel, awaiting capacity if necessary, with an optional timeout. This
    /// function is semantically identical to [`axiom::secc::SeccSender::send`] but simply waits
    /// for there to be space in the channel before sending. If the timeout is not provided this
    /// function will wait forever for capacity.
    pub fn send_await_timeout(
        &self,
        mut message: T,
        timeout: Option<Duration>,
    ) -> Result<(), SeccErrors<T>> {
        loop {
            match self.send(message) {
                Err(SeccErrors::Full(v)) => {
                    message = v;
                    // We will put a condvar to be notified if space opens up.
                    let (ref mutex, ref condvar) = &*self.core.receive_ptrs;
                    let receive_ptrs = mutex.lock().unwrap();

                    // We will check if something got received before this function could create
                    // the condvar; this would mean we missed the condvar message and space is
                    // available to send.
                    let next_read_pos = unsafe {
                        let read_ptr = if receive_ptrs.cursor == NIL_NODE {
                            (*self.core.node_ptrs.get())[receive_ptrs.queue_head]
                        } else {
                            (*self.core.node_ptrs.get())[receive_ptrs.cursor]
                        };
                        (*read_ptr).next.load(Ordering::SeqCst)
                    };
                    if NIL_NODE != next_read_pos {
                        match timeout {
                            Some(dur) => {
                                // Wait for the specified time.
                                let result = condvar.wait_timeout(receive_ptrs, dur).unwrap();
                                if result.1.timed_out() {
                                    return Err(SeccErrors::Full(message));
                                }
                            }
                            None => {
                                // Wait forever
                                let _guard = condvar.wait(receive_ptrs).unwrap();
                            }
                        };
                        self.core.awaited_capacity.fetch_add(1, Ordering::SeqCst);
                    }
                }
                v => return v,
            }
        }
    }

    /// Helper to call [`axiom::secc::SeccSender::send_await_with_timeout`] using a
    /// [`std:Option::None`] for the timeout.
    pub fn send_await(&self, message: T) -> Result<(), SeccErrors<T>> {
        self.send_await_timeout(message, None)
    }
}

impl<T: Sync + Send> SeccCoreOps<T> for SeccSender<T> {
    fn core(&self) -> &SeccCore<T> {
        &self.core
    }
}

unsafe impl<T: Send + Sync> Send for SeccSender<T> {}

unsafe impl<T: Send + Sync> Sync for SeccSender<T> {}

/// Receiver side of the channel.
pub struct SeccReceiver<T: Sync + Send> {
    /// The core of the channel.
    core: Arc<SeccCore<T>>,
}

impl<T: Sync + Send> SeccReceiver<T> {
    /// Peeks at the next receivable message in the channel.
    pub fn peek(&self) -> Result<&T, SeccErrors<T>> {
        unsafe {
            // Retrieve receive pointers and the encoded indexes inside them.
            let (ref mutex, _) = &*self.core.receive_ptrs;
            let receive_ptrs = mutex.lock().unwrap();

            // Get a pointer to the queue_head or cursor and see check for anything receivable.
            let read_ptr = if receive_ptrs.cursor == NIL_NODE {
                (*self.core.node_ptrs.get())[receive_ptrs.queue_head]
            } else {
                (*self.core.node_ptrs.get())[receive_ptrs.cursor]
            };
            let next_read_pos = (*read_ptr).next.load(Ordering::SeqCst);
            if NIL_NODE == next_read_pos {
                return Err(SeccErrors::Empty);
            }

            // Extract the message and return a reference to it. If this panics then there
            // was somehow a receivable node with no message in it. That should never happen
            // under normal circumstances.
            let message: &T = (*((*read_ptr).cell).get())
                .as_ref()
                .expect("secc::peek(): empty receivable node");
            Ok(message)
        }
    }

    /// Receives the next message that is receivable. This will either receive the message at
    /// the head of the channel or, in the case that there is a skip cursor active, the next
    /// receivable message will be in the node at the skip cursor. This means that it is possible
    /// that receive could return an [`axiom::secc::SeccErrors::Empty`] when there are actually
    /// messages in the channel because there will be none readable until the skip is reset.
    pub fn receive(&self) -> Result<T, SeccErrors<T>> {
        unsafe {
            // Retrieve receive pointers and the encoded indexes inside them.
            let (ref mutex, ref condvar) = &*self.core.receive_ptrs;
            let mut receive_ptrs = mutex.lock().unwrap();

            // Get a pointer to the queue_head or cursor and see check for anything receivable.
            let read_ptr = if receive_ptrs.cursor == NIL_NODE {
                (*self.core.node_ptrs.get())[receive_ptrs.queue_head]
            } else {
                (*self.core.node_ptrs.get())[receive_ptrs.cursor]
            };
            let next_read_pos = (*read_ptr).next.load(Ordering::SeqCst);
            if NIL_NODE == next_read_pos {
                Err(SeccErrors::Empty)
            } else {
                // We can read something so we will pull the item out of the read pointer.
                let message: T = (*(*read_ptr).cell.get()).take().unwrap();

                // Now we have to manage either pulling a node out of the middle if there was a
                // cursor, or from the queue head if there was no cursor. Then we have to place
                // the released node on the pool tail.
                let pool_tail_ptr = (*self.core.node_ptrs.get())[receive_ptrs.pool_tail];
                (*read_ptr).next.store(NIL_NODE, Ordering::SeqCst);

                let new_pool_tail = if receive_ptrs.cursor == NIL_NODE {
                    // If we aren't using a cursor then the queue_head becomes the pool tail
                    receive_ptrs.pool_tail = receive_ptrs.queue_head;
                    let old_queue_head = receive_ptrs.queue_head;
                    receive_ptrs.queue_head = next_read_pos;
                    old_queue_head
                } else {
                    // If the cursor is set we have to dequeue in the middle of the list and fix
                    // the node chain and then move the node that the cursor was pointing at to
                    // the pool tail. Note that the precursor will never be [`NIL_NODE`] when the
                    // cursor is set because that would mean that there is no skip going on.
                    // Precursor is only ever set to a skipped node that could be read.
                    let skipped_ptr = (*self.core.node_ptrs.get())[receive_ptrs.skipped];
                    ((*skipped_ptr).next).store(next_read_pos, Ordering::SeqCst);
                    (*read_ptr).next.store(NIL_NODE, Ordering::SeqCst);
                    receive_ptrs.pool_tail = receive_ptrs.cursor;
                    let old_cursor = receive_ptrs.cursor;
                    receive_ptrs.cursor = next_read_pos;
                    old_cursor
                };

                // Update the channel metrics.
                self.core.received.fetch_add(1, Ordering::SeqCst);
                self.core.receivable.fetch_sub(1, Ordering::SeqCst);
                self.core.pending.fetch_sub(1, Ordering::SeqCst);

                // Finally add the new pool tail to the previous pool tail. We MUST set this
                // LAST or we get into a race with the sender which would think that the node
                // is available for sending when it actually isn't until just now.
                (*pool_tail_ptr).next.store(new_pool_tail, Ordering::SeqCst);

                // Notify anyone waiting on messages to be available.
                condvar.notify_all();

                // Return the message associated.
                Ok(message)
            }
        }
    }

    /// messages in the channel or an error if the channel was empty.
    pub fn pop(&self) -> Result<(), SeccErrors<T>> {
        self.receive()?;
        Ok(())
    }

    /// A helper to call [`axiom::secc::SeccReceiver::receive`] and await receivable messages
    /// until a specified optional timeout has expired. If the timeout is [`std::Option::None`]
    /// then this function will wait forever for new messages.
    pub fn receive_await_timeout(&self, timeout: Option<Duration>) -> Result<T, SeccErrors<T>> {
        loop {
            match self.receive() {
                Err(SeccErrors::Empty) => {
                    let (ref mutex, ref condvar) = &*self.core.send_ptrs;
                    let send_ptrs = mutex.lock().unwrap();

                    // We will check if something got sent to the channel before this function
                    // could create the Condvar and thus the function missed the Condvar notify
                    // and there is content to read.
                    let next_pool_head = unsafe {
                        let pool_head_ptr = (*self.core.node_ptrs.get())[send_ptrs.pool_head];
                        (*pool_head_ptr).next.load(Ordering::SeqCst)
                    };
                    if NIL_NODE != next_pool_head {
                        // In this case there is still nothing to read so we set up a Condvar
                        // and wait for the sender to notify us of new available messages.
                        match timeout {
                            Some(dur) => {
                                let result = condvar.wait_timeout(send_ptrs, dur).unwrap();
                                if result.1.timed_out() {
                                    return Err(SeccErrors::Empty);
                                }
                            }
                            None => {
                                let _condvar_guard = condvar.wait(send_ptrs).unwrap();
                            }
                        };
                        self.core.awaited_messages.fetch_add(1, Ordering::SeqCst);
                    }
                }
                v => return v,
            }
        }
    }

    /// A helper to call [`axiom::secc::SeccReceiver::receive_await_timeout`] with [`None`]
    /// for a timeout.
    pub fn receive_await(&self) -> Result<T, SeccErrors<T>> {
        self.receive_await_timeout(None)
    }

    /// Skips the next message to be received from the channel. If the skip succeeds than the number
    /// of receivable messages will drop by one. Calling this function will either set up a skip
    /// cursor in the channel or move an existing skip cursor. To receive skipped messages the
    /// user will need to first call [`axiom::secc::SeccReceiver::reset_skip`] prior to calling
    /// [`axiom::secc::SeccReceiver::receive`] in order to clear the skip cursor.
    pub fn skip(&self) -> Result<(), SeccErrors<T>> {
        unsafe {
            // Retrieve receive pointers and the encoded indexes inside them.
            let (ref mutex, _) = &*self.core.receive_ptrs;
            let mut receive_ptrs = mutex.lock().unwrap();

            let read_ptr = if receive_ptrs.cursor == NIL_NODE {
                (*self.core.node_ptrs.get())[receive_ptrs.queue_head]
            } else {
                (*self.core.node_ptrs.get())[receive_ptrs.cursor]
            };
            let next_read_pos = (*read_ptr).next.load(Ordering::SeqCst);

            // If there is a single node in the queue then there are no messages in the channel
            // and therefore nothing to skip so we just return an empty error.
            if NIL_NODE == next_read_pos {
                return Err(SeccErrors::Empty);
            }
            if receive_ptrs.cursor == NIL_NODE {
                // No current cursor; we need to establish one.
                receive_ptrs.skipped = receive_ptrs.queue_head;
                receive_ptrs.cursor = next_read_pos;
            } else {
                // There is a cursor already so make sure we increment cursor and precursor.
                receive_ptrs.skipped = receive_ptrs.cursor;
                receive_ptrs.cursor = next_read_pos;
            }
            self.core.receivable.fetch_sub(1, Ordering::SeqCst);
            Ok(())
        }
    }

    /// Cancels skipping messages in the channel and resets the pointers of the channel back to
    /// the head allowing previously skipped messages to be received. Note that calling this
    /// method on a channel with no skip cursor is essentially a no-op.
    pub fn reset_skip(&self) -> Result<(), SeccErrors<T>> {
        // Retrieve receive pointers and the encoded indexes inside them.
        let (ref mutex, ref condvar) = &*self.core.receive_ptrs;
        let mut receive_ptrs = mutex.lock().unwrap();

        if receive_ptrs.cursor != NIL_NODE {
            unsafe {
                // We start from queue head and count to the cursor to get the number of now
                // receivable messages in the channel.
                let mut count: usize = 1; // minimum number of skipped nodes
                let mut next_ptr = (*(*self.core.node_ptrs.get())[receive_ptrs.queue_head])
                    .next
                    .load(Ordering::SeqCst);
                while next_ptr != receive_ptrs.cursor {
                    count += 1;
                    next_ptr = (*(*self.core.node_ptrs.get())[next_ptr])
                        .next
                        .load(Ordering::SeqCst);
                }
                self.core.receivable.fetch_add(count, Ordering::SeqCst);
                receive_ptrs.cursor = NIL_NODE;
                receive_ptrs.skipped = NIL_NODE;
            }
        }
        // Notify anyone waiting on receivable messages to be available.
        condvar.notify_all();
        Ok(())
    }
}

impl<T: Sync + Send> SeccCoreOps<T> for SeccReceiver<T> {
    fn core(&self) -> &SeccCore<T> {
        &self.core
    }
}

unsafe impl<T: Send + Sync> Send for SeccReceiver<T> {}

unsafe impl<T: Send + Sync> Sync for SeccReceiver<T> {}

/// Creates the sender and receiver sides of this channel and returns them separately as
/// a tuple.
pub fn create<T: Sync + Send>(capacity: u16) -> (SeccSender<T>, SeccReceiver<T>) {
    if capacity < 1 {
        panic!("capacity cannot be smaller than 1");
    }

    // We add two to the allocated capacity to account for the mandatory two placeholder nodes
    // which guarantees that both queue and pool are never empty.
    let alloc_capacity = (capacity + 2) as usize;
    let mut nodes = Vec::<SeccNode<T>>::with_capacity(alloc_capacity);
    let mut node_ptrs = Vec::<*mut SeccNode<T>>::with_capacity(alloc_capacity);

    // The queue just gets one initial node with and the queue_tail is the same as the queue_head.
    nodes.push(SeccNode::<T>::new());
    node_ptrs.push(nodes.last_mut().unwrap() as *mut SeccNode<T>);
    let queue_head = nodes.len() - 1;
    let queue_tail = queue_head;

    // Allocate the tail in the pool of nodes that will be added to in order to form
    // the pool. Note that although this is expensive, it only has to be done once.
    nodes.push(SeccNode::<T>::new());
    node_ptrs.push(nodes.last_mut().unwrap() as *mut SeccNode<T>);
    let mut pool_head = nodes.len() - 1;
    let pool_tail = pool_head;

    // Allocate the rest of the pool setting the next pointers of each node to the previous node.
    for _ in 0..capacity {
        nodes.push(SeccNode::<T>::with_next(pool_head));
        node_ptrs.push(nodes.last_mut().unwrap() as *mut SeccNode<T>);
        pool_head = nodes.len() - 1;
    }

    // Materialize the starting indexes for both send and receive.
    let send_ptrs = SeccSendPtrs {
        queue_tail,
        pool_head,
    };

    let receive_ptrs = SeccReceivePtrs {
        queue_head,
        pool_tail,
        skipped: NIL_NODE,
        cursor: NIL_NODE,
    };

    // Create the channel structures
    let core = Arc::new(SeccCore {
        capacity: capacity as usize,
        _nodes: nodes.into_boxed_slice(),
        node_ptrs: UnsafeCell::new(node_ptrs),
        send_ptrs: Arc::new((Mutex::new(send_ptrs), Condvar::new())),
        receive_ptrs: Arc::new((Mutex::new(receive_ptrs), Condvar::new())),
        awaited_messages: AtomicUsize::new(0),
        awaited_capacity: AtomicUsize::new(0),
        pending: AtomicUsize::new(0),
        receivable: AtomicUsize::new(0),
        sent: AtomicUsize::new(0),
        received: AtomicUsize::new(0),
    });

    // Return the resulting sender and receiver as a tuple
    let sender = SeccSender { core: core.clone() };
    let receiver = SeccReceiver { core };

    (sender, receiver)
}

/// Creates the sender and receiver sides of the channel for multiple producers and
/// multiple consumers by returning sender and receiver each wrapped in [`Arc`] instances.
pub fn create_with_arcs<T: Sync + Send>(
    capacity: u16,
) -> (Arc<SeccSender<T>>, Arc<SeccReceiver<T>>) {
    let (sender, receiver) = create(capacity);
    (Arc::new(sender), Arc::new(receiver))
}

// --------------------- Test Cases ---------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::*;
    use log::info;
    use std::sync::MutexGuard;
    use std::thread;
    use std::time::Duration;

    /// A macro to assert that pointers point to the right nodes.
    macro_rules! assert_pointer_nodes {
        (
            $sender:expr,
            $receiver:expr,
            $queue_head:expr,
            $queue_tail:expr,
            $pool_head:expr,
            $pool_tail:expr,
            $skipped:expr,
            $cursor:expr
        ) => {{
            let (ref mutex, _) = &*$sender.core.send_ptrs;
            let send_ptrs = mutex.lock().unwrap();
            let (ref mutex, _) = &*$receiver.core.receive_ptrs;
            let receive_ptrs = mutex.lock().unwrap();

            assert_eq!(
                $queue_head, receive_ptrs.queue_head,
                " <== queue_head mismatch\n"
            );
            assert_eq!(
                $queue_tail, send_ptrs.queue_tail,
                "<== queue_tail mismatch\n"
            );
            assert_eq!($pool_head, send_ptrs.pool_head, "<== pool_head mismatch\n");
            assert_eq!(
                $pool_tail, receive_ptrs.pool_tail,
                " <== pool_tail mismatch\n"
            );
            assert_eq!($skipped, receive_ptrs.skipped, " <== skipped mismatch\n");
            assert_eq!($cursor, receive_ptrs.cursor, " <== pool_tail mismatch\n");
        }};
    }

    /// Asserts that the given node in the queue has the expected next pointer.
    macro_rules! assert_node_next {
        ($pointers:expr, $node:expr, $next:expr) => {
            unsafe { assert_eq!((*$pointers[$node]).next.load(Ordering::Relaxed), $next) }
        };
    }

    /// Asserts that the given node in the queue has the expected next pointing to `NIL_NODE`.
    macro_rules! assert_node_next_nil {
        ($pointers:expr, $node:expr) => {
            unsafe { assert_eq!((*$pointers[$node]).next.load(Ordering::Relaxed), NIL_NODE) }
        };
    }

    /// Creates a debug string for diagnosing problems with the send side of the channel.
    fn debug_send<T: Send + Sync>(
        core: Arc<SeccCore<T>>,
        send_ptrs: MutexGuard<SeccSendPtrs>,
    ) -> String {
        unsafe {
            let mut pool = Vec::with_capacity(core.capacity);
            pool.push(send_ptrs.pool_head);
            let mut next_ptr = (*(*core.node_ptrs.get())[send_ptrs.pool_head])
                .next
                .load(Ordering::SeqCst);
            let mut count = 1;
            while next_ptr != NIL_NODE {
                count += 1;
                pool.push(next_ptr);
                next_ptr = (*(*core.node_ptrs.get())[next_ptr])
                    .next
                    .load(Ordering::SeqCst);
            }

            format!(
                "send_ptrs: {:?}, pool_size: {}, pool: {:?}",
                send_ptrs, count, pool
            )
        }
    }

    /// Creates a debug string for diagnosing problems with the receive side of the channel.
    fn debug_receive<T: Send + Sync>(
        core: Arc<SeccCore<T>>,
        receive_ptrs: MutexGuard<SeccReceivePtrs>,
    ) -> String {
        unsafe {
            let mut queue = Vec::with_capacity(core.capacity);
            let mut next_ptr = (*(*core.node_ptrs.get())[receive_ptrs.queue_head])
                .next
                .load(Ordering::SeqCst);
            queue.push(receive_ptrs.queue_head);
            let mut count = 1;
            while next_ptr != NIL_NODE {
                count += 1;
                queue.push(next_ptr);
                next_ptr = (*(*core.node_ptrs.get())[next_ptr])
                    .next
                    .load(Ordering::SeqCst);
            }

            format!(
                "receive_ptrs: {:?}, queue_size: {}, queue: {:?}",
                receive_ptrs, count, queue
            )
        }
    }

    /// Creates a debug string for debugging channel problems.
    pub fn debug_channel<T: Send + Sync>(prefix: &str, core: Arc<SeccCore<T>>) {
        let r = core.receivable.load(Ordering::Relaxed);
        let (ref mutex, _) = &*core.receive_ptrs;
        let receive_ptrs = mutex.lock().unwrap();
        let (ref mutex, _) = &*core.send_ptrs;
        let send_ptrs = mutex.lock().unwrap();
        println!(
            "{} Receivable: {}, {}, {}",
            prefix,
            r,
            debug_receive(core.clone(), receive_ptrs),
            debug_send(core.clone(), send_ptrs)
        );
    }

    #[derive(Debug, Eq, PartialEq)]
    enum Items {
        A,
        B,
        C,
        D,
        E,
        F,
    }

    #[test]
    fn test_send_and_receive() {
        init_test_log();

        // This test checks the basic functionality of sending and receiving messages from the
        // channel in a single thread. This is used to verify basic functionality.
        let channel = create::<Items>(5);
        let (sender, receiver) = channel;

        // fetch the pointers for easy checking of the nodes.
        let pointers = unsafe { &*sender.core.node_ptrs.get() };

        assert_eq!(7, pointers.len());
        assert_eq!(5, sender.core.capacity);
        assert_eq!(5, sender.capacity());
        assert_eq!(5, receiver.capacity());

        // Check the initial structure.
        assert_eq!(0, sender.pending());
        assert_eq!(0, sender.receivable());
        assert_eq!(0, sender.sent());
        assert_eq!(0, sender.received());
        assert_node_next_nil!(pointers, 0);
        assert_node_next!(pointers, 6, 5);
        assert_node_next!(pointers, 5, 4);
        assert_node_next!(pointers, 4, 3);
        assert_node_next!(pointers, 3, 2);
        assert_node_next!(pointers, 2, 1);
        assert_node_next_nil!(pointers, 1);
        assert_pointer_nodes!(sender, receiver, 0, 0, 6, 1, NIL_NODE, NIL_NODE);

        // Check that sending a message to the channel removes pool head and appends to queue
        // tail and changes nothing else in the node structure.
        assert_eq!(Ok(()), sender.send(Items::A));
        assert_eq!(1, sender.pending());
        assert_eq!(1, sender.receivable());
        assert_eq!(1, sender.sent());
        assert_eq!(0, sender.received());
        assert_node_next!(pointers, 0, 6);
        assert_node_next_nil!(pointers, 6);
        assert_node_next!(pointers, 5, 4);
        assert_node_next!(pointers, 4, 3);
        assert_node_next!(pointers, 3, 2);
        assert_node_next!(pointers, 2, 1);
        assert_node_next_nil!(pointers, 1);
        assert_pointer_nodes!(sender, receiver, 0, 6, 5, 1, NIL_NODE, NIL_NODE);

        assert_eq!(Ok(()), sender.send(Items::B));
        assert_eq!(2, sender.pending());
        assert_eq!(2, sender.receivable());
        assert_eq!(2, sender.sent());
        assert_eq!(0, sender.received());
        assert_node_next!(pointers, 0, 6);
        assert_node_next!(pointers, 6, 5);
        assert_node_next_nil!(pointers, 5);
        assert_node_next!(pointers, 4, 3);
        assert_node_next!(pointers, 3, 2);
        assert_node_next!(pointers, 2, 1);
        assert_node_next_nil!(pointers, 1);
        assert_pointer_nodes!(sender, receiver, 0, 5, 4, 1, NIL_NODE, NIL_NODE);

        assert_eq!(Ok(()), sender.send(Items::C));
        assert_eq!(3, sender.pending());
        assert_eq!(3, sender.receivable());
        assert_eq!(3, sender.sent());
        assert_eq!(0, sender.received());
        assert_node_next!(pointers, 0, 6);
        assert_node_next!(pointers, 6, 5);
        assert_node_next!(pointers, 5, 4);
        assert_node_next_nil!(pointers, 4);
        assert_node_next!(pointers, 3, 2);
        assert_node_next!(pointers, 2, 1);
        assert_node_next_nil!(pointers, 1);
        assert_pointer_nodes!(sender, receiver, 0, 4, 3, 1, NIL_NODE, NIL_NODE);

        assert_eq!(Ok(()), sender.send(Items::D));
        assert_eq!(4, sender.pending());
        assert_eq!(4, sender.receivable());
        assert_eq!(4, sender.sent());
        assert_eq!(0, sender.received());
        assert_node_next!(pointers, 0, 6);
        assert_node_next!(pointers, 6, 5);
        assert_node_next!(pointers, 5, 4);
        assert_node_next!(pointers, 4, 3);
        assert_node_next_nil!(pointers, 3);
        assert_node_next!(pointers, 2, 1);
        assert_node_next_nil!(pointers, 1);
        assert_pointer_nodes!(sender, receiver, 0, 3, 2, 1, NIL_NODE, NIL_NODE);

        assert_eq!(Ok(()), sender.send(Items::E));
        assert_eq!(5, sender.pending());
        assert_eq!(5, sender.receivable());
        assert_eq!(5, sender.sent());
        assert_eq!(0, sender.received());
        assert_node_next!(pointers, 0, 6);
        assert_node_next!(pointers, 6, 5);
        assert_node_next!(pointers, 5, 4);
        assert_node_next!(pointers, 4, 3);
        assert_node_next!(pointers, 3, 2);
        assert_node_next_nil!(pointers, 2);
        assert_node_next_nil!(pointers, 1);
        assert_pointer_nodes!(sender, receiver, 0, 2, 1, 1, NIL_NODE, NIL_NODE);

        // Validate that we cannot fill the channel past its capacity.
        assert_eq!(Err(SeccErrors::Full(Items::F)), sender.send(Items::F));
        assert_eq!(5, sender.pending());
        assert_eq!(5, sender.receivable());
        assert_eq!(5, sender.sent());
        assert_eq!(0, sender.received());

        assert_eq!(Err(SeccErrors::Full(Items::F)), sender.send(Items::F));
        assert_eq!(5, sender.pending());
        assert_eq!(5, sender.receivable());
        assert_eq!(5, sender.sent());
        assert_eq!(0, sender.received());

        assert_eq!(Err(SeccErrors::Full(Items::F)), sender.send(Items::F));
        assert_eq!(5, sender.pending());
        assert_eq!(5, sender.receivable());
        assert_eq!(5, sender.sent());
        assert_eq!(0, sender.received());

        // Validate that receiving from the channel performs the proper pointer operations.
        assert_node_next!(pointers, 0, 6);
        assert_node_next!(pointers, 6, 5);
        assert_node_next!(pointers, 5, 4);
        assert_node_next!(pointers, 4, 3);
        assert_node_next!(pointers, 3, 2);
        assert_node_next_nil!(pointers, 2);
        assert_node_next_nil!(pointers, 1);
        assert_pointer_nodes!(sender, receiver, 0, 2, 1, 1, NIL_NODE, NIL_NODE);

        assert_eq!(Ok(Items::A), receiver.receive());
        assert_eq!(4, receiver.pending());
        assert_eq!(4, receiver.receivable());
        assert_eq!(5, receiver.sent());
        assert_eq!(1, receiver.received());
        assert_node_next!(pointers, 6, 5);
        assert_node_next!(pointers, 5, 4);
        assert_node_next!(pointers, 4, 3);
        assert_node_next!(pointers, 3, 2);
        assert_node_next_nil!(pointers, 2);
        assert_node_next!(pointers, 1, 0);
        assert_node_next_nil!(pointers, 0);
        assert_pointer_nodes!(sender, receiver, 6, 2, 1, 0, NIL_NODE, NIL_NODE);

        assert_eq!(Ok(Items::B), receiver.receive());
        assert_eq!(3, receiver.pending());
        assert_eq!(3, receiver.receivable());
        assert_eq!(5, receiver.sent());
        assert_eq!(2, receiver.received());
        assert_node_next!(pointers, 5, 4);
        assert_node_next!(pointers, 4, 3);
        assert_node_next!(pointers, 3, 2);
        assert_node_next_nil!(pointers, 2);
        assert_node_next!(pointers, 1, 0);
        assert_node_next!(pointers, 0, 6);
        assert_node_next_nil!(pointers, 6);
        assert_pointer_nodes!(sender, receiver, 5, 2, 1, 6, NIL_NODE, NIL_NODE);

        assert_eq!(Ok(Items::C), receiver.receive());
        assert_eq!(2, receiver.pending());
        assert_eq!(2, receiver.receivable());
        assert_eq!(5, receiver.sent());
        assert_eq!(3, receiver.received());
        assert_node_next!(pointers, 4, 3);
        assert_node_next!(pointers, 3, 2);
        assert_node_next_nil!(pointers, 2);
        assert_node_next!(pointers, 1, 0);
        assert_node_next!(pointers, 0, 6);
        assert_node_next!(pointers, 6, 5);
        assert_node_next_nil!(pointers, 5);
        assert_pointer_nodes!(sender, receiver, 4, 2, 1, 5, NIL_NODE, NIL_NODE);

        assert_eq!(Ok(Items::D), receiver.receive());
        assert_eq!(1, receiver.pending());
        assert_eq!(1, receiver.receivable());
        assert_eq!(5, receiver.sent());
        assert_eq!(4, receiver.received());
        assert_node_next!(pointers, 3, 2);
        assert_node_next_nil!(pointers, 2);
        assert_node_next!(pointers, 1, 0);
        assert_node_next!(pointers, 0, 6);
        assert_node_next!(pointers, 6, 5);
        assert_node_next!(pointers, 5, 4);
        assert_node_next_nil!(pointers, 4);
        assert_pointer_nodes!(sender, receiver, 3, 2, 1, 4, NIL_NODE, NIL_NODE);

        assert_eq!(Ok(Items::E), receiver.receive());
        assert_eq!(0, receiver.pending());
        assert_eq!(0, receiver.receivable());
        assert_eq!(5, receiver.sent());
        assert_eq!(5, receiver.received());
        assert_node_next_nil!(pointers, 2);
        assert_node_next!(pointers, 1, 0);
        assert_node_next!(pointers, 0, 6);
        assert_node_next!(pointers, 6, 5);
        assert_node_next!(pointers, 5, 4);
        assert_node_next!(pointers, 4, 3);
        assert_node_next_nil!(pointers, 3);
        assert_pointer_nodes!(sender, receiver, 2, 2, 1, 3, NIL_NODE, NIL_NODE);

        // Validate that we cannot continue to receive from an empty channel and attempts
        // don't mangle the pointers.
        assert_eq!(Err(SeccErrors::Empty), receiver.receive());
        assert_eq!(0, receiver.pending());
        assert_eq!(0, receiver.receivable());
        assert_eq!(5, receiver.sent());
        assert_eq!(5, receiver.received());
        assert_node_next_nil!(pointers, 2);
        assert_node_next!(pointers, 1, 0);
        assert_node_next!(pointers, 0, 6);
        assert_node_next!(pointers, 6, 5);
        assert_node_next!(pointers, 5, 4);
        assert_node_next!(pointers, 4, 3);
        assert_node_next_nil!(pointers, 3);
        assert_pointer_nodes!(sender, receiver, 2, 2, 1, 3, NIL_NODE, NIL_NODE);

        assert_eq!(Err(SeccErrors::Empty), receiver.receive());
        assert_eq!(0, receiver.pending());
        assert_eq!(0, receiver.receivable());
        assert_eq!(5, receiver.sent());
        assert_eq!(5, receiver.received());
        assert_node_next_nil!(pointers, 2);
        assert_node_next!(pointers, 1, 0);
        assert_node_next!(pointers, 0, 6);
        assert_node_next!(pointers, 6, 5);
        assert_node_next!(pointers, 5, 4);
        assert_node_next!(pointers, 4, 3);
        assert_node_next_nil!(pointers, 3);
        assert_pointer_nodes!(sender, receiver, 2, 2, 1, 3, NIL_NODE, NIL_NODE);

        // Validate that after the channel is empty it can still be sent to and received from.
        assert_eq!(Ok(()), sender.send(Items::F));
        assert_eq!(1, receiver.pending());
        assert_eq!(1, receiver.receivable());
        assert_eq!(6, receiver.sent());
        assert_eq!(5, receiver.received());
        assert_node_next!(pointers, 2, 1);
        assert_node_next_nil!(pointers, 1);
        assert_node_next!(pointers, 0, 6);
        assert_node_next!(pointers, 6, 5);
        assert_node_next!(pointers, 5, 4);
        assert_node_next!(pointers, 4, 3);
        assert_node_next_nil!(pointers, 3);
        assert_pointer_nodes!(sender, receiver, 2, 1, 0, 3, NIL_NODE, NIL_NODE);

        assert_eq!(Ok(Items::F), receiver.receive());
        assert_eq!(0, receiver.pending());
        assert_eq!(0, receiver.receivable());
        assert_eq!(6, receiver.sent());
        assert_eq!(6, receiver.received());
        assert_node_next_nil!(pointers, 1);
        assert_node_next!(pointers, 0, 6);
        assert_node_next!(pointers, 6, 5);
        assert_node_next!(pointers, 5, 4);
        assert_node_next!(pointers, 4, 3);
        assert_node_next!(pointers, 3, 2);
        assert_node_next_nil!(pointers, 2);
        assert_pointer_nodes!(sender, receiver, 1, 1, 0, 2, NIL_NODE, NIL_NODE);

        // This debug line here so we don't get unused warnings as it is not necessary to the
        // test. :) This function can be called by developers writing tests to debug the channel.
        debug_channel("send/receive done: ", sender.core.clone());

        // FIXME test Skipping
    }

    #[test]
    fn test_single_producer_single_receiver() {
        init_test_log();

        // Tests that the channel can send and receive messages with separate senders and
        // receivers on different threads.
        let message_count = 200;
        let capacity = 32;
        let (sender, receiver) = create_with_arcs::<u32>(capacity);
        let timeout = Some(Duration::from_millis(20));

        let rx = thread::spawn(move || {
            let mut count = 0;
            while count < message_count {
                match receiver.receive_await_timeout(timeout) {
                    Ok(_v) => count += 1,
                    _ => (),
                };
            }
        });

        let tx = thread::spawn(move || {
            for i in 0..message_count {
                sender.send_await_timeout(i, timeout).unwrap();
                thread::sleep(Duration::from_millis(1));
            }
        });

        tx.join().unwrap();
        rx.join().unwrap();
    }

    #[test]
    fn test_receive_before_send() {
        init_test_log();

        // Test that if a user attempts to receive before a message is sent, he will be forced
        // to wait for the message.
        let (sender, receiver) = create_with_arcs::<u32>(5);
        let timeout = Some(Duration::from_millis(20));
        let receiver2 = receiver.clone();
        let rx = thread::spawn(move || {
            match receiver2.receive_await_timeout(timeout) {
                Ok(_) => assert!(true),
                e => assert!(false, "Error {:?} when receive.", e),
            };
        });
        let tx = thread::spawn(move || {
            match sender.send_await_timeout(1 as u32, timeout) {
                Ok(_) => assert!(true),
                e => assert!(false, "Error {:?} when receive.", e),
            };
        });

        tx.join().unwrap();
        rx.join().unwrap();

        assert_eq!(1, receiver.sent());
        assert_eq!(1, receiver.received());
        assert_eq!(0, receiver.pending());
        assert_eq!(0, receiver.receivable());
    }

    #[test]
    fn test_receive_concurrent_send() {
        init_test_log();

        // Tests that triggering send and receive as close to at the same time as possible does
        // not cause any race conditions.  order to try to test potential races.
        let (sender, receiver) = create_with_arcs::<u32>(5);
        let timeout = Some(Duration::from_millis(20));
        let receiver2 = receiver.clone();
        let pair = Arc::new((Mutex::new((false, false)), Condvar::new()));
        let rx_pair = pair.clone();
        let tx_pair = pair.clone();

        let rx = thread::spawn(move || {
            let mut guard = rx_pair.0.lock().unwrap();
            guard.0 = true;
            let c_guard = rx_pair.1.wait(guard).unwrap();
            drop(c_guard);
            match receiver2.receive_await_timeout(timeout) {
                Ok(_) => assert!(true),
                e => assert!(false, "Error {:?} when receive.", e),
            };
        });
        let tx = thread::spawn(move || {
            let mut guard = tx_pair.0.lock().unwrap();
            guard.1 = true;
            let c_guard = tx_pair.1.wait(guard).unwrap();
            drop(c_guard);
            match sender.send_await_timeout(1 as u32, timeout) {
                Ok(_) => assert!(true),
                e => assert!(false, "Error {:?} when receive.", e),
            };
        });

        loop {
            let guard = pair.0.lock().unwrap();
            if guard.0 && guard.1 {
                break;
            }
        }

        let guard = pair.0.lock().unwrap();
        pair.1.notify_all();
        drop(guard);

        tx.join().unwrap();
        rx.join().unwrap();

        assert_eq!(1, receiver.sent());
        assert_eq!(1, receiver.received());
        assert_eq!(0, receiver.pending());
        assert_eq!(0, receiver.receivable());
    }

    #[test]
    fn test_multiple_producer_single_receiver() {
        init_test_log();

        // Tests that the channel can handle multiple producers with a single receiver and that
        // the senders will properly wait for capacity as well as receivers waiting for messages.
        // The channel size is intentionally small to force wait conditions.
        let message_count = 10000;
        let capacity = 10;
        let (sender, receiver) = create_with_arcs::<u32>(capacity);
        let timeout = Some(Duration::from_millis(20));

        let debug_if_needed = |core: Arc<SeccCore<u32>>| {
            if core.receivable.load(Ordering::Relaxed) > core.capacity {
                debug_channel(thread::current().name().unwrap(), core);
            }
        };

        let receiver1 = receiver.clone();
        let rx = thread::Builder::new()
            .name("R1".into())
            .spawn(move || {
                let mut count = 0;
                while count < message_count {
                    match receiver1.receive_await_timeout(timeout) {
                        Ok(_) => {
                            debug_if_needed(receiver1.core.clone());
                            count += 1;
                        }
                        _ => (),
                    };
                }
            })
            .unwrap();

        let sender1 = sender.clone();
        let tx = thread::Builder::new()
            .name("S1".into())
            .spawn(move || {
                for i in 0..(message_count / 3) {
                    match sender1.send_await_timeout(i, timeout) {
                        Ok(_c) => {
                            debug_if_needed(sender1.core.clone());
                            ()
                        }
                        Err(e) => assert!(false, "----> Error while sending: {}:{:?}", i, e),
                    }
                }
            })
            .unwrap();

        let sender2 = sender.clone();
        let tx2 = thread::Builder::new()
            .name("S2".into())
            .spawn(move || {
                for i in (message_count / 3)..((message_count / 3) * 2) {
                    match sender2.send_await_timeout(i, timeout) {
                        Ok(_c) => {
                            debug_if_needed(sender2.core.clone());
                            ()
                        }
                        Err(e) => assert!(false, "----> Error while sending: {}:{:?}", i, e),
                    }
                }
            })
            .unwrap();

        let sender3 = sender.clone();
        let tx3 = thread::Builder::new()
            .name("S3".into())
            .spawn(move || {
                for i in ((message_count / 3) * 2)..(message_count) {
                    match sender3.send_await_timeout(i, timeout) {
                        Ok(_c) => {
                            debug_if_needed(sender3.core.clone());
                            ()
                        }
                        Err(e) => assert!(false, "----> Error while sending: {}:{:?}", i, e),
                    }
                }
            })
            .unwrap();

        tx.join().unwrap();
        tx2.join().unwrap();
        tx3.join().unwrap();
        rx.join().unwrap();

        info!(
            "All messages complete: awaited_messages: {}, awaited_capacity: {}",
            receiver.awaited_messages(),
            sender.awaited_capacity()
        );
    }
}
