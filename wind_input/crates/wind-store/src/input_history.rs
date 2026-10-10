//! 输入历史：按时间记录**实际落屏**的文本，供丢失后挑片段拼回。
//!
//! ## 要解决的问题
//!
//! 打完一段字、还没复制就发送失败，内容就没了。本模块把「刚打过什么」按时间落到本地库，
//! 再配合专用恢复入口（`wind-coordinator` 的 `ModeKind::InputHistory`）把片段取回来。
//!
//! ## 两级粒度（会话 + 片段），而不是一级
//!
//! 一条消息是**选了很多次词**才打完的，所以单看「一次上屏」只有一个个词。只存单次上屏的话，
//! 恢复一条长消息要点很多次、还容易漏。故分两级：
//!
//! | 层 | 一条 = 什么 | 回答的问题 |
//! |---|---|---|
//! | 会话 | 一次连续输入（到停顿/切窗口为止） | 「我上一段打的是什么」→ 整段取回 |
//! | 片段 | 会话内的每个落屏片段，保留先后顺序 | 「那一段里的某个词」→ 只取需要的 |
//!
//! ## 写事务按**会话**提交，不按按键
//!
//! 这是本模块最重要的性能约定。`wind-store/AGENTS.md` 明写 redb 读缓存只涨不落、
//! **绝不能进按键链路**。若每次上屏都开一个写事务，就是把一次按键变成一次 redb 写 +
//! 一次全表规模操作。故：调用方（coordinator）在内存里攒会话，**会话结束时才调一次**
//! [`Store::append_input_session`]——一次写事务同时落会话行与它全部片段行。
//!
//! ## 与 [`crate::completion`] 的分界
//!
//! `completion` 回答「这条文本用过几次」（LFU，key 由文本本身构成，同一文本反复用只累加计数）。
//! 本模块回答「**什么时候**打过什么」（时间序列，同一文本出现两次就是两条）。
//! 两者判据正交，故**不复用** `COMPLETION` 表：那张表的 key 里带文本，天然无法表达
//! 「同一文本的两个不同时刻」，而那正是恢复功能赖以工作的东西。
//!
//! 也因此**不按次数排序**：一条只打过一次的长消息在 LFU 排序里恒垫底（`completion::better`
//! 的首要判据是次数降序），而那恰是需要恢复的东西。本表一律**时间倒序**。
//!
//! ## 键编码
//!
//! ```text
//! 会话：  "{start_ms:020}\0{seq:010}"
//! 片段：  "{会话键}\0{ordinal:06}"
//! ```
//!
//! `start_ms` 零填充成 20 位定宽，使**字典序等于时间序**——redb 只有升序 range 扫描，
//! 定宽是「按时间范围取」成立的前提（同 `STATS_DAILY` 的 `YYYY-MM-DD` 用同一招）。
//! `seq` 是进程内单调计数器，只为消解「同一毫秒内开了两个会话」的碰撞；不靠它排序。
//!
//! ## 隐私
//!
//! 本表内容是**用户原始输入**。调用方与日志一律遵守根 `AGENTS.md`：INFO 级不得输出内容。
//! 本模块自身除错误路径外不打任何日志。

use crate::store::{INPUT_SEGMENTS, INPUT_SESSIONS, Store};
use redb::{ReadableTable, ReadableTableMetadata};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};

/// 一段会话的文本上限（字符数）。超出**静默丢弃整段**（同 `completion` 的取舍：
/// 调用点在上屏路径上，为一条学习数据报错没有任何可做的补救）。
///
/// 取值远大于 [`crate::completion::COMPLETION_TEXT_MAX_CHARS`]（512）：那里放的是网址，
/// 这里放的是一整段话。
pub const INPUT_SESSION_TEXT_MAX_CHARS: usize = 4096;

/// 单个片段的文本上限（字符数）。超出的片段不入库，但仍计入会话文本。
pub const INPUT_SEGMENT_TEXT_MAX_CHARS: usize = 512;

/// 会话层记录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputSessionRecord {
    /// 整段文本（各片段按序拼接）。
    pub text: String,
    /// 会话开始时间（Unix 毫秒）。
    pub start_ms: i64,
    /// 会话结束时间（Unix 毫秒）。
    pub end_ms: i64,
    /// 片段数。与 [`Store::list_input_segments`] 返回的条数应一致；
    /// 不一致只可能来自崩溃中断（片段行未落全），调用方须按实际取到的片段为准。
    pub segments: u32,
}

/// 片段层记录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputSegmentRecord {
    /// 该片段的文本。
    pub text: String,
    /// 该片段落屏时间（Unix 毫秒）。
    pub at_ms: i64,
}

