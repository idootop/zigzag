//! 去重结果的持久化，以及哈希缓存的 SQLite 实现。
//!
//! 分成两件事，各自解决一个问题：
//!
//! 1. **结果落库**（[`Db::create_dedup_run`] 等）。扫十万文件要几分钟，扫完之后
//!    用户还得逐组看过去、勾选、确认——这中间应用被关掉是常态。结果不落库，
//!    每开一次应用就得重扫一次。
//! 2. **哈希缓存**（[`SqliteHashCache`]）。这才是「续跑」的实现：扫描过程被打断，
//!    重来一遍时三级筛的结构一点不变，只是最贵的那一级全部变成查表命中。
//!    理由见 [`crate::dedup::cache`] 的模块文档。
//!
//! 去重核心（[`crate::dedup`]）不引用本模块，只认 [`HashCache`] 这个 trait —— 方向是
//! store 依赖 dedup，不是反过来。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use rusqlite::{params, OptionalExtension};
use serde::Serialize;
use ts_rs::TS;

use super::repo::Db;
use crate::dedup::apply::{GroupPlan, Outcome, Target};
use crate::dedup::cache::HashCache;
use crate::dedup::keep::{Entry, Policy};
use crate::dedup::{exact::DupGroup, perceptual::SimilarGroup};
use crate::error::Result;

/// 一次去重扫描。
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export, export_to = "../../src/lib/bindings/")]
pub struct DedupRun {
    #[ts(type = "number")] pub id: i64,
    pub roots: Vec<String>,
    /// `exact` | `perceptual`。
    pub mode: String,
    /// `scanning` | `ready` | `applying` | `done` | `cancelled`。
    pub status: String,
    /// 感知模式的汉明距离阈值；精确模式为 `None`。
    pub threshold: Option<u32>,
}

/// 库里读出来的一组，连成员一起。
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export, export_to = "../../src/lib/bindings/")]
pub struct StoredGroup {
    #[ts(type = "number")] pub id: i64,
    pub hash: String,
    #[ts(type = "number")]
    pub reclaimable: u64,
    pub members: Vec<StoredMember>,
}

#[derive(Debug, Clone, Serialize, TS)]
#[ts(export, export_to = "../../src/lib/bindings/")]
pub struct StoredMember {
    #[ts(type = "number")] pub id: i64,
    pub path: String,
    #[ts(type = "number")]
    pub size: u64,
    #[ts(type = "number")] pub mtime: i64,
    /// 到代表元的汉明距离。精确组恒为 0。
    pub distance: u32,
    /// `true` = 留下。默认全 `true`，见 [`Db::save_dedup_groups`]。
    pub keep: bool,
    /// `None` = 还没动；`trashed` | `failed`。
    pub disposal: Option<String>,
}

/// 落库用的中性分组。精确组和感知组都折成这个形状。
///
/// 不让 [`Db`] 直接吃 [`DupGroup`] / [`SimilarGroup`] 两种类型各写一遍插入逻辑：
/// 两者的差别只有「成员的 distance 是不是恒零」，为此重复一整套 SQL 不值当。
#[derive(Debug, Clone)]
pub struct GroupRow {
    pub hash: String,
    pub reclaimable: u64,
    pub members: Vec<MemberRow>,
}

#[derive(Debug, Clone)]
pub struct MemberRow {
    pub path: String,
    pub size: u64,
    pub mtime: i64,
    pub distance: u32,
}

impl From<&DupGroup> for GroupRow {
    fn from(g: &DupGroup) -> Self {
        Self {
            hash: g.hash.clone(),
            reclaimable: g.reclaimable(),
            members: g
                .files
                .iter()
                .map(|c| MemberRow {
                    path: c.path.to_string_lossy().into_owned(),
                    size: c.size,
                    mtime: c.mtime,
                    distance: 0,
                })
                .collect(),
        }
    }
}

impl From<&SimilarGroup> for GroupRow {
    fn from(g: &SimilarGroup) -> Self {
        // 代表元排在第一位，距离 0。前端按存入顺序展示，第一条就是「基准照」。
        let mut members = vec![MemberRow {
            path: g.seed.path.to_string_lossy().into_owned(),
            size: g.seed.size,
            mtime: g.seed.mtime,
            distance: 0,
        }];
        members.extend(g.others.iter().map(|(c, d)| MemberRow {
            path: c.path.to_string_lossy().into_owned(),
            size: c.size,
            mtime: c.mtime,
            distance: *d,
        }));
        Self {
            hash: g.seed_fingerprint.to_hex(),
            reclaimable: g.reclaimable(),
            members,
        }
    }
}

