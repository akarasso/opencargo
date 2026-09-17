use std::collections::HashMap;
use std::hash::Hash;

/// A bounded map evicting the least recently touched entry when full.
pub struct Memo<K, V> {
    cap: usize,
    tick: u64,
    map: HashMap<K, (V, u64)>,
}

impl<K: Hash + Eq + Clone, V> Memo<K, V> {
    pub fn new(cap: usize) -> Self {
        Self {
            cap: cap.max(1),
            tick: 0,
            map: HashMap::new(),
        }
    }

    pub fn get_mut(&mut self, key: &K) -> Option<&mut V> {
        self.tick += 1;
        let tick = self.tick;
        self.map.get_mut(key).map(|(value, used)| {
            *used = tick;
            value
        })
    }

    pub fn insert(&mut self, key: K, value: V) -> &mut V {
        if self.map.len() >= self.cap && !self.map.contains_key(&key) {
            if let Some(oldest) = self
                .map
                .iter()
                .min_by_key(|(_, (_, used))| *used)
                .map(|(k, _)| k.clone())
            {
                self.map.remove(&oldest);
            }
        }
        self.tick += 1;
        self.map.insert(key.clone(), (value, self.tick));
        &mut self.map.get_mut(&key).expect("just inserted").0
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.map.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evicts_least_recently_used() {
        let mut memo = Memo::new(2);
        memo.insert("a", 1);
        memo.insert("b", 2);
        assert_eq!(memo.get_mut(&"a"), Some(&mut 1));
        memo.insert("c", 3);
        assert_eq!(memo.len(), 2);
        assert!(
            memo.get_mut(&"b").is_none(),
            "b was the least recently used"
        );
        assert_eq!(memo.get_mut(&"a"), Some(&mut 1));
        assert_eq!(memo.get_mut(&"c"), Some(&mut 3));
        *memo.insert("c", 4) += 1;
        assert_eq!(memo.get_mut(&"c"), Some(&mut 5));
    }
}