/// 会话键的构造与解析。
///
/// 拼法只此一处：写端（[`Store::append_input_session`]）与读端（片段前缀扫描）必须同源，
/// 两处各拼一次迟早分叉，而分叉的症状是「会话在、片段取不出来」。
fn enc_session_key(start_ms: i64, seq: u64) -> String {
    format!("{:020}\0{:010}", start_ms.max(0), seq)
}

/// 片段键：会话键 + 序号。**必须带分隔符**——不带的话 `…10` 会命中 `…100` 的片段。
fn enc_segment_key(session_key: &str, ordinal: u32) -> String {
    format!("{}\0{:06}", session_key, ordinal)
}

/// 片段前缀（含分隔符）。
fn segment_prefix(session_key: &str) -> String {
    format!("{}\0", session_key)
}

/// 进程内单调序号，仅用于消解同毫秒碰撞。
static SESSION_SEQ: AtomicU64 = AtomicU64::new(0);

/// 当前 Unix 毫秒。
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

impl Store {
    /// 落一整段会话：会话行 + 它全部片段行，**同一个写事务**。
    ///
    /// `segments` 按落屏先后给出。返回会话键，供调用方（或测试）随后取回。
    ///
    /// 静默忽略的场合（返回 `Ok(None)`）：会话文本为空、或超出
    /// [`INPUT_SESSION_TEXT_MAX_CHARS`]。超长片段不入库，但会话文本照收。
    pub fn append_input_session(
        &self,
        start_ms: i64,
        end_ms: i64,
        text: &str,
        segments: &[InputSegmentRecord],
    ) -> anyhow::Result<Option<String>> {
        let n = text.chars().count();
        if n == 0 || n > INPUT_SESSION_TEXT_MAX_CHARS {
            return Ok(None);
        }
        let seq = SESSION_SEQ.fetch_add(1, Ordering::Relaxed);
        let key = enc_session_key(start_ms, seq);
        let kept: Vec<&InputSegmentRecord> = segments
            .iter()
            .filter(|s| {
                let c = s.text.chars().count();
                c > 0 && c <= INPUT_SEGMENT_TEXT_MAX_CHARS
            })
            .collect();
        let rec = InputSessionRecord {
            text: text.to_string(),
            start_ms,
            end_ms: end_ms.max(start_ms),
            segments: kept.len() as u32,
        };
        let rec_bytes = serde_json::to_vec(&rec)?;
        // 片段行先序列化好再进事务：事务里只做 insert，不留任何可能失败的中间步骤
        // （redb 的写事务持单写锁，里面做 JSON 序列化等于把锁按住干杂活）。
        let mut rows: Vec<(String, Vec<u8>)> = Vec::with_capacity(kept.len());
        for (i, s) in kept.iter().enumerate() {
            rows.push((enc_segment_key(&key, i as u32), serde_json::to_vec(*s)?));
        }
        self.with_db(|db| {
            let txn = db.begin_write()?;
            {
                let mut ts = txn.open_table(INPUT_SESSIONS)?;
                ts.insert(key.as_str(), rec_bytes.as_slice())?;
                let mut tg = txn.open_table(INPUT_SEGMENTS)?;
                for (k, v) in &rows {
                    tg.insert(k.as_str(), v.as_slice())?;
                }
            }
            txn.commit()?;
            Ok(())
        })?;
        Ok(Some(key))
    }

    /// 会话总数。
    pub fn count_input_sessions(&self) -> anyhow::Result<usize> {
        self.with_db(|db| {
            let txn = db.begin_read()?;
            Ok(txn.open_table(INPUT_SESSIONS)?.len()? as usize)
        })
    }

    /// 按**时间倒序**列出会话（最新在前），返回 `(本页, 总数)`。`limit = 0` 表示不限。
    ///
    /// 键的字典序即时间序，故取全表后反转即可；会话数由 `max_sessions` 封顶，
    /// 全量取回的代价有界（与 `completion::list_completions` 同一取舍）。
    pub fn list_input_sessions(
        &self,
        offset: usize,
        limit: usize,
    ) -> anyhow::Result<(Vec<(String, InputSessionRecord)>, usize)> {
        self.with_db(|db| {
            let txn = db.begin_read()?;
            let t = txn.open_table(INPUT_SESSIONS)?;
            let mut all: Vec<(String, InputSessionRecord)> = Vec::new();
            for item in t.iter()? {
                let (k, v) = item?;
                if let Ok(rec) = serde_json::from_slice::<InputSessionRecord>(v.value()) {
                    all.push((k.value().to_string(), rec));
                }
            }
            all.reverse(); // 键升序 = 时间升序 ⇒ 反转即时间倒序
            let total = all.len();
            let page: Vec<_> = all
                .into_iter()
                .skip(offset)
                .take(if limit == 0 { usize::MAX } else { limit })
                .collect();
            Ok((page, total))
        })
    }