impl Db {
    /// 开一次去重扫描，状态 `scanning`。
    pub fn create_dedup_run(
        &self,
        roots: &[String],
        mode: &str,
        threshold: Option<u32>,
    ) -> Result<i64> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO dedup_runs (roots_json, mode, status, threshold, created_at)
             VALUES (?1, ?2, 'scanning', ?3, ?4)",
            params![serde_json::to_string(roots)?, mode, threshold, super::repo::now()],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// 把分组写进去。一个事务，要么全进要么全不进。
    ///
    /// 所有成员的 `keep` 都是 1——**默认状态必须是「什么都不删」**。哪些该置 0
    /// 由保留策略在这之后单独决定（[`Db::set_member_keep`]），而不是在写入时
    /// 顺手替用户做主。
    pub fn save_dedup_groups(&self, run_id: i64, groups: &[GroupRow]) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        for g in groups {
            tx.execute(
                "INSERT INTO dedup_groups (run_id, hash, reclaimable) VALUES (?1, ?2, ?3)",
                params![run_id, g.hash, g.reclaimable as i64],
            )?;
            let gid = tx.last_insert_rowid();
            for m in &g.members {
                tx.execute(
                    "INSERT INTO dedup_members (group_id, path, size, mtime, distance)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![gid, m.path, m.size as i64, m.mtime, m.distance],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// 按「能省下的字节」从多到少列出分组。分页是为了十万文件时前端不至于一次
    /// 吃下几万组。
    pub fn list_dedup_groups(
        &self,
        run_id: i64,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<StoredGroup>> {
        let conn = self.lock();
        let mut q = conn.prepare(
            "SELECT id, hash, reclaimable FROM dedup_groups
             WHERE run_id = ?1 ORDER BY reclaimable DESC, id LIMIT ?2 OFFSET ?3",
        )?;
        let mut groups: Vec<StoredGroup> = q
            .query_map(params![run_id, limit as i64, offset as i64], |r| {
                Ok(StoredGroup {
                    id: r.get(0)?,
                    hash: r.get(1)?,
                    reclaimable: r.get::<_, i64>(2)? as u64,
                    members: Vec::new(),
                })
            })?
            .collect::<rusqlite::Result<_>>()?;

        let mut mq = conn.prepare(
            "SELECT id, path, size, mtime, distance, keep, disposal FROM dedup_members
             WHERE group_id = ?1 ORDER BY id",
        )?;
        for g in &mut groups {
            g.members = mq
                .query_map(params![g.id], |r| {
                    Ok(StoredMember {
                        id: r.get(0)?,
                        path: r.get(1)?,
                        size: r.get::<_, i64>(2)? as u64,
                        mtime: r.get(3)?,
                        distance: r.get(4)?,
                        keep: r.get::<_, i64>(5)? != 0,
                        disposal: r.get(6)?,
                    })
                })?
                .collect::<rusqlite::Result<_>>()?;
        }
        Ok(groups)
    }

    /// 勾选/取消勾选一条。前端每次点击都调这个——直接落库，应用被关掉勾选不丢。
    pub fn set_member_keep(&self, member_id: i64, keep: bool) -> Result<()> {
        self.lock().execute(
            "UPDATE dedup_members SET keep = ?2 WHERE id = ?1",
            params![member_id, i64::from(keep)],
        )?;
        Ok(())
    }

    /// 批量置 `keep`。保留策略一次算完整个 run，逐条 UPDATE 会是几万次 fsync。
    pub fn set_members_keep(&self, ids: &[i64], keep: bool) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        {
            let mut st = tx.prepare("UPDATE dedup_members SET keep = ?2 WHERE id = ?1")?;
            for id in ids {
                st.execute(params![id, i64::from(keep)])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// 记下一条的处置结果（`trashed` / `failed`）。
    ///
    /// 删除本身不可逆（虽然进的是回收站），所以**先删后记**：记录写失败最多是
    /// 界面上少一个标记，反过来则会让用户以为文件还在。
    pub fn mark_member_disposed(&self, member_id: i64, disposal: &str) -> Result<()> {
        self.lock().execute(
            "UPDATE dedup_members SET disposal = ?2 WHERE id = ?1",
            params![member_id, disposal],
        )?;
        Ok(())
    }

    pub fn set_dedup_run_status(&self, run_id: i64, status: &str) -> Result<()> {
        let finished = matches!(status, "done" | "cancelled").then(super::repo::now);
        self.lock().execute(
            "UPDATE dedup_runs SET status = ?2, finished_at = ?3 WHERE id = ?1",
            params![run_id, status, finished],
        )?;
        Ok(())
    }

    /// 最近一次扫描。应用启动时用它决定要不要把上次没看完的结果直接摆出来。
    pub fn latest_dedup_run(&self) -> Result<Option<DedupRun>> {
        let conn = self.lock();
        let run = conn
            .query_row(
                "SELECT id, roots_json, mode, status, threshold FROM dedup_runs
                 ORDER BY id DESC LIMIT 1",
                [],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, Option<u32>>(4)?,
                    ))
                },
            )
            .optional()?;
        Ok(run.map(|(id, roots_json, mode, status, threshold)| DedupRun {
            id,
            // 解不出来就当没有根目录：这个字段只用来在界面上回显「上次扫的是哪儿」，
            // 为它让整次续跑失败不值得。
            roots: serde_json::from_str(&roots_json).unwrap_or_default(),
            mode,
            status,
            threshold,
        }))
    }

    /// 按保留策略把整个 run 的 `keep` 重算一遍。返回被勾选删除的条数。
    ///
    /// 这是一次**批量覆盖**：用户手动改过的勾选会被冲掉。这是有意的——它对应
    /// 界面上「按策略重新勾选」那个动作，用户明确要求了才会触发。
    ///
    /// 已经处置过的成员（`disposal` 非空）不参与：文件都进回收站了，
    /// 再给它算一次留不留没有意义，还会让它重新出现在删除计划里。
    ///
    /// [`Policy::Manual`] 会把所有 `keep` 复位成 1，即「什么都不删」——
    /// 感知模式靠它保证不预勾选（D-113）。
    pub fn apply_keep_policy(&self, run_id: i64, policy: Policy) -> Result<usize> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        // 先全部复位成「留」，再把该删的挑出来置 0。这样策略换来换去也不会
        // 留下上一次的残留勾选。
        tx.execute(
            "UPDATE dedup_members SET keep = 1
             WHERE disposal IS NULL AND group_id IN (SELECT id FROM dedup_groups WHERE run_id = ?1)",
            params![run_id],
        )?;

        let mut removed = 0usize;
        if policy != Policy::Manual {
            let mut q = tx.prepare(
                "SELECT m.group_id, m.id, m.path, m.mtime FROM dedup_members m
                 JOIN dedup_groups g ON g.id = m.group_id
                 WHERE g.run_id = ?1 AND m.disposal IS NULL ORDER BY m.group_id, m.id",
            )?;
            let rows: Vec<(i64, i64, String, i64)> = q
                .query_map(params![run_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
                .collect::<rusqlite::Result<_>>()?;
            drop(q);

            let mut st = tx.prepare("UPDATE dedup_members SET keep = 0 WHERE id = ?1")?;
            // rows 已按 group_id 排序，`chunk_by` 把连续同组的切成一片。
            for group in rows.chunk_by(|a, b| a.0 == b.0) {
                let paths: Vec<PathBuf> = group.iter().map(|r| PathBuf::from(&r.2)).collect();
                let entries: Vec<Entry> = group
                    .iter()
                    .zip(&paths)
                    .map(|(r, p)| Entry { id: r.1, path: p, mtime: r.3 })
                    .collect();
                // 挑不出来（理论上只有空组）就整组不动，绝不「随便删」。
                let Some(keeper) = crate::dedup::keep::choose(&entries, policy) else {
                    continue;
                };
                for e in &entries {
                    if e.id != keeper {
                        st.execute(params![e.id])?;
                        removed += 1;
                    }
                }
            }
        }
        tx.commit()?;
        Ok(removed)
    }

    /// 当前勾了多少条要删、共多少字节。
    ///
    /// 确认框上那两个数只能从这里来。界面是翻页读的，拿已加载的那几页去数，
    /// 用户会以为「删的就是我看到的这些」，而实际删的是整个 run。
    pub fn pending_removals(&self, run_id: i64) -> Result<(usize, u64)> {
        let conn = self.lock();
        let (count, bytes): (i64, i64) = conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(m.size), 0) FROM dedup_members m
             JOIN dedup_groups g ON g.id = m.group_id
             WHERE g.run_id = ?1 AND m.keep = 0 AND m.disposal IS NULL",
            params![run_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok((count as usize, bytes.max(0) as u64))
    }

    /// 把库里的勾选状态翻成删除计划。
    ///
    /// 只收 `keep = 0` 的做 `remove`，`keep = 1` 的进 `keep` 列表——后者是
    /// [`crate::dedup::apply`] 那条「一组不能被删空」的判据，必须一起带过去。
    /// 已处置的两边都不进。
    pub fn dedup_plans(&self, run_id: i64) -> Result<Vec<GroupPlan>> {
        let conn = self.lock();
        let mut q = conn.prepare(
            "SELECT m.group_id, m.id, m.path, m.size, m.mtime, m.keep FROM dedup_members m
             JOIN dedup_groups g ON g.id = m.group_id
             WHERE g.run_id = ?1 AND m.disposal IS NULL ORDER BY m.group_id, m.id",
        )?;
        let rows: Vec<(i64, i64, String, i64, i64, i64)> = q
            .query_map(params![run_id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
            })?
            .collect::<rusqlite::Result<_>>()?;

        let mut plans: Vec<GroupPlan> = Vec::new();
        for group in rows.chunk_by(|a, b| a.0 == b.0) {
            let mut plan =
                GroupPlan { group_id: group[0].0, keep: Vec::new(), remove: Vec::new() };
            for r in group {
                if r.5 != 0 {
                    plan.keep.push(PathBuf::from(&r.2));
                } else {
                    plan.remove.push(Target {
                        member_id: r.1,
                        path: PathBuf::from(&r.2),
                        size: r.3 as u64,
                        mtime: r.4,
                    });
                }
            }
            // 一条都不删的组不必进计划，白跑一趟。
            if !plan.remove.is_empty() {
                plans.push(plan);
            }
        }
        Ok(plans)
    }

    /// 记下一批处置结果。一个事务。
    ///
    /// [`Outcome::Skipped`] 不写库——`disposal` 为 NULL 的含义就是「还没动过」，
    /// 而跳过的正是没动过，下次还能再试。
    pub fn record_disposals(&self, results: &[(i64, Outcome)]) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        {
            let mut st = tx.prepare("UPDATE dedup_members SET disposal = ?2 WHERE id = ?1")?;
            for (id, outcome) in results {
                if let Some(d) = outcome.disposal() {
                    st.execute(params![id, d])?;
                }
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// 删掉一次扫描的全部结果。外键 `ON DELETE CASCADE` 带走组和成员。
    ///
    /// 不动 `hash_cache`：那张表描述的是**文件**，不是这次扫描的结论，
    /// 删掉等于把下一次续跑的本钱也扔了。
    pub fn delete_dedup_run(&self, run_id: i64) -> Result<()> {
        self.lock().execute("DELETE FROM dedup_runs WHERE id = ?1", params![run_id])?;
        Ok(())
    }
}

/// 攒够多少条 `put` 就写一次库。
///
/// 不逐条写：十万文件逐条 INSERT 会是十万次事务提交。也不全攒到最后：中途被
/// 强杀（用户直接退出、掉电）就前功尽弃，而缓存的全部意义就是「被打断之后
/// 不用重来」。
const FLUSH_EVERY: usize = 2048;

/// [`HashCache`] 的 SQLite 实现。
///
/// **读走内存快照，不查库。** 构造时把该算法的全部缓存行一次读进 `HashMap`，
/// 之后 `get` 不碰连接。理由是 [`Db`] 是单连接 + `Mutex`，而 `get` 会被 rayon
/// 的每个线程在每个文件上调用——让它们去抢同一把锁，等于把并行去重变回串行。
/// 代价是十万行大约十来兆内存，换掉一个必然的争用点，划算。
pub struct SqliteHashCache<'a> {
    db: &'a Db,
    algo: String,
    snapshot: HashMap<String, (u64, i64, String)>,
    pending: Mutex<Vec<(String, u64, i64, String)>>,
    hits: AtomicUsize,
}

impl<'a> SqliteHashCache<'a> {
    /// `algo` 是算法标识（全量哈希用 `blake3`，感知指纹用
    /// [`crate::dedup::perceptual::FINGERPRINT_ALGO`]）。
    ///
    /// 它必须随算法口径一起变：口径变了而标识没变，旧指纹会被当成新指纹复用，
    /// 分组静默全错。
    pub fn new(db: &'a Db, algo: &str) -> Result<Self> {
        let snapshot = {
            let conn = db.lock();
            let mut q =
                conn.prepare("SELECT path, size, mtime, hash FROM hash_cache WHERE algo = ?1")?;
            let rows = q.query_map(params![algo], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    (r.get::<_, i64>(1)? as u64, r.get::<_, i64>(2)?, r.get::<_, String>(3)?),
                ))
            })?;
            rows.collect::<rusqlite::Result<HashMap<_, _>>>()?
        };
        Ok(Self {
            db,
            algo: algo.to_string(),
            snapshot,
            pending: Mutex::new(Vec::new()),
            hits: AtomicUsize::new(0),
        })
    }

    pub fn hits(&self) -> usize {
        self.hits.load(Ordering::Relaxed)
    }

    /// 把攒着的写库。`(path, algo)` 是主键，同一文件重算后覆盖旧值。
    pub fn flush(&self) -> Result<()> {
        let batch = std::mem::take(&mut *self.pending.lock().expect("缓存锁中毒"));
        if batch.is_empty() {
            return Ok(());
        }
        let mut conn = self.db.lock();
        let tx = conn.transaction()?;
        {
            let mut st = tx.prepare(
                "INSERT OR REPLACE INTO hash_cache (path, algo, size, mtime, hash)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for (path, size, mtime, hash) in &batch {
                st.execute(params![path, self.algo, *size as i64, mtime, hash])?;
            }
        }
        tx.commit()?;
        Ok(())
    }
}

impl HashCache for SqliteHashCache<'_> {
    fn get(&self, path: &Path, size: u64, mtime: i64) -> Option<String> {
        let (s, m, h) = self.snapshot.get(path.to_string_lossy().as_ref())?;
        // 大小或 mtime 对不上 = 文件被改过，旧哈希描述的不是眼前这份内容。
        // 放行的话两个不同的文件会被判成重复，然后一个被当副本删掉。
        if *s != size || *m != mtime {
            return None;
        }
        self.hits.fetch_add(1, Ordering::Relaxed);
        Some(h.clone())
    }

    fn put(&self, path: &Path, size: u64, mtime: i64, hash: &str) {
        let full = {
            let mut p = self.pending.lock().expect("缓存锁中毒");
            p.push((path.to_string_lossy().into_owned(), size, mtime, hash.to_string()));
            p.len() >= FLUSH_EVERY
        };
        if full {
            // 写库失败不该让整次去重停下——缓存丢了只是下次要重算。
            if let Err(e) = self.flush() {
                tracing::warn!(%e, "哈希缓存写入失败，本批丢弃");
            }
        }
    }
}

impl Drop for SqliteHashCache<'_> {
    fn drop(&mut self) {
        if let Err(e) = self.flush() {
            tracing::warn!(%e, "哈希缓存收尾写入失败");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dedup::exact::Candidate;
    use std::path::PathBuf;

    fn cand(p: &str, size: u64, mtime: i64) -> Candidate {
        Candidate { path: PathBuf::from(p), size, mtime }
    }

    fn sample_group() -> GroupRow {
        (&DupGroup {
            hash: "aabb".into(),
            size: 100,
            files: vec![cand("/a.jpg", 100, 1), cand("/b.jpg", 100, 2)],
        })
            .into()
    }

    #[test]
    fn a_saved_run_round_trips() {
        let db = Db::open_in_memory().unwrap();
        let run = db.create_dedup_run(&["/vol".into()], "exact", None).unwrap();
        db.save_dedup_groups(run, &[sample_group()]).unwrap();

        let groups = db.list_dedup_groups(run, 50, 0).unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].hash, "aabb");
        assert_eq!(groups[0].reclaimable, 100, "两份 100 字节，删一份省 100");
        let paths: Vec<_> = groups[0].members.iter().map(|m| m.path.as_str()).collect();
        assert_eq!(paths, ["/a.jpg", "/b.jpg"]);

        let latest = db.latest_dedup_run().unwrap().unwrap();
        assert_eq!(latest.id, run);
        assert_eq!(latest.roots, ["/vol"]);
        assert_eq!(latest.status, "scanning");
    }

