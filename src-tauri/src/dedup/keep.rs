//! 保留策略：一组重复文件里，留哪一份。
//!
//! **只对精确重复有意义。** 精确组里各份字节完全相同，留哪一份是纯粹的偏好问题，
//! 机器替用户选没有风险。感知组不一样——那是五张不同的照片，「哪张更好」机器答不了，
//! 所以感知组一律 [`Policy::Manual`]（D-113）。
//!
//! 这里是纯函数，不碰盘也不碰库：给一组条目和一个策略，返回该留哪一条的 id。
//! 真正的删除在 [`super::apply`]。

use std::path::Path;

/// 参与挑选的一条。字段就是挑选要用到的那几个，不多。
#[derive(Debug, Clone, Copy)]
pub struct Entry<'a> {
    pub id: i64,
    pub path: &'a Path,
    /// Unix 秒。
    pub mtime: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize, ts_rs::TS)]
#[serde(rename_all = "snake_case")]
#[ts(export, export_to = "../../src/lib/bindings/")]
pub enum Policy {
    /// 路径最浅的那份。
    ///
    /// 归档盘上的常见形态是「原始目录 + 若干层备份目录」，越深的往往越是副本。
    /// 这也是默认值：它挑出来的结果最符合「哪个是正本」的直觉。
    #[default]
    ShallowestPath,
    /// mtime 最早的那份，即最先存在的那份。
    Oldest,
    /// 谁都不选，全交给用户勾。
    Manual,
}

impl Policy {
    pub fn as_str(self) -> &'static str {
        match self {
            Policy::ShallowestPath => "shallowest_path",
            Policy::Oldest => "oldest",
            Policy::Manual => "manual",
        }
    }
}

/// 挑出该留下的那一条，返回它的 id。
///
/// [`Policy::Manual`] 和空输入返回 `None`——**调用方必须把 `None` 理解成
/// 「这一组一条都不删」**，而不是「随便删」。
///
/// 所有策略都带确定的兜底比较（路径字典序）：同深度、同 mtime 的情况很常见
/// （备份工具会保留时间戳），没有兜底的话两次运行会挑出不同的那份，
/// 用户看到的勾选就会莫名其妙地跳。
pub fn choose(entries: &[Entry], policy: Policy) -> Option<i64> {
    match policy {
        Policy::Manual => None,
        Policy::ShallowestPath => entries
            .iter()
            .min_by(|a, b| {
                depth(a.path).cmp(&depth(b.path)).then_with(|| a.path.cmp(b.path))
            })
            .map(|e| e.id),
        Policy::Oldest => entries
            .iter()
            .min_by(|a, b| a.mtime.cmp(&b.mtime).then_with(|| a.path.cmp(b.path)))
            .map(|e| e.id),
    }
}

/// 路径有多少层。
fn depth(p: &Path) -> usize {
    p.components().count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn entries(v: &[(i64, &str, i64)]) -> Vec<(i64, PathBuf, i64)> {
        v.iter().map(|(id, p, m)| (*id, PathBuf::from(p), *m)).collect()
    }

    fn pick(v: &[(i64, PathBuf, i64)], policy: Policy) -> Option<i64> {
        let es: Vec<Entry> = v.iter().map(|(id, p, m)| Entry { id: *id, path: p, mtime: *m }).collect();
        choose(&es, policy)
    }

    #[test]
    fn shallowest_wins() {
        let v = entries(&[
            (1, "/vol/backup/2019/old/a.jpg", 100),
            (2, "/vol/a.jpg", 200),
            (3, "/vol/photos/a.jpg", 50),
        ]);
        assert_eq!(pick(&v, Policy::ShallowestPath), Some(2));
    }

    #[test]
    fn oldest_wins() {
        let v = entries(&[(1, "/vol/x/a.jpg", 100), (2, "/vol/a.jpg", 200), (3, "/z/a.jpg", 50)]);
        assert_eq!(pick(&v, Policy::Oldest), Some(3));
    }

    #[test]
    fn ties_break_the_same_way_every_time() {
        // 备份工具会原样保留 mtime，同深度同时间是常态；没有确定的兜底，
        // 用户每次打开看到的勾选都不一样。
        let v = entries(&[(1, "/vol/b.jpg", 100), (2, "/vol/a.jpg", 100), (3, "/vol/c.jpg", 100)]);
        for p in [Policy::ShallowestPath, Policy::Oldest] {
            assert_eq!(pick(&v, p), Some(2), "{p:?} 该按路径字典序兜底");
        }
    }

    #[test]
    fn manual_picks_nothing() {
        let v = entries(&[(1, "/vol/a.jpg", 100), (2, "/vol/x/a.jpg", 200)]);
        assert_eq!(pick(&v, Policy::Manual), None, "感知组靠它保证「默认什么都不删」");
    }

    #[test]
    fn an_empty_group_picks_nothing() {
        assert_eq!(pick(&[], Policy::ShallowestPath), None);
        assert_eq!(pick(&[], Policy::Oldest), None);
    }

    #[test]
    fn the_default_is_shallowest() {
        assert_eq!(Policy::default(), Policy::ShallowestPath);
    }
}