    /// 取某会话的全部片段，**按落屏先后**（不是倒序——拼回时要顺着读）。
    pub fn list_input_segments(
        &self,
        session_key: &str,
    ) -> anyhow::Result<Vec<InputSegmentRecord>> {
        let scan = segment_prefix(session_key);
        self.with_db(|db| {
            let txn = db.begin_read()?;
            let t = txn.open_table(INPUT_SEGMENTS)?;
            let mut out = Vec::new();
            for item in t.range(scan.as_str()..)? {
                let (k, v) = item?;
                if !k.value().starts_with(&scan) {
                    break;
                }
                if let Ok(rec) = serde_json::from_slice::<InputSegmentRecord>(v.value()) {
                    out.push(rec);
                }
            }
            Ok(out)
        })
    }

    /// 删一条会话及其全部片段。返回它本来在不在。
    pub fn remove_input_session(&self, session_key: &str) -> anyhow::Result<bool> {
        let scan = segment_prefix(session_key);
        self.with_db(|db| {
            let txn = db.begin_write()?;
            let existed;
            {
                let mut ts = txn.open_table(INPUT_SESSIONS)?;
                existed = ts.remove(session_key)?.is_some();
                let mut tg = txn.open_table(INPUT_SEGMENTS)?;
                let keys: Vec<String> = tg
                    .range(scan.as_str()..)?
                    .filter_map(|item| item.ok())
                    .map(|(k, _)| k.value().to_string())
                    .take_while(|k| k.starts_with(&scan))
                    .collect();
                for k in keys {
                    tg.remove(k.as_str())?;
                }
            }
            txn.commit()?;
            Ok(existed)
        })
    }

    /// 清空全部输入历史（会话 + 片段两张表），返回删除的会话条数。
    pub fn clear_input_history(&self) -> anyhow::Result<usize> {
        self.with_db(|db| {
            let txn = db.begin_write()?;
            let removed;
            {
                // 先各自收齐键再删：redb 的 range/iter 借用表，边遍历边 remove 借不到两次。
                let mut ts = txn.open_table(INPUT_SESSIONS)?;
                let session_keys: Vec<String> = ts
                    .iter()?
                    .filter_map(|item| item.ok())
                    .map(|(k, _)| k.value().to_string())
                    .collect();
                let mut tg = txn.open_table(INPUT_SEGMENTS)?;
                let segment_keys: Vec<String> = tg
                    .iter()?
                    .filter_map(|item| item.ok())
                    .map(|(k, _)| k.value().to_string())
                    .collect();
                for k in &session_keys {
                    ts.remove(k.as_str())?;
                }
                for k in segment_keys {
                    tg.remove(k.as_str())?;
                }
                removed = session_keys.len();
            }
            txn.commit()?;
            Ok(removed)
        })
    }

    /// 裁剪到**最新的** `max` 条会话，返回删除条数。`max = 0` 表示不限（不裁剪）。
    ///
    /// 与 `completion::prune_completions` 的关键差异：那里按 LFU 排序取舍，且必须靠 `keep`
    /// 参数救回刚写入的那条（否则表一满就冻结）。**本表按时间取舍，没有这个结构性缺陷**——
    /// 刚写入的会话必然最新，天然落在保留集内，故不需要 `keep` 参数。
    pub fn prune_input_history(&self, max: usize) -> anyhow::Result<usize> {
        if max == 0 {
            return Ok(0);
        }
        let (newest, total) = self.list_input_sessions(0, 0)?;
        if total <= max {
            return Ok(0);
        }
        // `newest` 已时间倒序，保留前 `max` 条，其余全删。
        let doomed: Vec<String> = newest.into_iter().skip(max).map(|(k, _)| k).collect();
        let mut removed = 0usize;
        for key in doomed {
            if self.remove_input_session(&key)? {
                removed += 1;
            }
        }
        Ok(removed)
    }