    #[test]
    fn every_member_starts_out_kept() {
        // 这条守的是「默认什么都不删」。写入阶段替用户勾掉任何一条都是错的。
        let db = Db::open_in_memory().unwrap();
        let run = db.create_dedup_run(&[], "exact", None).unwrap();
        db.save_dedup_groups(run, &[sample_group()]).unwrap();
        let g = db.list_dedup_groups(run, 50, 0).unwrap();
        assert!(g[0].members.iter().all(|m| m.keep), "落库时不该有任何一条被预先勾掉");
        assert!(g[0].members.iter().all(|m| m.disposal.is_none()));
    }

    #[test]
    fn perceptual_distances_survive_the_round_trip() {
        // 距离是用户决定删不删的唯一依据，丢了这一列界面就只能瞎显示。
        let db = Db::open_in_memory().unwrap();
        let run = db.create_dedup_run(&[], "perceptual", Some(12)).unwrap();
        let g = SimilarGroup {
            seed: cand("/seed.jpg", 300, 1),
            seed_fingerprint: crate::dedup::perceptual::Fingerprint(0x0123_4567_89ab_cdef),
            others: vec![(cand("/near.jpg", 200, 2), 3), (cand("/far.jpg", 100, 3), 11)],
        };
        db.save_dedup_groups(run, &[(&g).into()]).unwrap();

        let got = db.list_dedup_groups(run, 50, 0).unwrap();
        let ds: Vec<_> = got[0].members.iter().map(|m| m.distance).collect();
        assert_eq!(ds, [0, 3, 11], "代表元距离 0 且排第一");
        assert_eq!(got[0].reclaimable, 300, "感知组省下的是除代表元外各自的实际大小");
        assert_eq!(db.latest_dedup_run().unwrap().unwrap().threshold, Some(12));
    }

