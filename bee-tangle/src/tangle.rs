// Copyright 2020-2021 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

use crate::{vertex::Vertex, MessageRef};

use bee_message::{Message, MessageId};

use async_trait::async_trait;
use hashbrown::{hash_map::DefaultHashBuilder, HashMap};
use log::info;
use lru::LruCache;
use tokio::sync::{Mutex, RwLock as TRwLock, RwLockWriteGuard as TRwLockWriteGuard};

use std::{
    fmt::Debug,
    marker::PhantomData,
    ops::{Deref, DerefMut},
    sync::atomic::{AtomicUsize, Ordering},
};

pub const DEFAULT_CACHE_LEN: usize = 100_000;
const CACHE_THRESHOLD_FACTOR: f64 = 0.1;

/// A trait used to provide hooks for a tangle. The tangle acts as an in-memory cache and will use hooks to extend its
/// effective volume. When an entry doesn't exist in the tangle cache and needs fetching, or when an entry gets
/// inserted, the tangle will call out to the hooks in order to fulfil these actions.
#[async_trait]
pub trait Hooks<T> {
    /// An error generated by these hooks.
    type Error: Debug;

    /// Fetch a message from some external storage medium.
    async fn get(&self, message_id: &MessageId) -> Result<Option<(Message, T)>, Self::Error>;
    /// Insert a message into some external storage medium.
    async fn insert(&self, message_id: MessageId, msg: Message, metadata: T) -> Result<(), Self::Error>;
    /// Fetch the approvers list for a given message.
    async fn fetch_approvers(&self, message_id: &MessageId) -> Result<Option<Vec<MessageId>>, Self::Error>;
    /// Insert a new approver for a given message.
    async fn insert_approver(&self, message_id: MessageId, approver: MessageId) -> Result<(), Self::Error>;
    /// Update the approvers list for a given message.
    async fn update_approvers(&self, message_id: MessageId, approvers: &[MessageId]) -> Result<(), Self::Error>;
}

/// Phoney default hooks that do nothing.
pub struct NullHooks<T>(PhantomData<T>);

impl<T> Default for NullHooks<T> {
    fn default() -> Self {
        Self(PhantomData)
    }
}

#[async_trait]
impl<T: Send + Sync> Hooks<T> for NullHooks<T> {
    type Error = ();

    async fn get(&self, _message_id: &MessageId) -> Result<Option<(Message, T)>, Self::Error> {
        Ok(None)
    }

    async fn insert(&self, _message_id: MessageId, _msg: Message, _metadata: T) -> Result<(), Self::Error> {
        Ok(())
    }

    async fn fetch_approvers(&self, _message_id: &MessageId) -> Result<Option<Vec<MessageId>>, Self::Error> {
        Ok(None)
    }

    async fn insert_approver(&self, _message_id: MessageId, _approver: MessageId) -> Result<(), Self::Error> {
        Ok(())
    }

    async fn update_approvers(&self, _message_id: MessageId, _approvers: &[MessageId]) -> Result<(), Self::Error> {
        Ok(())
    }
}

/// A foundational, thread-safe graph datastructure to represent the IOTA Tangle.
pub struct Tangle<T, H = NullHooks<T>>
where
    T: Clone,
{
    vertices: TRwLock<HashMap<MessageId, Vertex<T>>>,

    cache_queue: Mutex<LruCache<MessageId, (), DefaultHashBuilder>>,
    max_len: AtomicUsize,

    hooks: H,
}

impl<T, H: Hooks<T>> Default for Tangle<T, H>
where
    T: Clone,
    H: Default,
{
    fn default() -> Self {
        Self::new(H::default())
    }
}