    /// 便利方法：用「当前时刻」作为结束时间戳落一段会话。
    pub fn append_input_session_now(
        &self,
        start_ms: i64,
        text: &str,
        segments: &[InputSegmentRecord],
    ) -> anyhow::Result<Option<String>> {
        self.append_input_session(start_ms, now_ms(), text, segments)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (Store, std::path::PathBuf) {
        // 每个测试独立文件：redb 是单写者，共用文件会让并发测试互相阻塞
        let p = std::env::temp_dir().join(format!(
            "wind_input_history_test_{}_{:?}.redb",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&p);
        (Store::open(&p).unwrap(), p)
    }

    fn seg(text: &str, at: i64) -> InputSegmentRecord {
        InputSegmentRecord {
            text: text.to_string(),
            at_ms: at,
        }
    }

    #[test]
    fn append_then_read_back() {
        let (s, p) = store();
        let key = s
            .append_input_session(
                1000,
                2000,
                "你好世界",
                &[seg("你好", 1100), seg("世界", 1900)],
            )
            .unwrap()
            .expect("非空文本应入库");
        let (rows, total) = s.list_input_sessions(0, 0).unwrap();
        assert_eq!(total, 1);
        assert_eq!(rows[0].0, key);
        assert_eq!(rows[0].1.text, "你好世界");
        assert_eq!(rows[0].1.start_ms, 1000);
        assert_eq!(rows[0].1.end_ms, 2000);
        assert_eq!(rows[0].1.segments, 2);

        // 片段层：按落屏先后，不按时间倒序
        let segs = s.list_input_segments(&key).unwrap();
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].text, "你好");
        assert_eq!(segs[1].text, "世界");
        let _ = std::fs::remove_file(&p);
    }

    /// ★ 会话列表必须**时间倒序**（最新在前）——恢复功能靠的就是「上一条」。
    #[test]
    fn sessions_are_newest_first() {
        let (s, p) = store();
        let a = s
            .append_input_session(1000, 1100, "第一段", &[seg("第一段", 1000)])
            .unwrap()
            .unwrap();
        let b = s
            .append_input_session(5000, 5100, "第二段", &[seg("第二段", 5000)])
            .unwrap()
            .unwrap();
        let c = s
            .append_input_session(9000, 9100, "第三段", &[seg("第三段", 9000)])
            .unwrap()
            .unwrap();
        let (rows, total) = s.list_input_sessions(0, 0).unwrap();
        assert_eq!(total, 3);
        let order: Vec<&str> = rows.iter().map(|(_, r)| r.text.as_str()).collect();
        assert_eq!(
            order,
            vec!["第三段", "第二段", "第一段"],
            "最新的必须在最前"
        );
        assert_eq!(rows[0].0, c);
        assert_eq!(rows[2].0, a);
        assert_ne!(a, b);
        let _ = std::fs::remove_file(&p);
    }