    #[test]
    fn groups_come_back_biggest_win_first() {
        let db = Db::open_in_memory().unwrap();
        let run = db.create_dedup_run(&[], "exact", None).unwrap();
        let small = (&DupGroup {
            hash: "small".into(),
            size: 10,
            files: vec![cand("/s1", 10, 1), cand("/s2", 10, 1)],
        })
            .into();
        let big = (&DupGroup {
            hash: "big".into(),
            size: 900,
            files: vec![cand("/b1", 900, 1), cand("/b2", 900, 1)],
        })
            .into();
        db.save_dedup_groups(run, &[small, big]).unwrap();
        let hashes: Vec<_> =
            db.list_dedup_groups(run, 50, 0).unwrap().into_iter().map(|g| g.hash).collect();
        assert_eq!(hashes, ["big", "small"]);
    }

    #[test]
    fn keep_and_disposal_persist() {
        let db = Db::open_in_memory().unwrap();
        let run = db.create_dedup_run(&[], "exact", None).unwrap();
        db.save_dedup_groups(run, &[sample_group()]).unwrap();
        let m = db.list_dedup_groups(run, 50, 0).unwrap()[0].members[1].id;

        db.set_member_keep(m, false).unwrap();
        db.mark_member_disposed(m, "trashed").unwrap();

        let back = db.list_dedup_groups(run, 50, 0).unwrap();
        assert!(!back[0].members[1].keep);
        assert_eq!(back[0].members[1].disposal.as_deref(), Some("trashed"));
        assert!(back[0].members[0].keep, "只该动被指名的那一条");
    }

