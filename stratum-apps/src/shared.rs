use std::{
    hash::Hash,
    sync::{Arc, RwLock},
};

use dashmap::DashMap;
use std::sync::Mutex;

#[derive(Debug)]
pub struct Shared<T>(Arc<Mutex<T>>);

impl<T> Clone for Shared<T> {
    fn clone(&self) -> Self {
        Shared(Arc::clone(&self.0))
    }
}

impl<T> Shared<T> {
    pub fn new(v: T) -> Self {
        Shared(Arc::new(Mutex::new(v)))
    }

    pub fn with<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut T) -> R,
    {
        let mut lock = self.0.lock().unwrap();
        f(&mut *lock)
    }

    pub fn get(&self) -> T
    where
        T: Clone,
    {
        self.with(|v| v.clone())
    }

    pub fn set(&self, value: T) {
        self.with(|v| *v = value);
    }
}

#[derive(Debug)]
pub struct SharedRw<T>(Arc<RwLock<T>>);

impl<T> Clone for SharedRw<T> {
    fn clone(&self) -> Self {
        SharedRw(Arc::clone(&self.0))
    }
}

impl<T> SharedRw<T> {
    pub fn new(v: T) -> Self {
        SharedRw(Arc::new(RwLock::new(v)))
    }

    pub fn read<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&T) -> R,
    {
        let guard = self.0.read().unwrap();
        f(&*guard)
    }

    pub fn write<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut T) -> R,
    {
        let mut guard = self.0.write().unwrap();
        f(&mut *guard)
    }

    pub fn get(&self) -> T
    where
        T: Clone,
    {
        self.read(|v| v.clone())
    }

    pub fn set(&self, value: T) {
        self.write(|v| *v = value);
    }
}

pub struct SharedMap<K: Eq + Clone + Hash, V>(Arc<DashMap<K, V>>);

impl<K: Eq + Clone + Hash, V> Clone for SharedMap<K, V> {
    fn clone(&self) -> Self {
        SharedMap(Arc::clone(&self.0))
    }
}

impl<K: Eq + Hash + Clone, V> SharedMap<K, V> {
    pub fn new() -> Self {
        SharedMap(Arc::new(DashMap::new()))
    }

    pub fn with<F, R>(&self, key: &K, f: F) -> Option<R>
    where
        F: FnOnce(&V) -> R,
    {
        let guard = self.0.get(key)?;
        let result = f(guard.value());
        drop(guard);
        Some(result)
    }

    pub fn with_mut<F, R>(&self, key: &K, f: F) -> Option<R>
    where
        F: FnOnce(&mut V) -> R,
    {
        let mut guard = self.0.get_mut(key)?;
        let result = f(guard.value_mut());
        Some(result)
    }

    pub fn for_each<F, Ret>(&self, mut f: F)
    where
        F: FnMut(K, &V) -> Ret,
    {
        for entry in self.0.iter() {
            f(entry.key().clone(), entry.value());
        }
    }

    pub fn for_each_mut<F, Ret>(&self, mut f: F)
    where
        F: FnMut(K, &mut V) -> Ret,
    {
        for mut entry in self.0.iter_mut() {
            f(entry.key().clone(), entry.value_mut());
        }
    }

    pub fn try_for_each_mut<F, E>(&self, mut f: F) -> Result<(), E>
    where
        F: FnMut(K, &mut V) -> Result<(), E>,
    {
        for mut entry in self.0.iter_mut() {
            f(entry.key().clone(), entry.value_mut())?;
        }
        Ok(())
    }

    pub fn insert(&self, key: K, value: V) -> Option<V> {
        self.0.insert(key, value)
    }

    pub fn remove(&self, key: &K) -> Option<(K, V)> {
        self.0.remove(key)
    }

    pub fn contains_key(&self, key: &K) -> bool {
        self.0.contains_key(key)
    }

    pub fn retain<F>(&self, f: F)
    where
        F: FnMut(&K, &mut V) -> bool,
    {
        self.0.retain(f);
    }

    pub fn keys(&self) -> Vec<K>
    where
        K: Clone,
    {
        self.0.iter().map(|e| e.key().clone()).collect()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl<K: Eq + Hash + Clone, V> Default for SharedMap<K, V> {
    fn default() -> Self {
        Self::new()
    }
}
