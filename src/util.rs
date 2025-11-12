use indexmap::IndexMap;
use std::collections::HashMap;

pub trait MapExt<K, V> {
    fn insert_first(&mut self, key: K, value: V);
}

impl<K, V> MapExt<K, V> for IndexMap<K, V>
where
    K: std::hash::Hash + Eq,
{
    fn insert_first(&mut self, key: K, value: V) {
        let prev = self.insert(key, value);
        assert!(prev.is_none());
    }
}

impl<K, V> MapExt<K, V> for HashMap<K, V>
where
    K: std::hash::Hash + Eq,
{
    fn insert_first(&mut self, key: K, value: V) {
        let prev = self.insert(key, value);
        assert!(prev.is_none());
    }
}