    #[test]
    fn deleting_a_run_takes_its_groups_but_leaves_the_hash_cache() {
        let db = Db::open_in_memory().unwrap();
        let run = db.create_dedup_run(&[], "exact", None).unwrap();
        db.save_dedup_groups(run, &[sample_group()]).unwrap();
        {
            let c = SqliteHashCache::new(&db, "blake3").unwrap();
            c.put(Path::new("/a.jpg"), 100, 1, "deadbeef");
            c.flush().unwrap();
        }
        db.delete_dedup_run(run).unwrap();

        assert!(db.list_dedup_groups(run, 50, 0).unwrap().is_empty());
        let c = SqliteHashCache::new(&db, "blake3").unwrap();
        assert_eq!(
            c.get(Path::new("/a.jpg"), 100, 1).as_deref(),
            Some("deadbeef"),
            "缓存描述的是文件不是这次扫描，不该被连坐删掉"
        );
    }

    #[test]
    fn the_cache_survives_a_restart() {
        // 「续跑」的最小证明：新开一个 SqliteHashCache（相当于重启应用）仍然命中。
        let db = Db::open_in_memory().unwrap();
        {
            let c = SqliteHashCache::new(&db, "blake3").unwrap();
            c.put(Path::new("/a.jpg"), 100, 7, "abc");
        } // Drop 兜底写库，调用方忘了 flush 也不丢

        let c = SqliteHashCache::new(&db, "blake3").unwrap();
        assert_eq!(c.get(Path::new("/a.jpg"), 100, 7).as_deref(), Some("abc"));
        assert_eq!(c.hits(), 1);
    }