impl<T, H: Hooks<T>> Tangle<T, H>
where
    T: Clone,
{
    /// Creates a new Tangle.
    pub fn new(hooks: H) -> Self {
        Self {
            vertices: TRwLock::new(HashMap::new()),

            cache_queue: Mutex::new(LruCache::unbounded_with_hasher(DefaultHashBuilder::default())),
            max_len: AtomicUsize::new(DEFAULT_CACHE_LEN),

            hooks,
        }
    }

    /// Create a new tangle with the given capacity.
    pub fn with_capacity(self, cap: usize) -> Self {
        Self {
            cache_queue: Mutex::new(LruCache::with_hasher(cap + 1, DefaultHashBuilder::default())),
            ..self
        }
    }

    /// Change the maximum number of entries to store in the cache.
    pub fn resize(&self, len: usize) {
        self.max_len.store(len, Ordering::Relaxed);
    }

    /// Return a reference to the storage hooks used by this tangle.
    pub fn hooks(&self) -> &H {
        &self.hooks
    }

    async fn insert_inner(
        &self,
        message_id: MessageId,
        message: Message,
        metadata: T,
        prevent_eviction: bool,
    ) -> Option<MessageRef> {
        let mut vertices = self.vertices.write().await;
        let vertex = vertices.entry(message_id).or_insert_with(Vertex::empty);

        if prevent_eviction {
            vertex.prevent_eviction();
        }

        let msg = if vertex.message().is_some() {
            None
        } else {
            let parents = message.parents().clone();

            vertex.insert_message_and_metadata(message, metadata);
            let msg = vertex.message().cloned();

            let mut cache_queue = self.cache_queue.lock().await;

            // Insert children for parents
            for &parent in parents.iter() {
                let children = vertices.entry(parent).or_insert_with(Vertex::empty);
                children.add_child(message_id);

                // Insert cache queue entry to track eviction priority
                cache_queue.put(parent, ());
            }

            // Insert cache queue entry to track eviction priority
            cache_queue.put(message_id, ());

            msg
        };

        drop(vertices);

        self.perform_eviction().await;

        msg
    }

    /// Inserts a message, and returns a thread-safe reference to it in case it didn't already exist.
    pub async fn insert(&self, message_id: MessageId, message: Message, metadata: T) -> Option<MessageRef> {
        let exists = self.pull_message(&message_id, true).await;

        let msg = self
            .insert_inner(message_id, message.clone(), metadata.clone(), !exists)
            .await;

        self.vertices
            .write()
            .await
            .get_mut(&message_id)
            .expect("Just-inserted message is missing")
            .allow_eviction();

        if msg.is_some() {
            // Write parents to DB
            for &parent in message.parents().iter() {
                self.hooks
                    .insert_approver(parent, message_id)
                    .await
                    .unwrap_or_else(|e| info!("Failed to update approvers for message {:?}", e));
            }

            // Insert into backend using hooks
            self.hooks
                .insert(message_id, message, metadata.clone())
                .await
                .unwrap_or_else(|e| info!("Failed to insert message {:?}", e));
        }

        msg
    }

    async fn get_inner(&self, message_id: &MessageId) -> Option<impl DerefMut<Target = Vertex<T>> + '_> {
        let res = TRwLockWriteGuard::try_map(self.vertices.write().await, |m| m.get_mut(message_id)).ok();

        if res.is_some() {
            // Update message_id priority
            self.cache_queue.lock().await.put(*message_id, ());
        }

        res
    }

    /// Get the data of a vertex associated with the given `message_id`.
    pub async fn get_with<R>(&self, message_id: &MessageId, f: impl FnOnce(&mut Vertex<T>) -> Option<R>) -> Option<R> {
        let exists = self.pull_message(message_id, true).await;

        self.get_inner(message_id).await.and_then(|mut v| {
            if exists {
                v.allow_eviction();
            }
            f(&mut v)
        })
    }

    /// Get the data of a vertex associated with the given `message_id`.
    pub async fn get(&self, message_id: &MessageId) -> Option<MessageRef> {
        self.get_with(message_id, |v| v.message().cloned()).await
    }

    async fn contains_inner(&self, message_id: &MessageId) -> bool {
        self.vertices
            .read()
            .await
            .get(message_id)
            .map_or(false, |v| v.message().is_some())
    }

    /// Returns whether the message is stored in the Tangle.
    pub async fn contains(&self, message_id: &MessageId) -> bool {
        self.contains_inner(message_id).await || self.pull_message(message_id, false).await
    }

    /// Get the metadata of a vertex associated with the given `message_id`.
    pub async fn get_metadata(&self, message_id: &MessageId) -> Option<T> {
        self.get_with(message_id, |v| v.metadata().cloned()).await
    }

    /// Get the metadata of a vertex associated with the given `message_id`, if it's in the cache.
    pub async fn get_metadata_maybe(&self, message_id: &MessageId) -> Option<T> {
        self.get_inner(message_id).await.and_then(|v| v.metadata().cloned())
    }

    /// Get the metadata of a vertex associated with the given `message_id`.
    pub async fn get_vertex(&self, message_id: &MessageId) -> Option<impl Deref<Target = Vertex<T>> + '_> {
        let exists = self.pull_message(message_id, true).await;

        self.get_inner(message_id).await.map(|mut v| {
            if exists {
                v.allow_eviction();
            }
            v
        })
    }

    /// Updates the metadata of a particular vertex.
    pub async fn set_metadata(&self, message_id: &MessageId, metadata: T) {
        self.update_metadata(message_id, |m| *m = metadata).await;
    }

    /// Updates the metadata of a vertex.
    pub async fn update_metadata<R, Update>(&self, message_id: &MessageId, update: Update) -> Option<R>
    where
        Update: FnOnce(&mut T) -> R,
    {
        let exists = self.pull_message(message_id, true).await;
        let mut vertices = self.vertices.write().await;
        if let Some(vertex) = vertices.get_mut(message_id) {
            // If we previously blocked eviction, allow it again
            if exists {
                vertex.allow_eviction();
            }

            let r = vertex.metadata_mut().map(|m| update(m));

            if let Some((msg, meta)) = vertex.message_and_metadata() {
                let (msg, meta) = ((&**msg).clone(), meta.clone());

                // Insert cache queue entry to track eviction priority
                self.cache_queue.lock().await.put(*message_id, ());

                drop(vertices);

                self.hooks
                    .insert(*message_id, msg, meta)
                    .await
                    .unwrap_or_else(|e| info!("Failed to update metadata for message {:?}", e));
            }

            r
        } else {
            None
        }
    }

    /// Returns the number of messages in the Tangle.
    pub async fn len(&self) -> usize {
        // Does not take GTL because this is effectively atomic
        self.vertices.read().await.len()
    }

    /// Checks if the tangle is empty.
    pub async fn is_empty(&self) -> bool {
        self.len().await == 0
    }

    async fn children_inner(&self, message_id: &MessageId) -> Option<impl Deref<Target = Vec<MessageId>> + '_> {
        struct Wrapper<'a> {
            children: Vec<MessageId>,
            phantom: PhantomData<&'a ()>,
        }

        impl<'a> Deref for Wrapper<'a> {
            type Target = Vec<MessageId>;

            fn deref(&self) -> &Self::Target {
                &self.children
            }
        }

        let vertices = self.vertices.read().await;
        let v = vertices
            .get(message_id)
            // Skip approver lists that are not exhaustive
            .filter(|v| v.children_exhaustive());

        let children = match v {
            Some(v) => {
                // Insert cache queue entry to track eviction priority
                self.cache_queue.lock().await.put(*message_id, ());
                let children = v.children().to_vec();
                drop(vertices);
                children
            }
            None => {
                // Insert cache queue entry to track eviction priority
                self.cache_queue.lock().await.put(*message_id, ());
                drop(vertices);
                let to_insert = match self.hooks.fetch_approvers(message_id).await {
                    Err(e) => {
                        info!("Failed to update approvers for message message {:?}", e);
                        Vec::new()
                    }
                    Ok(None) => Vec::new(),
                    Ok(Some(approvers)) => approvers,
                };

                let mut vertices = self.vertices.write().await;
                let v = vertices.entry(*message_id).or_insert_with(Vertex::empty);

                // We've just fetched approvers from the database, so we have all the information available to us now.
                // Therefore, the approvers list is exhaustive (i.e: it contains all knowledge we have).
                v.set_exhaustive();

                for child in to_insert {
                    v.add_child(child);
                }

                v.children().to_vec()
            }
        };

        Some(Wrapper {
            children,
            phantom: PhantomData,
        })
    }

    /// Returns the children of a vertex, if we know about them.
    pub async fn get_children(&self, message_id: &MessageId) -> Option<Vec<MessageId>> {
        // Effectively atomic
        self.children_inner(message_id).await.map(|approvers| approvers.clone())
    }

    /// Returns the number of children of a vertex.
    pub async fn num_children(&self, message_id: &MessageId) -> usize {
        // Effectively atomic
        self.children_inner(message_id)
            .await
            .map_or(0, |approvers| approvers.len())
    }

    #[cfg(test)]
    pub async fn clear(&mut self) {
        self.vertices.write().await.clear();
    }

    // Attempts to pull the message from the storage, returns true if successful.
    async fn pull_message(&self, message_id: &MessageId, prevent_eviction: bool) -> bool {
        let contains_now = if prevent_eviction {
            self.vertices.write().await.get_mut(message_id).map_or(false, |v| {
                if v.message().is_some() {
                    v.prevent_eviction();
                    true
                } else {
                    false
                }
            })
        } else {
            self.contains_inner(message_id).await
        };

        // If the tangle already contains the message, do no more work
        if contains_now {
            // Insert cache queue entry to track eviction priority
            self.cache_queue.lock().await.put(*message_id, ());

            true
        } else if let Ok(Some((msg, metadata))) = self.hooks.get(message_id).await {
            // Insert cache queue entry to track eviction priority
            self.cache_queue.lock().await.put(*message_id, ());

            self.insert_inner(*message_id, msg, metadata, prevent_eviction).await;

            true
        } else {
            false
        }
    }

    async fn perform_eviction(&self) {
        let max_len = self.max_len.load(Ordering::Relaxed);
        let len = self.vertices.read().await.len();
        if len > max_len {
            let mut vertices = self.vertices.write().await;
            let mut cache_queue = self.cache_queue.lock().await;
            while vertices.len() > ((1.0 - CACHE_THRESHOLD_FACTOR) * max_len as f64) as usize {
                let remove = cache_queue.pop_lru().map(|(id, _)| id);

                if let Some(message_id) = remove {
                    if let Some(v) = vertices.remove(&message_id) {
                        if !v.can_evict() {
                            // Reinsert it if we're not permitted to evict it yet (because something is using it)
                            vertices.insert(message_id, v);
                            cache_queue.put(message_id, ());
                        }
                    }
                } else {
                    break;
                }
            }
        }
    }
}