    /// 同一毫秒开两个会话不得互相覆盖（`seq` 的存在理由）。
    #[test]
    fn same_millisecond_sessions_do_not_collide() {
        let (s, p) = store();
        let k1 = s
            .append_input_session(1000, 1000, "甲", &[seg("甲", 1000)])
            .unwrap()
            .unwrap();
        let k2 = s
            .append_input_session(1000, 1000, "乙", &[seg("乙", 1000)])
            .unwrap()
            .unwrap();
        assert_ne!(k1, k2);
        assert_eq!(s.count_input_sessions().unwrap(), 2);
        assert_eq!(s.list_input_segments(&k1).unwrap()[0].text, "甲");
        assert_eq!(s.list_input_segments(&k2).unwrap()[0].text, "乙");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn empty_text_is_ignored() {
        let (s, p) = store();
        assert!(s.append_input_session(1, 2, "", &[]).unwrap().is_none());
        assert_eq!(s.count_input_sessions().unwrap(), 0);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn over_long_session_text_is_ignored() {
        let (s, p) = store();
        let long = "字".repeat(INPUT_SESSION_TEXT_MAX_CHARS + 1);
        assert!(s.append_input_session(1, 2, &long, &[]).unwrap().is_none());
        let ok = "字".repeat(INPUT_SESSION_TEXT_MAX_CHARS);
        assert!(s.append_input_session(1, 2, &ok, &[]).unwrap().is_some());
        let _ = std::fs::remove_file(&p);
    }

    /// 超长**片段**不入库，但会话文本照收（两级各自设限）。
    #[test]
    fn over_long_segment_dropped_but_session_kept() {
        let (s, p) = store();
        let long_seg = "x".repeat(INPUT_SEGMENT_TEXT_MAX_CHARS + 1);
        let key = s
            .append_input_session(1, 2, "保留我", &[seg(&long_seg, 1), seg("短片段", 2)])
            .unwrap()
            .unwrap();
        let segs = s.list_input_segments(&key).unwrap();
        assert_eq!(segs.len(), 1, "超长片段被丢弃");
        assert_eq!(segs[0].text, "短片段");
        let (rows, _) = s.list_input_sessions(0, 0).unwrap();
        assert_eq!(rows[0].1.text, "保留我", "会话文本不受片段丢弃影响");
        assert_eq!(rows[0].1.segments, 1, "片段数与实际落库数一致");
        let _ = std::fs::remove_file(&p);
    }

    /// ★ 前缀扫描必须带分隔符：否则 `…\0{seq=1}` 的片段扫描会命中 `…\0{seq=10}`。
    #[test]
    fn segment_scan_is_not_confused_by_prefixing_session_keys() {
        let (s, p) = store();
        let keys: Vec<String> = (0..12)
            .map(|i| {
                s.append_input_session(
                    1000 + i,
                    1001 + i,
                    &format!("第{i}段"),
                    &[seg(&format!("片{i}"), 1000 + i)],
                )
                .unwrap()
                .unwrap()
            })
            .collect();
        for (i, k) in keys.iter().enumerate() {
            let segs = s.list_input_segments(k).unwrap();
            assert_eq!(
                segs.len(),
                1,
                "会话 {i} 只该有 1 个片段，实际 {}",
                segs.len()
            );
            assert_eq!(segs[0].text, format!("片{i}"), "取到了别的会话的片段");
        }
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn remove_session_also_removes_its_segments() {
        let (s, p) = store();
        let key = s
            .append_input_session(1, 2, "待删", &[seg("待", 1), seg("删", 2)])
            .unwrap()
            .unwrap();
        assert!(s.remove_input_session(&key).unwrap());
        assert!(!s.remove_input_session(&key).unwrap(), "删第二次报 false");
        assert_eq!(s.count_input_sessions().unwrap(), 0);
        assert!(
            s.list_input_segments(&key).unwrap().is_empty(),
            "孤儿片段必须一起走"
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn clear_removes_everything() {
        let (s, p) = store();
        let k1 = s
            .append_input_session(1, 2, "甲", &[seg("甲", 1)])
            .unwrap()
            .unwrap();
        let _k2 = s
            .append_input_session(3, 4, "乙", &[seg("乙", 3)])
            .unwrap()
            .unwrap();
        assert_eq!(s.clear_input_history().unwrap(), 2);
        assert_eq!(s.count_input_sessions().unwrap(), 0);
        assert!(s.list_input_segments(&k1).unwrap().is_empty());
        let _ = std::fs::remove_file(&p);
    }

    /// ★ 裁剪保留**最新**的 N 条，删最旧的。刚写入的那条天然在保留集内（无需 `keep` 参数）。
    #[test]
    fn prune_keeps_the_newest() {
        let (s, p) = store();
        for i in 0..5 {
            s.append_input_session(
                1000 * (i + 1),
                1000 * (i + 1) + 1,
                &format!("第{i}段"),
                &[seg(&format!("片{i}"), 1000 * (i + 1))],
            )
            .unwrap()
            .unwrap();
        }
        assert_eq!(s.prune_input_history(2).unwrap(), 3, "5 条裁到 2 条应删 3");
        let (rows, total) = s.list_input_sessions(0, 0).unwrap();
        assert_eq!(total, 2);
        let texts: Vec<&str> = rows.iter().map(|(_, r)| r.text.as_str()).collect();
        assert_eq!(texts, vec!["第4段", "第3段"], "留下的必须是最新两条");
        // 被裁会话的片段不得留下孤儿
        let all_seg_keys: Vec<String> = rows.iter().map(|(k, _)| k.clone()).collect();
        for k in all_seg_keys {
            assert_eq!(s.list_input_segments(&k).unwrap().len(), 1);
        }
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn prune_noop_when_under_limit() {
        let (s, p) = store();
        s.append_input_session(1, 2, "只有一条", &[])
            .unwrap()
            .unwrap();
        assert_eq!(s.prune_input_history(10).unwrap(), 0);
        assert_eq!(s.prune_input_history(0).unwrap(), 0, "0 = 不限，不裁剪");
        assert_eq!(s.count_input_sessions().unwrap(), 1);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn paging_respects_offset_and_limit() {
        let (s, p) = store();
        for i in 0..5 {
            s.append_input_session(1000 * (i + 1), 1000 * (i + 1) + 1, &format!("第{i}段"), &[])
                .unwrap()
                .unwrap();
        }
        let (page, total) = s.list_input_sessions(1, 2).unwrap();
        assert_eq!(total, 5);
        let texts: Vec<&str> = page.iter().map(|(_, r)| r.text.as_str()).collect();
        assert_eq!(
            texts,
            vec!["第3段", "第2段"],
            "倒序下 offset=1 应跳过最新的那条"
        );
        let _ = std::fs::remove_file(&p);
    }
}
