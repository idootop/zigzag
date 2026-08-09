//! 哈希缓存的接口。
//!
//! 去重最贵的一步是把盘上的字节读完（全量 blake3），或者把图解开（感知指纹）。
//! 这两件事对同一个未改动的文件永远给出同一个答案，算第二遍纯属浪费——归档盘
//! 上「再查一次重」是很常见的操作。
//!
//! **这也就是「续跑」本身。** 去重被打断后重来一遍，三级筛的结构一点不变，
//! 只是最贵的那一级全部变成查表命中。不必再为它单做一套断点续传：断点续传要
//! 记录「算到哪一条了」，而那个状态一旦和盘上的实际情况错位就会漏文件；
//! 缓存则是逐文件独立的，错位不了。
//!
//! 接口留在这里而不是直接依赖 [`crate::store`]：去重核心不该知道背后是 SQLite
//! 还是一张内存表，测试里也就能拿 [`MemoryCache`] 顶上，不必起一个数据库。

use std::path::Path;

/// 算过的哈希往哪儿存。
///
/// 实现方要能被多个 rayon 线程同时调用，所以是 `Sync` 且方法收 `&self`。
///
/// `size`/`mtime` 是键的一部分，不是附带信息：文件被改过之后，旧哈希描述的
/// 已经不是眼前这份内容。这一条和 [`crate::store::Db::probe_cache_get`] 同理，
/// 但后果更重——探测缓存错了只是压缩参数不对，哈希缓存错了会**把两个不同的
/// 文件判成重复**，然后其中一个被当副本删掉。
pub trait HashCache: Sync {
    fn get(&self, path: &Path, size: u64, mtime: i64) -> Option<String>;
    fn put(&self, path: &Path, size: u64, mtime: i64, hash: &str);
}

/// 没有缓存。查什么都不中，存什么都丢掉。
///
/// 让「不带缓存」和「带缓存」走同一条代码路径，省掉满地的 `Option` 分支。
pub struct NoCache;

impl HashCache for NoCache {
    fn get(&self, _: &Path, _: u64, _: i64) -> Option<String> {
        None
    }
    fn put(&self, _: &Path, _: u64, _: i64, _: &str) {}
}

/// 内存实现，给测试与基准用。
#[derive(Default)]
pub struct MemoryCache {
    map: std::sync::Mutex<std::collections::HashMap<(std::path::PathBuf, u64, i64), String>>,
    /// 命中次数，测试用来确认「第二遍真的没再算」。
    pub hits: std::sync::atomic::AtomicUsize,
}

impl MemoryCache {
    pub fn hits(&self) -> usize {
        self.hits.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn len(&self) -> usize {
        self.map.lock().expect("缓存锁中毒").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl HashCache for MemoryCache {
    fn get(&self, path: &Path, size: u64, mtime: i64) -> Option<String> {
        let hit = self
            .map
            .lock()
            .expect("缓存锁中毒")
            .get(&(path.to_path_buf(), size, mtime))
            .cloned();
        if hit.is_some() {
            self.hits.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        hit
    }

    fn put(&self, path: &Path, size: u64, mtime: i64, hash: &str) {
        self.map
            .lock()
            .expect("缓存锁中毒")
            .insert((path.to_path_buf(), size, mtime), hash.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_changed_file_misses_even_at_the_same_path() {
        // 这条是缓存里最要命的一种错：路径没变、内容变了。
        // 命中的话两个不同的文件会被判成重复，然后一个被当副本删掉。
        let c = MemoryCache::default();
        let p = Path::new("/a.jpg");
        c.put(p, 100, 7, "abc");
        assert_eq!(c.get(p, 100, 7).as_deref(), Some("abc"));
        assert_eq!(c.get(p, 101, 7), None, "大小变了不该命中");
        assert_eq!(c.get(p, 100, 8), None, "mtime 变了不该命中");
    }

    #[test]
    fn nocache_never_hits() {
        NoCache.put(Path::new("/a.jpg"), 1, 1, "abc");
        assert_eq!(NoCache.get(Path::new("/a.jpg"), 1, 1), None);
    }
}