    #[test]
    fn a_changed_file_misses() {
        let db = Db::open_in_memory().unwrap();
        let c = SqliteHashCache::new(&db, "blake3").unwrap();
        c.put(Path::new("/a.jpg"), 100, 7, "abc");
        c.flush().unwrap();
        let c = SqliteHashCache::new(&db, "blake3").unwrap();
        assert_eq!(c.get(Path::new("/a.jpg"), 101, 7), None, "大小变了不该命中");
        assert_eq!(c.get(Path::new("/a.jpg"), 100, 8), None, "mtime 变了不该命中");
        assert_eq!(c.hits(), 0);
    }

    /// 一个 run，一组三份：一份在浅处、两份埋在备份目录里。
    fn run_with_a_three_way_group(db: &Db) -> i64 {
        let run = db.create_dedup_run(&[], "exact", None).unwrap();
        let g: GroupRow = (&DupGroup {
            hash: "h".into(),
            size: 50,
            files: vec![
                cand("/vol/backup/2019/a.jpg", 50, 300),
                cand("/vol/a.jpg", 50, 200),
                cand("/vol/backup/b.jpg", 50, 100),
            ],
        })
            .into();
        db.save_dedup_groups(run, &[g]).unwrap();
        run
    }

    fn kept_paths(db: &Db, run: i64) -> Vec<String> {
        db.list_dedup_groups(run, 50, 0).unwrap()[0]
            .members
            .iter()
            .filter(|m| m.keep)
            .map(|m| m.path.clone())
            .collect()
    }

