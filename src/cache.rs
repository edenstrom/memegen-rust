//! Size-bounded caches: moka natively, and a small single-threaded LRU on
//! WebAssembly (moka reads `std::time::Instant`, which panics there).

use std::hash::Hash;
use std::sync::Arc;

pub type Weigher<K, V> = fn(&K, &V) -> u32;

#[cfg(not(target_arch = "wasm32"))]
pub struct Cache<K, V> {
    inner: moka::sync::Cache<K, V>,
}

#[cfg(not(target_arch = "wasm32"))]
impl<K, V> Cache<K, V>
where
    K: Hash + Eq + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
{
    /// A cache holding up to `capacity` units as measured by `weigher`.
    pub fn new(capacity: u64, weigher: Weigher<K, V>) -> Self {
        Self {
            inner: moka::sync::Cache::builder()
                .weigher(weigher)
                .max_capacity(capacity)
                .build(),
        }
    }

    pub fn contains(&self, key: &K) -> bool {
        self.inner.contains_key(key)
    }

    pub fn get(&self, key: &K) -> Option<V> {
        self.inner.get(key)
    }

    pub fn get_with(&self, key: K, init: impl FnOnce() -> V) -> V {
        self.inner.get_with(key, init)
    }

    pub fn try_get_with<E: Send + Sync + 'static>(
        &self,
        key: K,
        init: impl FnOnce() -> Result<V, E>,
    ) -> Result<V, Arc<E>> {
        self.inner.try_get_with(key, init)
    }

    pub fn invalidate_all(&self) {
        self.inner.invalidate_all();
    }
}

#[cfg(target_arch = "wasm32")]
pub use lru::Cache;

#[cfg(any(target_arch = "wasm32", test))]
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
mod lru {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use super::*;

    struct Entry<V> {
        value: V,
        weight: u64,
        used: u64,
    }

    struct State<K, V> {
        entries: HashMap<K, Entry<V>>,
        weight: u64,
        clock: u64,
    }

    /// Least-recently-used eviction by scanning; fine for the few hundred
    /// entries these caches hold.
    pub struct Cache<K, V> {
        capacity: u64,
        weigher: Weigher<K, V>,
        state: Mutex<State<K, V>>,
    }

    impl<K: Hash + Eq + Clone, V: Clone> Cache<K, V> {
        pub fn new(capacity: u64, weigher: Weigher<K, V>) -> Self {
            Self {
                capacity,
                weigher,
                state: Mutex::new(State {
                    entries: HashMap::new(),
                    weight: 0,
                    clock: 0,
                }),
            }
        }

        fn state(&self) -> std::sync::MutexGuard<'_, State<K, V>> {
            self.state.lock().unwrap_or_else(|error| error.into_inner())
        }

        pub fn get(&self, key: &K) -> Option<V> {
            let mut state = self.state();
            state.clock += 1;
            let clock = state.clock;
            let entry = state.entries.get_mut(key)?;
            entry.used = clock;
            Some(entry.value.clone())
        }

        fn insert(&self, key: K, value: V) {
            let weight = u64::from((self.weigher)(&key, &value));
            if weight > self.capacity {
                return;
            }
            let mut state = self.state();
            state.clock += 1;
            let used = state.clock;
            if let Some(old) = state.entries.insert(
                key,
                Entry {
                    value,
                    weight,
                    used,
                },
            ) {
                state.weight -= old.weight;
            }
            state.weight += weight;
            while state.weight > self.capacity {
                let Some(oldest) = state
                    .entries
                    .iter()
                    .min_by_key(|(_, entry)| entry.used)
                    .map(|(key, _)| key.clone())
                else {
                    break;
                };
                if let Some(entry) = state.entries.remove(&oldest) {
                    state.weight -= entry.weight;
                }
            }
        }

        pub fn contains(&self, key: &K) -> bool {
            self.state().entries.contains_key(key)
        }

        pub fn get_with(&self, key: K, init: impl FnOnce() -> V) -> V {
            if let Some(value) = self.get(&key) {
                return value;
            }
            let value = init();
            self.insert(key, value.clone());
            value
        }

        pub fn try_get_with<E>(
            &self,
            key: K,
            init: impl FnOnce() -> Result<V, E>,
        ) -> Result<V, Arc<E>> {
            if let Some(value) = self.get(&key) {
                return Ok(value);
            }
            let value = init().map_err(Arc::new)?;
            self.insert(key, value.clone());
            Ok(value)
        }

        pub fn invalidate_all(&self) {
            let mut state = self.state();
            state.entries.clear();
            state.weight = 0;
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn evicts_least_recently_used() {
            let cache: Cache<u32, u32> = Cache::new(3, |_, _| 1);
            for key in 0..3 {
                cache.get_with(key, || key);
            }
            cache.get_with(0, || unreachable!());
            cache.get_with(3, || 3);
            assert!(cache.contains(&0));
            assert!(!cache.contains(&1));
            assert!(cache.contains(&3));
        }

        #[test]
        fn skips_values_larger_than_capacity() {
            let cache: Cache<u32, u32> = Cache::new(10, |_, value| *value);
            assert_eq!(cache.get_with(1, || 11), 11);
            assert!(!cache.contains(&1));
            assert_eq!(
                cache
                    .try_get_with(2, || Err::<u32, _>("nope"))
                    .unwrap_err()
                    .as_ref(),
                &"nope"
            );
        }
    }
}