    #[test]
    fn the_policy_keeps_exactly_one_per_group() {
        let db = Db::open_in_memory().unwrap();
        let run = run_with_a_three_way_group(&db);

        assert_eq!(db.apply_keep_policy(run, Policy::ShallowestPath).unwrap(), 2);
        assert_eq!(kept_paths(&db, run), ["/vol/a.jpg"], "最浅的那份");

        assert_eq!(db.apply_keep_policy(run, Policy::Oldest).unwrap(), 2);
        assert_eq!(kept_paths(&db, run), ["/vol/backup/b.jpg"], "mtime 最早的那份");
    }

    #[test]
    fn the_pending_count_covers_the_whole_run_not_one_page() {
        // 确认框上的数字。它必须等于「点下去真会删掉的量」，
        // 否则那个确认就是在骗人授权。
        let db = Db::open_in_memory().unwrap();
        let run = run_with_a_three_way_group(&db);
        assert_eq!(db.pending_removals(run).unwrap(), (0, 0), "默认什么都不删");

        db.apply_keep_policy(run, Policy::ShallowestPath).unwrap();
        assert_eq!(db.pending_removals(run).unwrap(), (2, 100), "三份留一份，删两份共 100 字节");

        // 已经进过回收站的不该再算一遍。
        let doomed = db.dedup_plans(run).unwrap()[0].remove[0].member_id;
        db.mark_member_disposed(doomed, "trashed").unwrap();
        assert_eq!(db.pending_removals(run).unwrap(), (1, 50));
    }

    #[test]
    fn switching_policies_leaves_no_residue() {
        // 换策略是「重算」不是「叠加」。不复位的话上一次勾掉的会留在那儿，
        // 于是一组里可能一份都不剩。
        let db = Db::open_in_memory().unwrap();
        let run = run_with_a_three_way_group(&db);
        db.apply_keep_policy(run, Policy::ShallowestPath).unwrap();
        db.apply_keep_policy(run, Policy::Oldest).unwrap();
        assert_eq!(kept_paths(&db, run).len(), 1, "永远只留一份，不多不少");
    }

    #[test]
    fn manual_policy_unchecks_everything() {
        // 感知模式靠这条保证「默认什么都不删」（D-113）。
        let db = Db::open_in_memory().unwrap();
        let run = run_with_a_three_way_group(&db);
        db.apply_keep_policy(run, Policy::ShallowestPath).unwrap();

        assert_eq!(db.apply_keep_policy(run, Policy::Manual).unwrap(), 0);
        assert_eq!(kept_paths(&db, run).len(), 3, "全都恢复成「留」");
        assert!(db.dedup_plans(run).unwrap().is_empty(), "没有要删的，就没有计划");
    }

    #[test]
    fn plans_carry_the_keepers_so_a_group_cannot_be_emptied() {
        let db = Db::open_in_memory().unwrap();
        let run = run_with_a_three_way_group(&db);
        db.apply_keep_policy(run, Policy::ShallowestPath).unwrap();

        let plans = db.dedup_plans(run).unwrap();
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].keep, [PathBuf::from("/vol/a.jpg")], "留下的必须一起交给 apply");
        assert_eq!(plans[0].remove.len(), 2);
        assert!(plans[0].remove.iter().all(|t| t.size == 50 && t.mtime != 0), "核对用的元数据得带上");
    }

    #[test]
    fn already_trashed_members_drop_out_of_the_picture() {
        // 进了回收站的既不该再被策略算一次，也不该再出现在删除计划里。
        let db = Db::open_in_memory().unwrap();
        let run = run_with_a_three_way_group(&db);
        db.apply_keep_policy(run, Policy::ShallowestPath).unwrap();
        let plans = db.dedup_plans(run).unwrap();

        let done: Vec<_> =
            plans[0].remove.iter().map(|t| (t.member_id, Outcome::Trashed)).collect();
        db.record_disposals(&done).unwrap();

        assert!(db.dedup_plans(run).unwrap().is_empty(), "都处置完了就没有计划了");
        assert_eq!(db.apply_keep_policy(run, Policy::Oldest).unwrap(), 0, "剩一份，没得删");
        let m = &db.list_dedup_groups(run, 50, 0).unwrap()[0].members;
        assert_eq!(m.iter().filter(|m| m.disposal.as_deref() == Some("trashed")).count(), 2);
    }

    #[test]
    fn a_skipped_outcome_is_not_recorded() {
        // disposal 为 NULL = 还没动过。跳过的正是没动过，下次还得能再试。
        let db = Db::open_in_memory().unwrap();
        let run = run_with_a_three_way_group(&db);
        let id = db.list_dedup_groups(run, 50, 0).unwrap()[0].members[0].id;

        db.record_disposals(&[(id, Outcome::Skipped("测试"))]).unwrap();
        assert!(db.list_dedup_groups(run, 50, 0).unwrap()[0].members[0].disposal.is_none());

        db.record_disposals(&[(id, Outcome::Failed("盘满了".into()))]).unwrap();
        assert_eq!(
            db.list_dedup_groups(run, 50, 0).unwrap()[0].members[0].disposal.as_deref(),
            Some("failed")
        );
    }

    #[test]
    fn a_second_scan_reads_nothing_at_the_expensive_tier() {
        // #29 的正题：这就是「续跑」。同一批文件扫第二遍，第三级（全量读）
        // 必须一条都不读——扫十万文件几分钟的开销全在那一级上。
        struct Tmp(PathBuf);
        impl Drop for Tmp {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let dir = Tmp(std::env::temp_dir().join("zigzag-store-dedup-resume"));
        let _ = std::fs::remove_dir_all(&dir.0);
        std::fs::create_dir_all(&dir.0).unwrap();

        // 大于 128 KB，否则第二级就定论了、根本走不到第三级（sample_was_final）。
        let big = vec![7u8; 200 * 1024];
        let files: Vec<_> = ["a.bin", "b.bin"]
            .iter()
            .map(|n| {
                let p = dir.0.join(n);
                std::fs::write(&p, &big).unwrap();
                let md = std::fs::metadata(&p).unwrap();
                Candidate {
                    path: p,
                    size: md.len(),
                    mtime: md
                        .modified()
                        .unwrap()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_secs() as i64,
                }
            })
            .collect();

        let db = Db::open_in_memory().unwrap();
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let opts = crate::dedup::exact::Options::default();

        let (g1, s1) = {
            let c = SqliteHashCache::new(&db, "blake3").unwrap();
            let r = crate::dedup::exact::find(files.clone(), &opts, &c, &cancel, |_| {});
            c.flush().unwrap();
            r
        };
        assert_eq!(s1.fully_read, 2, "第一遍两条都得真读");

        let c = SqliteHashCache::new(&db, "blake3").unwrap();
        let (g2, s2) = crate::dedup::exact::find(files, &opts, &c, &cancel, |_| {});
        assert_eq!(s2.fully_read, 2, "fully_read 统计的是「过了第三级」的条数，不是读盘次数");
        assert_eq!(c.hits(), 2, "但这两条都该是查表命中，一个字节都没读");
        assert_eq!(g1, g2, "命中缓存不能改变分组结果");
    }

    #[test]
    fn algorithms_do_not_see_each_others_hashes() {
        // 全量 blake3 和感知指纹都住在这张表里。串了台就是拿指纹当内容哈希用。
        let db = Db::open_in_memory().unwrap();
        {
            let c = SqliteHashCache::new(&db, "blake3").unwrap();
            c.put(Path::new("/a.jpg"), 1, 1, "content-hash");
            c.flush().unwrap();
        }
        let c = SqliteHashCache::new(&db, "ahash8-128px-v1").unwrap();
        assert_eq!(c.get(Path::new("/a.jpg"), 1, 1), None);
    }

    #[test]
    fn recomputing_overwrites_the_stale_row() {
        // 主键是 (path, algo)：文件改过之后重算的哈希必须顶掉旧的，
        // 否则那一行永远命不中，缓存越攒越大却越来越没用。
        let db = Db::open_in_memory().unwrap();
        {
            let c = SqliteHashCache::new(&db, "blake3").unwrap();
            c.put(Path::new("/a.jpg"), 100, 7, "old");
            c.put(Path::new("/a.jpg"), 200, 9, "new");
            c.flush().unwrap();
        }
        let c = SqliteHashCache::new(&db, "blake3").unwrap();
        assert_eq!(c.get(Path::new("/a.jpg"), 200, 9).as_deref(), Some("new"));
        assert_eq!(c.get(Path::new("/a.jpg"), 100, 7), None);
    }
}
